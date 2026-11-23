//! 客户端传输的单元测试：状态文件、`pair.ack` 校验、伪造 `PairDone` 的假 hub、分片与背压（假 hub + 假节点）。
//! 多节点长连接的集成测试在 `multi.rs` 的测试里，用真 hub 和真 daemon。

use std::time::Duration;

use anycon_crypto::{fingerprint, Handshake, Keypair, Role, Transport};
use anycon_protocol::envelope::frame;
use anycon_protocol::{
    Control, DeviceKind, Envelope, PairAck, PairRole, RpcMessage, RpcNotification, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::error::ErrorClass;
use super::link::Driver;
use super::pairing::pair_with;
use super::*;
use crate::config::Paths;

#[test]
fn code_arguments_accept_codes_and_links() {
    assert_eq!(parse_code_arg("7k3p-q2mx").unwrap(), "7k3p-q2mx");
    assert_eq!(parse_code_arg("  7K3PQ2MX ").unwrap(), "7K3PQ2MX");
    assert_eq!(parse_code_arg("https://hub.example.com/pair#code=7K3P-Q2MX").unwrap(), "7K3P-Q2MX");
    assert_eq!(parse_code_arg("https://hub.example.com/pair?code=7K3P-Q2MX&x=1").unwrap(), "7K3P-Q2MX");
    assert!(
        parse_code_arg("http://127.0.0.1:8080/pair#code=7K3P-Q2MX-ILO0-1111").is_err(),
        "I/L/O/0/1 are not in the alphabet"
    );
    let long = anycon_crypto::generate_pair_code_long();
    assert_eq!(parse_code_arg(&format!("https://h/pair#code={long}")).unwrap(), long);
    assert!(parse_code_arg("1").is_err());
    assert!(parse_code_arg("").is_err());
    assert!(parse_code_arg("https://hub.example.com/pair").is_err());
}

fn ack(node: &str, peer: &str, pubkey: &[u8; 32]) -> PairAck {
    PairAck {
        node_id: node.into(),
        peer_id: peer.into(),
        name: "n".into(),
        version: "0".into(),
        fingerprint: fingerprint(pubkey),
    }
}

#[test]
fn a_pair_ack_must_match_the_ids_and_the_handshake_key() {
    let key = [7u8; 32];
    verify_pair_ack(&ack("node", "me", &key), "me", "node", &key).unwrap();
    let wrong_peer = verify_pair_ack(&ack("node", "someone-else", &key), "me", "node", &key).unwrap_err();
    assert_eq!((wrong_peer.class, wrong_peer.code.as_str()), (ErrorClass::NotPaired, "pair_failed"));
    assert!(verify_pair_ack(&ack("other-node", "me", &key), "me", "node", &key).is_err());
    // 握手得到的公钥和 ack 里的指纹不是同一把：中间人换了钥匙
    assert!(verify_pair_ack(&ack("node", "me", &[8u8; 32]), "me", "node", &key).is_err());
}

#[test]
fn protocol_versions_must_be_adjacent() {
    assert!(check_protocol(PROTOCOL_VERSION).is_ok());
    assert!(check_protocol(PROTOCOL_VERSION + 1).is_ok());
    assert!(check_protocol(PROTOCOL_VERSION + 2).is_err());
    assert_eq!(check_protocol(PROTOCOL_VERSION + 5).unwrap_err().code, "protocol_mismatch");
}

#[test]
fn corrupt_state_files_are_reported_not_treated_as_missing() {
    let d = tempfile::tempdir().unwrap();
    // 没有文件：空状态，没有问题
    assert!(HubState::load(d.path(), "http://h").problem.is_none());
    std::fs::write(d.path().join("nodes.json"), "{ not json").unwrap();
    let st = HubState::load(d.path(), "http://h");
    assert!(st.problem.as_deref().unwrap().contains("nodes.json"));
    assert!(st.check().is_err() && st.save().is_err(), "a corrupt state is never overwritten");
    std::fs::remove_file(d.path().join("nodes.json")).unwrap();
    std::fs::write(d.path().join("client.key"), "zz").unwrap();
    let st = HubState::load(d.path(), "http://h");
    assert!(st.problem.as_deref().unwrap().contains("client.key"));
    assert_eq!(std::fs::read_to_string(d.path().join("client.key")).unwrap(), "zz");
}

#[test]
fn forgetting_credentials_removes_the_files_but_keeps_the_key_and_nodes() {
    let d = tempfile::tempdir().unwrap();
    let mut st = HubState::load(d.path(), "http://h");
    st.secret = Some([1u8; 32]);
    st.token = Some("tok".into());
    st.device_id = Some("dev".into());
    st.nodes.push(NodeEntry { id: "n".into(), name: "n".into(), pubkey: "00".into(), paired_at_ms: 1 });
    st.save().unwrap();
    st.forget_credentials().unwrap();
    let again = HubState::load(d.path(), "http://h");
    assert!(again.token.is_none() && again.device_id.is_none());
    assert_eq!(again.secret, Some([1u8; 32]));
    assert_eq!(again.nodes.len(), 1);
    st.forget_credentials().unwrap(); // 幂等
}

// ------------------------------------------------------------ 恶意 hub：伪造 PairDone

/// 一个假 hub：配合 joiner 走完撮合，用**错误的 PSK** 当 responder（hub 其实不知道短码），然后伪造 `PairDone`。
/// `junk_ack` 为真时再发一条解密不过的「pair.ack」；为假时什么都不发。
async fn evil_hub(junk_ack: bool) -> (String, tokio::task::JoinHandle<()>) {
    use anycon_protocol::Kind as K;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let me = "joiner-temp".to_string();
        let mut seq = 0u64;
        let hub_kp = Keypair::generate();
        let mut hs = Handshake::pairing(Role::Responder, &hub_kp, &[9u8; 32]).unwrap();
        macro_rules! send_ctl {
            ($c:expr) => {{
                seq += 1;
                let b = Envelope::control("", seq, &$c).unwrap().encode().unwrap();
                ws.send(Message::Binary(b.into())).await.unwrap();
            }};
        }
        while let Some(Ok(Message::Binary(b))) = ws.next().await {
            let env = Envelope::decode(&b).unwrap();
            match env.kind {
                K::Control => match env.control_body().unwrap() {
                    Control::Auth { .. } => send_ctl!(Control::AuthOk {
                        device_id: me.clone(),
                        server_time_ms: 0,
                        peers_online: vec![],
                        quota: None
                    }),
                    Control::PairJoin { .. } => {
                        send_ctl!(Control::PairMatched { peer: "host-temp".into(), role: PairRole::Joiner })
                    }
                    Control::PairConfirm { .. } => {
                        send_ctl!(Control::PairDone {
                            peer: "forged-node".into(),
                            device_id: Some("new-device-id".into()),
                            device_token: Some("forged-token".into())
                        });
                        if junk_ack {
                            let mut body = vec![frame::WHOLE];
                            body.extend_from_slice(&[3u8; 80]);
                            seq += 1;
                            let b = Envelope::new("forged-node", "new-device-id", K::Data, seq, body).encode().unwrap();
                            ws.send(Message::Binary(b.into())).await.unwrap();
                        }
                    }
                    _ => {}
                },
                K::Handshake => {
                    let read_ok = hs.read(&env.body).is_ok();
                    if read_ok && !hs.is_finished() && hs.is_my_turn() {
                        let m2 = hs.write(br#"{"name":"EVIL","version":"0"}"#).unwrap();
                        seq += 1;
                        let b = Envelope::new("host-temp", me.clone(), K::Handshake, seq, m2).encode().unwrap();
                        ws.send(Message::Binary(b.into())).await.unwrap();
                    }
                }
                _ => {}
            }
        }
    });
    (url, h)
}

