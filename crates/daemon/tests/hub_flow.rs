//! daemon ↔ 真实 hub（进程内）↔ 假 web 客户端：首次连接、配对（Noise XXpsk3）、设备 token、
//! 重连握手（IK）、加密 RPC、会话事件流、吊销、公钥校验、重连与背压。

use std::time::Duration;

use anycon_crypto::{derive_pair_secrets, Handshake, Keypair, Role};
use anycon_daemon::testutil::{eventually, FakeWeb, TestDaemon};
use anycon_hub::Config as HubConfig;
use anycon_protocol::*;
use serde_json::{json, Value};

mod common;
use common::Schemas;

const T: Duration = Duration::from_secs(10);

struct TestHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
    http: reqwest::Client,
}

impl TestHub {
    async fn start() -> TestHub {
        Self::start_with(|_| {}).await
    }

    async fn start_with(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = HubConfig::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        tweak(&mut c);
        let handle = anycon_hub::run(c).await.expect("hub starts");
        TestHub { handle, _dir: dir, http: reqwest::Client::new() }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }

    /// 管理员登录，返回 cookie（web 客户端配对需要账号归属）。
    async fn admin_cookie(&self) -> String {
        let token = self.handle.generated_admin_token.clone().unwrap();
        let res = self
            .http
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let c = res.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        c.split(';').next().unwrap().to_string()
    }
}

async fn wait_daemon_connected(td: &TestDaemon) {
    assert!(
        eventually(T, || async { td.ctx.hub_state.get().connected }).await,
        "daemon should connect to the hub: {:?}",
        td.ctx.hub_state.get()
    );
}

/// 完整配对：返回已建立 IK 会话的假 web。
async fn pair_web(td: &TestDaemon, hub: &TestHub, name: &str) -> FakeWeb {
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    // 响应按 schema 校验（`node.pair_begin` 要有 hub 才能调，registry_schema.rs 的遍历测试跳过了它）
    let raw: Value = local.call(td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    Schemas::load().check("NodePairBeginResult", &raw, "node.pair_begin");
    let begin: NodePairBeginResult = serde_json::from_value(raw).unwrap();
    assert_eq!(begin.code.len(), 19, "the default is 16 characters: XXXX-XXXX-XXXX-XXXX");
    // 短码在 URL fragment 里（浏览器不会把 fragment 发给 hub），不在 query 里
    assert_eq!(begin.link, format!("{}/pair#code={}", hub.url(), begin.code));
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), name, Keypair::generate()).await;
    web.pair(&begin.code).await;
    web
}

#[tokio::test]
async fn first_pairing_issues_a_device_token_and_then_ik_rpc_works() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    // 首次连接：没有 token，用临时身份连上
    let st = td.ctx.hub_state.get();
    assert!(!st.authenticated_with_token);
    assert!(td.ctx.identity.device_token().is_none());
    let mut local = td.client("cli");
    let e = local.call::<NodeInfo>(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(e.node_id.starts_with("unpaired-"));

    let mut web = pair_web(&td, &hub, "Test Browser").await;

    // 配对完成：daemon 拿到了正式 device_id 和 token，落盘 0600，同一连接换了身份
    assert!(
        eventually(T, || async { td.ctx.identity.device_token().is_some() }).await,
        "daemon should receive a device token from PairDone"
    );
    let token = td.ctx.identity.device_token().unwrap();
    assert_eq!(std::fs::read_to_string(td.paths.device_token()).unwrap().trim(), token);
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(td.paths.device_token()).unwrap().permissions())
            & 0o777;
    assert_eq!(mode, 0o600);
    let node_id = web.node_id.clone().unwrap();
    assert_eq!(
        td.ctx.identity.device_id().as_deref(),
        Some(node_id.as_str()),
        "the host's permanent id is what the joiner sees as the node id"
    );
    assert!(td.ctx.hub_state.get().connected);

    // 登记：peers.json + 库，键是 joiner 的正式 id，名字来自第三条握手消息的 payload
    assert!(
        eventually(T, || async { td.ctx.peers.record(&web.device_id).is_some() }).await,
        "peer is re-keyed to the permanent id"
    );
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert_eq!(peers_json["peers"][0]["device_id"], json!(web.device_id));
    assert_eq!(peers_json["peers"][0]["name"], json!("Test Browser"));
    assert_eq!(peers_json["peers"][0]["pubkey"], json!(hex::encode(web.kp.public)));
    let rows = td.ctx.store.list_peers().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].device_id.as_str(), rows[0].name.as_str()), (web.device_id.as_str(), "Test Browser"));

    // hub 在配对后给双方发 peer_online：web 随即做 IK 握手并调用 RPC
    web.ik().await.expect("IK handshake");
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.node_id, node_id);
    assert_eq!(info.name, "test-node");
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    assert_eq!((devices.devices[0].name.as_str(), devices.devices[0].online), ("Test Browser", true));
    assert_eq!(devices.devices[0].fingerprint, anycon_crypto::fingerprint(&web.kp.public));
    let agents: AgentListResult = web.call(methods::AGENT_LIST, json!({})).await.unwrap();
    assert_eq!(agents.agents[0].id, "fake");

    // 会话：create + watch + 事件流经加密通道，同时 session.changed 到达
    let s: SessionSummary = web
        .call(methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "run a permission", "message_id": "web-1" }))
        .await
        .unwrap();
    let _: Empty = web.call(methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();
    let req: PermissionRequest =
        serde_json::from_value(web.next_note(|n| n.method == methods::PERMISSION_REQUEST).await.params).unwrap();
    let _: Empty = web
        .call(methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    loop {
        let n = web.next_note(|n| n.method == methods::SESSION_EVENT).await;
        let ev: SessionEvent = serde_json::from_value(n.params).unwrap();
        if matches!(ev.event, Event::TurnDone { .. }) {
            break;
        }
    }
    // 审计里能看到是哪台设备做的
    let audit: NodeAuditResult = web.call(methods::NODE_AUDIT, json!({ "limit": 20 })).await.unwrap();
    let respond = audit.entries.iter().find(|e| e.action == "permission.respond").unwrap();
    assert_eq!((respond.device_id.as_str(), respond.device_name.as_str()), (web.device_id.as_str(), "Test Browser"));
    // 本机管理方法不能经 hub 调用
    let e = web.call::<Value>("local.status", json!({})).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));

    td.shutdown().await;
}