#[tokio::test]
async fn a_forged_pair_done_without_a_valid_pair_ack_pairs_nothing_and_saves_nothing() {
    for junk in [false, true] {
        let (url, hub) = evil_hub(junk).await;
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let code = anycon_crypto::generate_pair_code();
        let t = std::time::Instant::now();
        let e = pair_with(&paths, &url, &code, client_info("cli-test", DeviceKind::Cli), Duration::from_millis(800))
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::NotPaired, "junk={junk}: {}", e.message);
        assert_eq!(e.code, "pair_failed", "junk={junk}: {}", e.message);
        let want = if junk { "could not be decrypted" } else { "did not confirm" };
        assert!(e.message.contains(want), "junk={junk}: {}", e.message);
        assert!(t.elapsed() < Duration::from_secs(8));
        // 什么都没落盘：没有节点、没有 token、没有设备 id、没有 hub 发的假身份
        let dir = paths.client_dir(&host_of(&url).unwrap());
        for f in ["nodes.json", "device-token", "device-id"] {
            assert!(!dir.join(f).exists(), "junk={junk}: {f} must not be written");
        }
        hub.abort();
    }
}

// ------------------------------------------------------------ 分片与背压

#[tokio::test]
async fn a_big_request_is_fragmented_and_a_dropped_fragment_is_resent_with_the_same_bytes() {
    use anycon_protocol::Kind as K;
    // 两个已握手的 Transport：(服务端, 客户端)
    let (srv_kp, cli_kp) = (Keypair::generate(), Keypair::generate());
    let mut ini = Handshake::reconnect(Role::Initiator, &cli_kp, Some(&srv_kp.public)).unwrap();
    let mut res = Handshake::reconnect(Role::Responder, &srv_kp, Some(&cli_kp.public)).unwrap();
    let m1 = ini.write(&[]).unwrap();
    res.read(&m1).unwrap();
    let m2 = res.write(&[]).unwrap();
    ini.read(&m2).unwrap();
    let (mut srv_t, cli_t) = (res.into_transport().unwrap(), ini.into_transport().unwrap());

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut reasm = frame::Reassembler::new();
        let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut dropped: Option<(u64, Vec<u8>, std::time::Instant)> = None;
        let mut seq = 0u64;
        while let Some(Ok(Message::Binary(b))) = ws.next().await {
            let env = Envelope::decode(&b).unwrap();
            assert_eq!(env.kind, K::Data);
            let is_last =
                matches!(frame::decode(&env.body).unwrap(), frame::DataFrame::Fragment { index: 1, count: 2, .. });
            if is_last && dropped.is_none() {
                // 假装 hub 丢了第二片，并发背压回执
                dropped = Some((env.seq, env.body.clone(), std::time::Instant::now()));
                seq += 1;
                let c = Control::Backpressure { to: "node-1".into(), seq: env.seq };
                let b = Envelope::control("", seq, &c).unwrap().encode().unwrap();
                ws.send(Message::Binary(b.into())).await.unwrap();
                continue;
            }
            if is_last {
                let (s0, body0, at) = dropped.take().unwrap();
                assert_eq!((env.seq, &env.body), (s0, &body0), "resent with the original seq and the same bytes");
                assert!(at.elapsed() >= Duration::from_millis(90), "it paused (100-300 ms) before resending");
                dropped = Some((0, vec![], at));
            }
            got.push((env.seq, env.body.clone()));
            if let Some(ct) = reasm.push(frame::decode(&env.body).unwrap()).unwrap() {
                let plain = srv_t.decrypt(&ct).unwrap();
                let RpcMessage::Request(req) = RpcMessage::decode(&plain).unwrap() else { panic!("request expected") };
                assert_eq!(req.params["blob"].as_str().unwrap().len(), 300_000);
                let resp = RpcMessage::result(req.id, serde_json::json!({ "ok": true })).encode();
                let ct = srv_t.encrypt(&resp).unwrap();
                seq += 1;
                let b =
                    Envelope::new("node-1", "dev-1", K::Data, seq, frame::encode(&ct, 1).remove(0)).encode().unwrap();
                ws.send(Message::Binary(b.into())).await.unwrap();
            }
        }
        got.len()
    });
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let run = tokio::spawn(driver.run(req_rx, note_tx));
    let (reply, rx) = oneshot::channel();
    req_tx
        .send(Req { method: "x.y".into(), params: serde_json::json!({ "blob": "b".repeat(300_000) }), reply })
        .unwrap();
    let v = tokio::time::timeout(Duration::from_secs(10), rx).await.expect("answered").unwrap().unwrap();
    assert_eq!(v, serde_json::json!({ "ok": true }));
    drop(req_tx);
    let reason = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
    assert_eq!(reason, "closed locally");
    server.abort();
}

/// 两个已握手的 Transport：(服务端, 客户端)。
fn transport_pair() -> (Transport, Transport) {
    let (srv_kp, cli_kp) = (Keypair::generate(), Keypair::generate());
    let mut ini = Handshake::reconnect(Role::Initiator, &cli_kp, Some(&srv_kp.public)).unwrap();
    let mut res = Handshake::reconnect(Role::Responder, &srv_kp, Some(&cli_kp.public)).unwrap();
    let m1 = ini.write(&[]).unwrap();
    res.read(&m1).unwrap();
    let m2 = res.write(&[]).unwrap();
    ini.read(&m2).unwrap();
    (res.into_transport().unwrap(), ini.into_transport().unwrap())
}

/// 起一个假 hub + 假 daemon 在同一个 WebSocket 连接的另一端：对 `data` 信封走 `StickyTarget`（容量 `cap`，每 10 ms 被读走
/// `read_per_tick`），被退回的回 `backpressure`，被读走的按序重组 + 解密，每条请求回一个 `{ "len": <blob 长度> }`。
/// `rate_limit_first`：第一次退回时再发一条 `rate_limited { retry_after_ms }`。返回 (ws url, 任务句柄；结果是看到的 blob 长度和退回次数)。
async fn sticky_node(
    cap: usize,
    read_per_tick: usize,
    rate_limit_first: Option<u64>,
    mut srv_t: Transport,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<(Vec<usize>, usize)>) {
    use crate::testutil::{Offer, StickyTarget};
    use anycon_protocol::Kind as K;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let h = tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut target = StickyTarget::new(cap);
        let mut reasm = frame::Reassembler::new();
        let mut seen = Vec::new();
        let mut ctl_seq = 0u64;
        let mut told_rate = false;
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                m = ws.next() => {
                    let Some(Ok(Message::Binary(b))) = m else { break };
                    let env = Envelope::decode(&b).unwrap();
                    assert_eq!(env.kind, K::Data);
                    if target.offer(env.seq, b.to_vec()) == Offer::Bounced {
                        ctl_seq += 1;
                        let c = Control::Backpressure { to: "node-1".into(), seq: env.seq };
                        ws.send(Message::Binary(Envelope::control("", ctl_seq, &c).unwrap().encode().unwrap().into())).await.unwrap();
                        if let (Some(ms), false) = (rate_limit_first, told_rate) {
                            told_rate = true;
                            ctl_seq += 1;
                            let c = Control::RateLimited { retry_after_ms: ms, to: None };
                            ws.send(Message::Binary(Envelope::control("", ctl_seq, &c).unwrap().encode().unwrap().into())).await.unwrap();
                        }
                    }
                }
                // 第一次退回之前目标不读：机器忙的时候线程可能在两个分片之间被调度走一阵，目标趁机把缓冲读空，
                // 第二片就不会被退回，用例要测的场景（和 `told_rate`）根本没发生
                _ = tick.tick(), if target.bounces > 0 => {
                    for (_, f) in target.read(read_per_tick) {
                        let env = Envelope::decode(&f).unwrap();
                        let Some(ct) = reasm.push(frame::decode(&env.body).unwrap()).expect("fragments arrive in order") else { continue };
                        let plain = srv_t.decrypt(&ct).expect("the secure channel stays in sync");
                        let RpcMessage::Request(req) = RpcMessage::decode(&plain).unwrap() else { panic!("request expected") };
                        let len = req.params["blob"].as_str().unwrap().len();
                        seen.push(len);
                        let resp = RpcMessage::result(req.id, serde_json::json!({ "len": len })).encode();
                        let ct = srv_t.encrypt(&resp).unwrap();
                        ctl_seq += 1;
                        let b = Envelope::new("node-1", "dev-1", K::Data, ctl_seq, frame::encode(&ct, ctl_seq as u32).remove(0)).encode().unwrap();
                        ws.send(Message::Binary(b.into())).await.unwrap();
                    }
                }
            }
        }
        (seen, target.bounces)
    });
    (addr, h)
}