#[tokio::test]
async fn reconnect_with_token_rehandshakes_and_revocation_closes_the_door() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Phone").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();

    // web 断线重连（带 token）：hub 发 peer_online -> daemon 丢旧会话 -> web 新 IK
    let (token, kp_secret, node_id, node_pub, name) =
        (web.token.clone().unwrap(), web.kp.secret, web.node_id.clone(), web.node_pub, web.name.clone());
    drop(web);
    let mut web = FakeWeb::connect(&hub.url(), Some(token.clone()), None, &name, Keypair::from_secret(kp_secret)).await;
    web.node_id = node_id.clone();
    web.node_pub = node_pub;
    assert!(web.online.contains(node_id.as_ref().unwrap()), "daemon is online for the reconnecting client");
    web.ik().await.expect("re-handshake after reconnect");
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.node_id, node_id.clone().unwrap());

    // 公钥不符：另一把密钥冒用同一设备 id 握手 -> 被忽略（不回握手）
    {
        let mut imposter = FakeWeb::connect(&hub.url(), Some(token.clone()), None, &name, Keypair::generate()).await;
        imposter.node_id = node_id.clone();
        imposter.node_pub = node_pub;
        let r = imposter.ik().await;
        assert!(r.is_err(), "daemon must not complete a handshake for an unregistered static key");
    }
    // 原客户端的会话不受影响（impostor 连上会顶掉 web 的连接，所以这里重新连接再握手）
    let mut web = FakeWeb::connect(&hub.url(), Some(token.clone()), None, &name, Keypair::from_secret(kp_secret)).await;
    web.node_id = node_id.clone();
    web.node_pub = node_pub;
    web.ik().await.unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();

    // daemon 重启（同 home，带 token）：用 token 认证连接，peers 还在，IK 照常
    let did = td.ctx.identity.device_id().unwrap();
    drop(web);
    let mut td = td;
    td.restart().await;
    wait_daemon_connected(&td).await;
    assert!(td.ctx.hub_state.get().authenticated_with_token);
    assert_eq!(
        td.ctx.hub_state.get().device_id.as_deref(),
        Some(did.as_str()),
        "same permanent identity after restart"
    );
    let mut web = FakeWeb::connect(&hub.url(), Some(token.clone()), None, &name, Keypair::from_secret(kp_secret)).await;
    web.node_id = node_id.clone();
    web.node_pub = node_pub;
    web.ik().await.expect("peers survive a daemon restart");
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);

    // 吊销：daemon 删公钥并通知 hub；之后握手被拒
    let mut local = td.client("cli");
    let revoked: Value =
        local.call(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": web.device_id })).await.unwrap();
    Schemas::load().check("Empty", &revoked, "node.revoke_device");
    assert!(td.ctx.store.list_peers().await.unwrap().is_empty());
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty());
    let e = local.call::<Empty>(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": "nope" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    // 吊销是持久的：重启 daemon 之后旧公钥仍然被拒（peers.json 和数据库都已经删了），握手得不到回应
    let mut td = td;
    td.restart().await;
    wait_daemon_connected(&td).await;
    assert!(td.ctx.peers.list().is_empty(), "the revoked device did not come back");
    assert!(td.ctx.store.list_peers().await.unwrap().is_empty());
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty(), "peers.json would otherwise re-import it");
    td.shutdown().await;
}

#[tokio::test]
async fn pairing_with_a_wrong_code_fails_and_pair_begin_needs_the_hub() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    // 用错的 PSK（比如手输错了短码里的一位但碰巧也是合法短码？这里直接用另一个码派生）：握手第三条失败，不登记
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Evil", Keypair::generate()).await;
    // hub 只认 match_id（由真正的短码算出）；之后用错的 PSK 握手
    web.send_control(Control::PairJoin { match_id: derive_pair_secrets(&begin.code).unwrap().match_id.clone() }).await;
    let host_temp = match web.wait_control(|c| matches!(c, Control::PairMatched { .. })).await {
        Control::PairMatched { peer, .. } => peer,
        _ => unreachable!(),
    };
    let wrong = derive_pair_secrets("ZZZZ-ZZZZ").unwrap().psk;
    let mut hs = Handshake::pairing(Role::Initiator, &web.kp, &wrong).unwrap();
    let m1 = hs.write(&[]).unwrap();
    web.send_env(&host_temp, Kind::Handshake, m1).await;
    let env = loop {
        let e = web.recv_env(T).await.unwrap();
        if e.kind == Kind::Handshake {
            break e;
        }
    };
    hs.read(&env.body).unwrap();
    let m3 = hs.write(b"{}").unwrap();
    web.send_env(&host_temp, Kind::Handshake, m3).await;
    web.send_control(Control::PairConfirm { peer: host_temp }).await;
    // daemon 拒绝第三条 -> 不发 confirm -> hub 超时前我们收不到 PairDone；daemon 不登记设备
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(td.ctx.store.list_peers().await.unwrap().is_empty());
    assert!(td.ctx.identity.device_token().is_none());
    td.shutdown().await;

    // 没有 hub 连接时 pair_begin 报错而不是挂起
    let td = TestDaemon::start_with(&[], true).await;
    let mut local = td.client("cli");
    let e = local.call::<NodePairBeginResult>(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Unsupported));
    td.shutdown().await;
}

#[tokio::test]
async fn hub_restart_makes_the_daemon_reconnect_and_peers_rehandshake() {
    let hub = TestHub::start().await;
    let addr = hub.handle.addr;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Laptop").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    // hub 优雅关停 -> daemon 掉线并重连（同一端口重新起一个 hub，沿用数据目录）
    let (token, secret, node_id, node_pub) =
        (web.token.clone().unwrap(), web.kp.secret, web.node_id.clone(), web.node_pub);
    drop(web);
    let dir = hub._dir;
    let h = hub.handle;
    h.shutdown().await;
    assert!(eventually(T, || async { !td.ctx.hub_state.get().connected }).await, "daemon notices the hub going away");
    let mut c = HubConfig::new("http://127.0.0.1", dir.path()).unwrap();
    c.listen = addr;
    c.admin_token = Some("x".repeat(32));
    let hub2 = anycon_hub::run(c).await.expect("hub restarts on the same port");
    assert!(
        eventually(Duration::from_secs(20), || async { td.ctx.hub_state.get().connected }).await,
        "daemon reconnects with backoff: {:?}",
        td.ctx.hub_state.get()
    );
    assert!(td.ctx.hub_state.get().authenticated_with_token);
    let hub_view = TestHub { handle: hub2, _dir: dir, http: reqwest::Client::new() };
    let mut web = FakeWeb::connect(&hub_view.url(), Some(token), None, "Laptop", Keypair::from_secret(secret)).await;
    web.node_id = node_id;
    web.node_pub = node_pub;
    web.ik().await.expect("fresh IK after the hub restarted");
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// 分片、重置、临时连接额度

/// 小一点的信封上限：一片（≤ 200 KiB + 头）刚好放得下，更大的整条消息必须分片。
async fn paired_web_over_a_small_envelope_hub() -> (TestHub, TestDaemon, FakeWeb) {
    let hub = TestHub::start_with(|c| c.max_envelope_bytes = 220 * 1024).await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Big Browser").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    (hub, td, web)
}

#[tokio::test]
async fn large_messages_cross_the_peer_transport_in_fragments() {
    let (_hub, td, mut web) = paired_web_over_a_small_envelope_hub().await;

    // 1) 150 KB 的 text_delta 事件（一个信封放得下，但很接近上限）
    let s: SessionSummary = web.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let _: Empty = web.call(methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();
    let _: SessionSendResult = web
        .call(methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m-bigtext", "content": "bigtext" }))
        .await
        .unwrap();
    let mut big_seen = false;
    loop {
        let n = web.next_note(|n| n.method == methods::SESSION_EVENT).await;
        let ev: SessionEvent = serde_json::from_value(n.params).unwrap();
        match ev.event {
            Event::TextDelta { text } if text.len() == 150_000 => {
                assert!(text.bytes().all(|b| b == b'T'), "intact");
                big_seen = true;
            }
            Event::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(big_seen, "the 150 KB text_delta arrived intact");

    // 2) 客户端 -> daemon：600 KB 的用户消息（3 片），daemon 重组后照常处理
    let content = "u".repeat(600_000);
    let r: SessionSendResult = web
        .call(methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m-600k", "content": content }))
        .await
        .unwrap();
    assert!(!r.duplicate);
    assert!(
        eventually(T, || async { td.ctx.sessions.get_summary(&s.id).await.unwrap().status == SessionStatus::Idle })
            .await
    );

    // 3) daemon -> 客户端：一页 ~1 MB 的 history（单个 600 KB 事件单独成页，加上别的事件）；5+ 片
    let before = web.fragments_seen;
    let mut total = 0usize;
    let mut after = 0u64;
    let mut found = false;
    loop {
        let h: SessionHistoryResult = web
            .call(methods::SESSION_HISTORY, json!({ "session_id": s.id, "after_seq": after, "limit": 1000 }))
            .await
            .unwrap();
        total += serde_json::to_vec(&h).unwrap().len();
        for ev in &h.events {
            after = ev.seq;
            if let Event::UserMessage { content, .. } = &ev.event {
                if content.len() == 600_000 {
                    assert!(content.bytes().all(|b| b == b'u'));
                    found = true;
                }
            }
        }
        if !h.has_more {
            break;
        }
    }
    assert!(found && total > 600_000, "the whole history came back ({total} bytes)");
    assert!(web.fragments_seen > before, "the big page was fragmented");

    // 4) 近 1 MiB 的文件读取：一条 ~1 MB 的响应（6 片）
    std::fs::write(td.workspace.join("big.txt"), "z".repeat(1_000_000)).unwrap();
    let r: FsReadResult =
        web.call(methods::FS_READ, json!({ "path": "big.txt", "max_bytes": 1_048_576 })).await.unwrap();
    assert_eq!(r.text.len(), 1_000_000);
    assert!(web.fragments_seen >= before + 5);

    // 传输仍然同步：之后的小请求照常
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    td.shutdown().await;
}

/// 附件（ADR-35）经 hub + Noise：512 KiB 的块（base64 后约 700 KB）要分片发过来、重组；`blob.get` 的响应同样分片回去。
/// 客户端会同时放 4 块在途——这里一次发出全部 3 块、乱序，再收三个响应。
#[tokio::test]
async fn attachment_chunks_and_downloads_cross_the_peer_transport_in_fragments() {
    use base64::Engine as _;
    let (_hub, td, mut web) = paired_web_over_a_small_envelope_hub().await;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let chunk = anycon_daemon::blobs::CHUNK_SIZE as usize;
    let bytes = anycon_daemon::testutil::png_fixture(1920, 1080, chunk * 2 + 5000, 9);
    let begin: BlobPutBeginResult = web
        .call(
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "big.png", "mime": "image/png", "size": bytes.len(),
                    "sha256": anycon_daemon::testutil::sha256_hex(&bytes) }),
        )
        .await
        .unwrap();
    assert_eq!(begin.chunk_size as usize, chunk);
    // 全部在途、乱序：2, 0, 1
    let before = web.fragments_seen;
    let parts: Vec<&[u8]> = bytes.chunks(chunk).collect();
    let mut ids = vec![];
    for i in [2usize, 0, 1] {
        ids.push(
            web.send_rpc(
                methods::BLOB_PUT_CHUNK,
                json!({ "blob_id": begin.blob_id, "index": i, "data": b64(parts[i]) }),
            )
            .await,
        );
    }
    for id in ids {
        loop {
            match web.recv_msg(T).await.expect("a chunk response") {
                RpcMessage::Response(r) if r.id == RpcId::Num(id) => {
                    assert!(r.error.is_none(), "{:?}", r.error);
                    break;
                }
                _ => {}
            }
        }
    }
    let att: AttachmentRef = web.call(methods::BLOB_PUT_END, json!({ "blob_id": begin.blob_id })).await.unwrap();
    assert_eq!((att.bytes, att.width, att.height), (bytes.len() as u64, Some(1920), Some(1080)));
    // 带着它发一条消息
    let s: SessionSummary = web.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let _: SessionSendResult = web
        .call(
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "m1", "content": "", "attachments": [att] }),
        )
        .await
        .unwrap();
    assert!(
        eventually(T, || async { td.ctx.sessions.get_summary(&s.id).await.unwrap().status == SessionStatus::Idle })
            .await
    );
    // 另一台设备的视角：分页拉回整张图，字节一致
    let mut got = Vec::new();
    loop {
        let g: BlobGetResult =
            web.call(methods::BLOB_GET, json!({ "blob_id": att.blob_id, "offset": got.len() })).await.unwrap();
        assert_eq!(g.blob, att);
        got.extend(base64::engine::general_purpose::STANDARD.decode(&g.data).unwrap());
        if g.eof {
            break;
        }
    }
    assert_eq!(got, bytes);
    assert!(
        web.fragments_seen >= before + 6,
        "chunks and pages were fragmented ({} -> {})",
        before,
        web.fragments_seen
    );
    // 传输仍然同步
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.blobs);
    td.shutdown().await;
}

#[tokio::test]
async fn a_broken_frame_resets_the_peer_transport_and_a_new_handshake_recovers() {
    use anycon_protocol::envelope::frame;
    let (_hub, td, mut web) = paired_web_over_a_small_envelope_hub().await;
    let node = web.node_id.clone().unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    // 1) 解密不过的整条消息 -> daemon 丢掉传输并回一个 RESET
    let mut junk = vec![frame::WHOLE];
    junk.extend_from_slice(&[7u8; 64]);
    web.send_env(&node, Kind::Data, junk).await;
    let reset = loop {
        let env = web.recv_env(T).await.expect("daemon answers with a RESET");
        if env.kind == Kind::Data {
            break env;
        }
    };
    assert_eq!(reset.body, frame::reset_body());
    assert!(eventually(T, || async { td.ctx.peers.online_count() == 0 }).await, "the daemon dropped the peer session");
    // 之后的数据没人理（没有会话），客户端重新 IK 就好
    web.ik().await.expect("a new IK handshake works after a reset");
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    // 2) 分片乱序（第二片先到）-> 同样重置
    let mut bad = vec![frame::FRAGMENT];
    bad.extend_from_slice(&9u32.to_be_bytes());
    bad.extend_from_slice(&1u16.to_be_bytes());
    bad.extend_from_slice(&3u16.to_be_bytes());
    bad.extend_from_slice(&[1, 2, 3]);
    web.send_env(&node, Kind::Data, bad).await;
    let reset = loop {
        let env = web.recv_env(T).await.expect("RESET");
        if env.kind == Kind::Data {
            break env;
        }
    };
    assert_eq!(reset.body, frame::reset_body());
    web.ik().await.unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    // 3) 客户端主动发 RESET：daemon 丢掉会话，等新的握手
    web.send_env(&node, Kind::Data, frame::reset_body()).await;
    assert!(eventually(T, || async { td.ctx.peers.online_count() == 0 }).await);
    web.ik().await.unwrap();
    let _: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn a_tokenless_daemon_moves_to_a_fresh_hub_connection_when_the_temporary_one_is_used_up() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let attempts_before = td.ctx.hub_state.get().attempts;
    // 临时连接一生最多 3 次 PairBegin：第 4 次之前 daemon 自己换一条新连接（新码），不会失败
    let mut codes = Vec::new();
    for i in 0..5 {
        let r: NodePairBeginResult = local
            .call(&td, methods::NODE_PAIR_BEGIN, json!({ "long": i == 3 }))
            .await
            .unwrap_or_else(|e| panic!("pair_begin #{i} failed: {e:?}"));
        codes.push(r);
    }
    assert!(
        td.ctx.hub_state.get().attempts > attempts_before,
        "the daemon reconnected to get a fresh temporary connection: {:?}",
        td.ctx.hub_state.get()
    );
    let mut sorted: Vec<&str> = codes.iter().map(|c| c.code.as_str()).collect();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "every PairBegin uses a brand-new code");
    // 长码：16 位，链接里是 fragment
    assert_eq!(codes[3].code.len(), 19, "XXXX-XXXX-XXXX-XXXX");
    assert_eq!(codes[3].link, format!("{}/pair#code={}", hub.url(), codes[3].code));
    // 最后一个码（新连接上登记的）可以完成配对，包括 pair.ack 校验
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Late Joiner", Keypair::generate()).await;
    web.pair(&codes[4].code).await;
    assert!(web.last_ack.is_some());
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    // 长码也能配对（第二台设备，已有 token 的 daemon）
    let long: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({ "long": true })).await.unwrap();
    let mut web2 = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Long Code", Keypair::generate()).await;
    web2.pair(&long.code).await;
    assert_eq!(web2.last_ack.as_ref().unwrap().node_id, web.node_id.clone().unwrap());
    td.shutdown().await;
}