async fn run_driver(
    addr: std::net::SocketAddr,
    cli_t: Transport,
    blobs: Vec<usize>,
) -> (Vec<serde_json::Value>, Duration, String) {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let run = tokio::spawn(driver.run(req_rx, note_tx));
    let t = std::time::Instant::now();
    let mut replies = Vec::new();
    for len in &blobs {
        let (reply, rx) = oneshot::channel();
        req_tx
            .send(Req { method: "x.y".into(), params: serde_json::json!({ "blob": "b".repeat(*len) }), reply })
            .unwrap();
        replies.push(rx);
    }
    let mut out = Vec::new();
    for rx in replies {
        out.push(tokio::time::timeout(Duration::from_secs(30), rx).await.expect("answered").unwrap().unwrap());
    }
    let took = t.elapsed();
    drop(req_tx);
    let reason = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
    (out, took, reason)
}

#[tokio::test]
async fn requests_cross_a_sticky_hub_with_a_tiny_buffer_in_order_and_intact() {
    let (srv_t, cli_t) = transport_pair();
    // 目标缓冲 300 KB、读得慢：每个 1 MB 的请求（5 片）都会被退回好几次
    let (addr, server) = sticky_node(300_000, 100_000, None, srv_t).await;
    let blobs = vec![1_000_000, 10, 1_200_000, 700_000];
    let (replies, _took, reason) = run_driver(addr, cli_t, blobs.clone()).await;
    assert_eq!(reason, "closed locally");
    let lens: Vec<usize> = replies.iter().map(|r| r["len"].as_u64().unwrap() as usize).collect();
    assert_eq!(lens, blobs, "every request arrives intact and answers come back in order");
    let (seen, bounces) = tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    assert_eq!(seen, blobs);
    assert!(bounces > 0, "the scenario really bounced envelopes");
}