#[tokio::test]
async fn a_device_can_revoke_itself_and_still_gets_the_reply_before_being_disconnected() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Leaving Browser").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    let me = web.device_id.clone();
    // 「退出并移除本设备」：先回复（客户端据此知道成功），然后 daemon 才断开它
    let r: Empty = web.call(methods::NODE_REVOKE_DEVICE, json!({ "device_id": me })).await.expect("the reply arrives");
    let _ = r;
    assert!(td.ctx.peers.record(&me).is_none(), "the key is gone immediately");
    assert!(td.ctx.store.list_peers().await.unwrap().is_empty());
    assert!(
        eventually(T, || async { td.ctx.peers.online_count() == 0 }).await,
        "and the session is torn down after the reply"
    );
    // 设备已经不存在：not_found（客户端视为已完成）
    let mut local = td.client("cli");
    let e = local.call::<Empty>(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": me })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    td.shutdown().await;
}

#[tokio::test]
async fn a_revoked_device_with_the_same_key_can_pair_again_and_talk_to_the_daemon() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut other = pair_web(&td, &hub, "Keeper").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    other.ik().await.unwrap();
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    // B 配对 -> 被吊销 -> 用同一把静态密钥重新配对（新的设备 id）-> IK 正常
    let b_kp = Keypair::generate();
    let b_secret = b_kp.secret;
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut b = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Revoked B", b_kp).await;
    b.pair(&begin.code).await;
    b.ik().await.unwrap();
    let old_id = b.device_id.clone();
    let _: Empty = local.call(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": old_id })).await.unwrap();
    assert!(td.ctx.peers.record(&old_id).is_none());
    drop(b);
    // 同一把密钥，全新的连接和身份
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut b2 = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Revoked B", Keypair::from_secret(b_secret)).await;
    b2.pair(&begin.code).await;
    assert_ne!(b2.device_id, old_id, "a brand-new device id");
    b2.ik().await.expect("IK works after re-pairing with the same static key");
    let info: NodeInfo = b2.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    let devices: NodeDevicesResult = b2.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.iter().filter(|d| d.name == "Revoked B").count(), 1);
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// 发送方规则经真实 hub 的粘住规则

/// 发一个 `fs.read`，**先不读**，等 `hold` 完成之后再收：客户端读得慢，hub 往它的发送缓冲（4 MiB）灌不进去就退回，
/// daemon 必须暂停、按序原样重发。返回读到的文本长度。
async fn slow_fs_read(web: &mut FakeWeb, path: &str, hold: impl std::future::Future<Output = ()>) -> usize {
    let id = web.send_rpc(methods::FS_READ, json!({ "path": path, "max_bytes": 10 * 1024 * 1024 })).await;
    hold.await;
    loop {
        // recv_msg 对 RESET、解密失败、分片乱序都会 panic：即 Noise 没有失步
        match web.recv_msg(Duration::from_secs(30)).await.expect("the response arrives") {
            RpcMessage::Response(r) if r.id == RpcId::Num(id) => {
                let v: FsReadResult = serde_json::from_value(r.result.expect("a result")).unwrap();
                assert!(!v.truncated && v.text.bytes().all(|b| b == b'z'), "the content is intact");
                return v.text.len();
            }
            RpcMessage::Notification(n) => web.notes.push_back(n),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn multi_mib_fs_reads_arrive_in_order_through_the_hub_even_when_the_client_reads_slowly() {
    const MIB: usize = 1024 * 1024;
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    // 客户端 socket 的接收缓冲定死在 64 KiB：不定的话内核会把它自动调到几 MiB，客户端不读时大块数据被内核吞下，
    // hub 的发送缓冲满不满要看运气
    let cookie = hub.admin_cookie().await;
    let begin: NodePairBeginResult = td.client("cli").call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web =
        FakeWeb::connect_with(&hub.url(), None, Some(&cookie), "Slow Reader", Keypair::generate(), Some(64 * 1024))
            .await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    std::fs::write(td.workspace.join("five.txt"), "z".repeat(5 * MIB)).unwrap();
    std::fs::write(td.workspace.join("nine.txt"), "z".repeat(9 * MIB)).unwrap();

    // 5 MiB：客户端晚读一会儿。hub 一侧 socket 的发送缓冲由内核自动调整（macOS / Linux 默认上限都是 4 MiB），
    // 它加上 hub 的 4 MiB 可能装得下 5 MiB，所以这里不要求一定有退回
    let len = slow_fs_read(&mut web, "five.txt", tokio::time::sleep(Duration::from_millis(600))).await;
    assert_eq!(len, 5 * MIB);
    // 同一条传输之后照常
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");

    // 9 MiB：hub 的发送缓冲（4 MiB）+ hub 一侧 socket 的发送缓冲（至多 4 MiB）+ 定死的接收缓冲（Linux 记成两倍，128 KiB）
    // 装不下，客户端不读就一定有信封被退回。等到 daemon 真的收到退回才开始读：等的是这件事，不是一段固定的时间
    let bounced = td.ctx.outbox.bounce_count();
    let hold = async {
        let bounced_now =
            eventually(Duration::from_secs(30), || async { td.ctx.outbox.bounce_count() > bounced }).await;
        assert!(bounced_now, "the hub really did bounce envelopes (otherwise this test proves nothing)");
    };
    assert_eq!(slow_fs_read(&mut web, "nine.txt", hold).await, 9 * MIB);
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    td.shutdown().await;
}

#[tokio::test]
async fn thirty_mib_of_consecutive_traffic_leaves_the_node_usable() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Heavy User").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    std::fs::write(td.workspace.join("three.txt"), "z".repeat(3 * 1024 * 1024)).unwrap();
    // 连续 10 × 3 MiB = 30 MiB，其中几次让客户端慢一点读
    for i in 0..10 {
        let delay = if i % 3 == 0 { Duration::from_millis(300) } else { Duration::ZERO };
        let len = slow_fs_read(&mut web, "three.txt", tokio::time::sleep(delay)).await;
        assert_eq!(len, 3 * 1024 * 1024, "read #{i}");
    }
    // 之后 projects.list 正常，传输没有失步（没有 30 秒超时，也没有重新握手）
    let t = std::time::Instant::now();
    let projects: WorkspaceListResult = web.call(methods::WORKSPACE_LIST, json!({})).await.unwrap();
    let _ = projects;
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(td.ctx.peers.online_count() >= 1, "the peer session was never dropped");
    td.shutdown().await;
}

/// 读到下一封 `data` 信封；None = 超时。
async fn next_data(web: &mut FakeWeb, timeout: Duration) -> Option<Envelope> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let env = web.recv_env(left).await?;
        if env.kind == Kind::Data {
            return Some(env);
        }
        if env.kind == Kind::Control {
            web.controls.push_back(env.control_body().unwrap());
        }
    }
}