#[tokio::test]
async fn rate_limited_pauses_the_cli_for_retry_after_ms() {
    let (srv_t, cli_t) = transport_pair();
    // 缓冲只有 150 KB：第一个请求的第二片就被退回，同时 hub 说停 700 ms
    let (addr, server) = sticky_node(150_000, 1_000_000, Some(700), srv_t).await;
    let (replies, took, reason) = run_driver(addr, cli_t, vec![500_000]).await;
    assert_eq!(reason, "closed locally");
    assert_eq!(replies[0]["len"], 500_000);
    assert!(took >= Duration::from_millis(690), "nothing was resent before retry_after_ms: {took:?}");
    let (_, bounces) = tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    assert!(bounces > 0);
}

#[tokio::test]
async fn a_bounced_seq_that_is_not_retained_ends_the_connection_instead_of_desyncing() {
    // 假 hub 对一个我们从没发过的 seq 回 backpressure
    let (_srv_t, cli_t) = transport_pair();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let c = Control::Backpressure { to: "node-1".into(), seq: 999 };
        ws.send(Message::Binary(Envelope::control("", 1, &c).unwrap().encode().unwrap().into())).await.unwrap();
        while ws.next().await.is_some() {}
    });
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
    let (_req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let reason = tokio::time::timeout(Duration::from_secs(5), driver.run(req_rx, note_tx)).await.unwrap();
    assert!(reason.contains("lost sync"), "{reason}");
    server.abort();
}