#[tokio::test]
async fn a_bounce_of_a_previous_transport_only_unsticks_the_hub_and_never_resets_the_current_one() {
    use anycon_protocol::envelope::frame;
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut web = pair_web(&td, &hub, "Re-handshaker").await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();
    // 第二次握手：新会话；seq 1 是很早以前（上一次传输、甚至上一次 hub 连接）发出的
    web.ik().await.unwrap();
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    td.ctx.peers.on_backpressure(&web.device_id, 1);
    assert_eq!(td.ctx.outbox.lost_count(), 1, "it is no longer retained");
    assert_eq!(td.ctx.peers.reset_count(), 0, "but the current transport is intact, so it is not reset");
    assert_eq!(td.ctx.peers.online_count(), 1);
    // 占位帧（同一个 seq 的 RESET）照样发出去，hub 的粘住状态因此能解开
    let env = next_data(&mut web, T).await.expect("the placeholder is sent");
    assert_eq!((env.seq, frame::decode(&env.body).unwrap()), (1, frame::DataFrame::Reset));
    // 当前传输没有受影响：同一条 Transport 照常往来
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    assert_eq!(td.ctx.peers.reset_count(), 0);
    // 对照：属于当前传输的 seq 找不到了 = 当前传输少了一封，只能重置
    td.ctx.peers.on_backpressure(&web.device_id, 5_000_000);
    assert_eq!(td.ctx.peers.reset_count(), 1);
    assert!(eventually(T, || async { td.ctx.peers.online_count() == 0 }).await, "the session is dropped");
    let env = next_data(&mut web, T).await.expect("a RESET");
    assert_eq!(frame::decode(&env.body).unwrap(), frame::DataFrame::Reset);
    web.ik().await.expect("and a new handshake recovers");
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// 确认阶段暂存的 peer 记录

/// 走到「daemon 已经以临时 id 登记了 joiner、发了 PairConfirm，但 joiner 一直不确认」。返回 (web, 临时 id)。
async fn stall_in_the_confirming_phase(td: &TestDaemon, hub: &TestHub) -> (FakeWeb, String) {
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Staller", Keypair::generate()).await;
    let (host_temp, _pub, _transport) = web.pair_handshake(&begin.code).await;
    // daemon 处理完第 3 条握手消息后先登记（临时 id）再确认
    assert!(
        eventually(T, || async { !td.ctx.store.list_peers().await.unwrap().is_empty() }).await,
        "the daemon registers the joiner before confirming"
    );
    let staged = td.ctx.store.list_peers().await.unwrap();
    assert_eq!(staged.len(), 1);
    assert_ne!(staged[0].device_id, host_temp, "keyed by the joiner's temporary id, not the daemon's");
    (web, staged[0].device_id.clone())
}

#[tokio::test]
async fn a_pairing_that_stalls_in_confirmation_is_not_listed_and_is_removed_when_the_hub_goes_away() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let (_web, temp_id) = stall_in_the_confirming_phase(&td, &hub).await;
    // 暂存期间：node.devices 不列它（`anycon pair` 轮询 node.devices，不能把临时 id 当成已配对）；通知也不发给它
    let mut local = td.client("cli");
    let devices: NodeDevicesResult = local.call(&td, methods::NODE_DEVICES, json!({})).await.unwrap();
    assert!(devices.devices.is_empty(), "{devices:?}");
    assert!(td.ctx.peers.list().is_empty() && !td.ctx.peers.paired_ids().contains(&temp_id));
    // 确认阶段 hub 断线：暂存的记录（数据库 + peers.json + 内存）都删掉
    let h = hub.handle;
    h.shutdown().await;
    assert!(eventually(T, || async { !td.ctx.hub_state.get().connected }).await);
    assert!(
        eventually(T, || async { td.ctx.store.list_peers().await.unwrap().is_empty() }).await,
        "the tentative peer row is deleted when the hub goes away"
    );
    assert!(td.ctx.peers.record(&temp_id).is_none());
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty(), "{peers_json}");
    let devices: NodeDevicesResult = local.call(&td, methods::NODE_DEVICES, json!({})).await.unwrap();
    assert!(devices.devices.is_empty(), "no temporary id in node.devices: {devices:?}");
    td.shutdown().await;
}

#[tokio::test]
async fn the_pairing_watchdog_removes_a_tentative_peer_that_never_gets_confirmed() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    td.ctx.peers.set_pair_step_timeout(Duration::from_millis(400));
    let (_web, temp_id) = stall_in_the_confirming_phase(&td, &hub).await;
    assert!(
        eventually(T, || async { td.ctx.store.list_peers().await.unwrap().is_empty() }).await,
        "the watchdog deletes the tentative row"
    );
    assert!(td.ctx.peers.record(&temp_id).is_none());
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty());
    // daemon 仍然可以重新配对（状态机没卡住）
    td.ctx.peers.set_pair_step_timeout(Duration::from_secs(70));
    let mut web = pair_web(&td, &hub, "Second Try").await;
    web.ik().await.unwrap();
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].name, "Second Try");
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// 暂存配对记录的其余作废路径

async fn no_trace_of(td: &TestDaemon, temp_id: &str) {
    assert!(
        eventually(T, || async { td.ctx.store.list_peers().await.unwrap().is_empty() }).await,
        "the tentative row is deleted"
    );
    assert!(td.ctx.peers.record(temp_id).is_none());
    assert!(td.ctx.peers.list().is_empty() && !td.ctx.peers.paired_ids().contains(&temp_id.to_string()));
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty(), "{peers_json}");
}

#[tokio::test]
async fn pair_failed_from_the_hub_removes_a_tentative_peer_that_was_waiting_for_confirmation() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let (web, temp_id) = stall_in_the_confirming_phase(&td, &hub).await;
    // 新设备在确认之前掉线：hub 对 daemon 发 `pair_failed { peer_gone }`
    drop(web);
    no_trace_of(&td, &temp_id).await;
    // 状态机没卡住：可以重新配对
    let mut web = pair_web(&td, &hub, "After The Failure").await;
    web.ik().await.unwrap();
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].name, "After The Failure");
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// `local.pair_outcome`：`anycon pair` 靠它知道这次配对被 hub 拒绝了

/// 本机方法 `local.pair_outcome`：这次配对（`node.pair_begin` 返回的 `expires_at_ms`）的失败结果。
async fn pair_outcome(td: &TestDaemon, local: &mut anycon_daemon::testutil::TestClient, expires_at_ms: u64) -> Value {
    local.call(td, anycon_daemon::local_methods::PAIR_OUTCOME, json!({ "expires_at_ms": expires_at_ms })).await.unwrap()
}

#[tokio::test]
async fn a_pairing_the_hub_refuses_is_reported_by_local_pair_outcome_for_that_pairing_only() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_eq!(pair_outcome(&td, &mut local, begin.expires_at_ms).await, json!({ "failed": null }), "nothing yet");

    // hub 拒绝了这次配对的收尾（多用户模式的设备配额满了）
    let before = anycon_daemon::util::now_ms();
    td.ctx.peers.on_control(Control::PairFailed { reason: PairFailReason::Quota }).await;
    let got = pair_outcome(&td, &mut local, begin.expires_at_ms).await;
    assert_eq!(got["failed"]["reason"], "quota", "{got}");
    assert!(got["failed"]["at_ms"].as_u64().unwrap() >= before, "{got}");
    assert_eq!(got["failed"].as_object().unwrap().len(), 2, "reason and at_ms only: {got}");
    // 别的配对（别的 expires_at_ms）看不到它
    assert_eq!(pair_outcome(&td, &mut local, begin.expires_at_ms + 1).await, json!({ "failed": null }));

    // 新的一次 pair_begin：旧的失败不会被当成这一次的
    let again: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_ne!(again.expires_at_ms, begin.expires_at_ms);
    assert_eq!(pair_outcome(&td, &mut local, again.expires_at_ms).await, json!({ "failed": null }));
    assert_eq!(pair_outcome(&td, &mut local, begin.expires_at_ms).await, json!({ "failed": null }));

    // 参数必填
    let missing: Result<Value, RpcError> = local.call(&td, anycon_daemon::local_methods::PAIR_OUTCOME, json!({})).await;
    assert_eq!(missing.unwrap_err().code, ErrorCode::Invalid.rpc_code());
    td.shutdown().await;
}

#[tokio::test]
async fn a_failure_with_no_pairing_in_progress_is_not_recorded() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    // 第一条 pair_failed 作废了这次配对；之后再来的（别的配对的）没有对应的状态，不能覆盖已记下的结果
    td.ctx.peers.on_control(Control::PairFailed { reason: PairFailReason::PeerGone }).await;
    td.ctx.peers.on_control(Control::PairFailed { reason: PairFailReason::Quota }).await;
    assert_eq!(pair_outcome(&td, &mut local, begin.expires_at_ms).await["failed"]["reason"], "peer_gone");
    td.shutdown().await;
}

#[tokio::test]
async fn a_joiner_that_disconnects_mid_pairing_is_reported_as_peer_gone_by_the_hub() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Quitter", Keypair::generate()).await;
    let _handshake = web.pair_handshake(&begin.code).await;
    drop(web);
    assert!(
        eventually(T, || async {
            let mut c = td.client("poll");
            pair_outcome(&td, &mut c, begin.expires_at_ms).await["failed"]["reason"] == "peer_gone"
        })
        .await,
        "the hub's pair_failed reaches local.pair_outcome"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn the_pairing_watchdog_reports_a_timeout_through_local_pair_outcome() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    td.ctx.peers.set_pair_step_timeout(Duration::from_millis(400));
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Staller", Keypair::generate()).await;
    let _handshake = web.pair_handshake(&begin.code).await;
    assert!(
        eventually(T, || async {
            let mut c = td.client("poll");
            pair_outcome(&td, &mut c, begin.expires_at_ms).await["failed"]["reason"] == "timeout"
        })
        .await,
        "a pairing that never gets confirmed ends as a timeout"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn a_hub_refusal_of_pair_begin_is_busy_with_a_reason_and_not_the_not_connected_error() {
    // 每个账号每天只能发起 1 次配对
    let hub = TestHub::start_with(|c| c.limits.daily_pairings_per_account = Some(1)).await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    // 第一次配对走的是临时连接（还没有账号），不计数；配对完成后这条连接就是正式设备，之后的 pair_begin 计入账号
    let _web = pair_web(&td, &hub, "First").await;
    let mut local = td.client("cli");
    let ok: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert!(!ok.code.is_empty());

    // 第二次：hub 回 pair_failed { quota }。这是「被拒绝」，不是「没连上 hub」（unsupported）
    let err = local.call::<NodePairBeginResult>(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Busy.rpc_code(), "{err:?}");
    let data = err.data.as_ref().expect("error data");
    assert_eq!((data.code, data.reason.as_deref()), (ErrorCode::Busy, Some("quota")), "{err:?}");
    assert!(err.message.contains("refused") && !err.message.contains("not connected"), "{}", err.message);
    // 被拒绝的这次没有留下进行中的配对
    assert!(td.ctx.peers.pair_failure(ok.expires_at_ms).is_none());
    td.shutdown().await;
}

#[tokio::test]
async fn a_new_pair_begin_takes_over_a_pairing_in_confirmation_and_removes_its_tentative_peer() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let (_web, temp_id) = stall_in_the_confirming_phase(&td, &hub).await;
    let mut local = td.client("cli");
    let again: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    no_trace_of(&td, &temp_id).await;
    // 顶掉之后新的这次配对能走完
    let cookie = hub.admin_cookie().await;
    let mut web2 = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Second", Keypair::generate()).await;
    web2.pair(&again.code).await;
    web2.ik().await.unwrap();
    let devices: NodeDevicesResult = web2.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["Second"]);
    td.shutdown().await;
}

#[tokio::test]
async fn a_pairing_cancelled_while_the_tentative_peer_is_being_saved_leaves_no_ghost_record() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    wait_daemon_connected(&td).await;
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    // daemon 已经把 peer 标成暂存、但写库还没落地时，看门狗把这次配对作废（hub 断线、新的 pair_begin、PairFailed 同理；
    // 这几条都从别的任务进来，不在 hub 连接的读循环里）
    td.ctx.peers.set_pair_step_timeout(Duration::from_millis(300));
    let gate = td.ctx.peers.hold_pair_saves();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Ghost", Keypair::generate()).await;
    let _ = web.pair_handshake(&begin.code).await;
    tokio::time::timeout(T, gate.started.notified()).await.expect("the daemon reaches the save");
    tokio::time::sleep(Duration::from_millis(700)).await; // 看门狗触发，已经对这个 peer 做过 discard_staged（删了个空）
                                                          // 现在放行：写库落地了，但这次配对已经不在了——记录必须被丢掉，而不是以临时 id 出现在 node.devices
    gate.release.notify_one();
    // 给 daemon 时间走完那一步，再看最终状态（之前的实现这里会留下一条记录）
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(td.ctx.store.list_peers().await.unwrap().is_empty(), "no ghost row in the database");
    assert!(td.ctx.peers.list().is_empty(), "nor in node.devices");
    assert!(td.ctx.peers.paired_ids().is_empty());
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().is_empty(), "{peers_json}");
    // 没有对被作废的这次配对发 PairConfirm：新设备等不到 PairDone
    assert!(
        web.recv_env(Duration::from_millis(300))
            .await
            .is_none_or(|e| e.kind != Kind::Control || !matches!(e.control_body(), Ok(Control::PairDone { .. }))),
        "the cancelled pairing must not complete"
    );
    td.shutdown().await;
}
