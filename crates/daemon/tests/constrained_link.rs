//! 受限链路上的发送方规则：daemon 的上行被限速、客户端时快时慢地读，
//! 同时有多条 MiB 级的响应在路上。
//!
//! 拓扑（全在进程内）：真实 hub ← 限速 TCP 代理 ← daemon；假 web 客户端直连 hub，并且每隔一阵就停读一阵
//! （模拟手机切后台）。hub 的 daemon 速率预算调大，排除限速退回，只剩「目标缓冲满」的退回——这就是要复现的现场：
//! daemon 的保留缓冲逐出了「已经发出、hub 还没处理」的旧分片，它们随后被退回时找不到原文，只能重置传输。
//!
//! 之前的实现在这个场景下 6 轮全部失败（`the hub bounced an envelope that is no longer retained` -> RESET）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anycon_crypto::Keypair;
use anycon_daemon::testutil::{eventually, FakeWeb, TestDaemon};
use anycon_hub::Config as HubConfig;
use anycon_protocol::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const T: Duration = Duration::from_secs(10);
const MIB: usize = 1024 * 1024;

struct TestHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
    http: reqwest::Client,
}

impl TestHub {
    async fn start(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
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

/// 限速 TCP 代理：监听本机随机端口，把每条连接转到 `hub`。**客户端 -> hub** 方向限速到 `up_bytes_per_sec`
/// （读得慢，对方的 TCP 窗口就满，对方的写就阻塞——和真实的受限上行一样把背压一路传回发送方），反方向不限。
async fn throttled_proxy(hub: SocketAddr, up_bytes_per_sec: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(upstream) = TcpStream::connect(hub).await else { continue };
            let (mut cr, mut cw) = client.into_split();
            let (mut ur, mut uw) = upstream.into_split();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                let mut next = tokio::time::Instant::now();
                while let Ok(n) = cr.read(&mut buf).await {
                    if n == 0 || uw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    // 按字节数排期；落后了不攒突发
                    next = next.max(tokio::time::Instant::now())
                        + Duration::from_secs_f64(n as f64 / up_bytes_per_sec as f64);
                    tokio::time::sleep_until(next).await;
                }
                let _ = uw.shutdown().await;
            });
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut ur, &mut cw).await;
                let _ = cw.shutdown().await;
            });
        }
    });
    addr
}

/// 读回 `ids` 对应的 `fs.read` 响应：读 `run` 这么久、停 `stall` 这么久，如此反复（停读期间 hub 往客户端的缓冲灌满就退回）。
/// 每个响应都逐字节核对（文件内容是 `z`）。返回收齐用的时间。
async fn read_with_stalls(web: &mut FakeWeb, ids: &[u64], len: usize, run: Duration, stall: Duration) -> Duration {
    let started = Instant::now();
    let mut left: Vec<u64> = ids.to_vec();
    while !left.is_empty() {
        let phase_end = Instant::now() + run;
        while !left.is_empty() && Instant::now() < phase_end {
            // recv_msg 对 RESET、解密失败、分片乱序都会 panic：即 Noise 没有失步
            match web.recv_msg(Duration::from_millis(100)).await {
                Some(RpcMessage::Response(r)) => {
                    let RpcId::Num(id) = r.id else { panic!("unexpected id {:?}", r.id) };
                    let at = left.iter().position(|x| *x == id).expect("a response for a request we sent");
                    left.remove(at);
                    let v: FsReadResult = serde_json::from_value(r.result.expect("a result, not an error")).unwrap();
                    assert!(!v.truncated && v.text.len() == len && v.text.bytes().all(|b| b == b'z'), "intact");
                }
                Some(RpcMessage::Notification(n)) => web.notes.push_back(n),
                Some(other) => panic!("unexpected {other:?}"),
                None => {}
            }
        }
        if !left.is_empty() {
            tokio::time::sleep(stall).await;
        }
        assert!(started.elapsed() < Duration::from_secs(120), "gave up waiting for {left:?}");
    }
    started.elapsed()
}

/// 一整轮场景：`rounds` 轮，每轮同时发 3 个 9 MiB 的 `fs.read`（用生产默认的 10 秒在途保护）。
async fn scenario(rounds: usize) {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_test_writer().try_init();
    // daemon 的预算调大：这里要的是「缓冲满」的退回，不是限速
    let hub = TestHub::start(|c| c.limits.daemon_bytes_per_min = 10 * 1024 * 1024 * 1024).await;
    let proxy = throttled_proxy(hub.handle.addr, 6 * MIB).await;
    let td = TestDaemon::start_full(&[], true, Some(&format!("http://{proxy}"))).await;
    assert!(eventually(T, || async { td.ctx.hub_state.get().connected }).await, "{:?}", td.ctx.hub_state.get());

    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Stalling Reader", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.unwrap();

    std::fs::write(td.workspace.join("nine.txt"), "z".repeat(9 * MIB)).unwrap();
    let mut took = Vec::new();
    for round in 1..=rounds {
        // 同时发 3 个：daemon 对同一个客户端的请求串行处理，3 条 9 MiB 的响应一条接一条排进出站队列
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(web.send_rpc(methods::FS_READ, json!({ "path": "nine.txt", "max_bytes": 10 * MIB })).await);
        }
        let d =
            read_with_stalls(&mut web, &ids, 9 * MIB, Duration::from_millis(2500), Duration::from_millis(2500)).await;
        eprintln!("round {round}: 3 x 9 MiB in {d:?}");
        took.push(d);
    }
    assert!(td.ctx.outbox.bounce_count() > 0, "the scenario really bounced envelopes (otherwise it proves nothing)");
    assert_eq!(td.ctx.outbox.lost_count(), 0, "no bounced envelope was missing from the retention buffer");
    assert_eq!(td.ctx.peers.reset_count(), 0, "the transport was never reset");
    assert_eq!(td.ctx.peers.online_count(), 1, "and the session is still the same one");
    // 之后传输照常
    let info: NodeInfo = web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    td.shutdown().await;
}

/// 2 轮：保留窗口（16 MiB）填满后，每条大响应要等最早发出的老过 10 秒的在途保护才能腾出位置，一轮约 15 秒。
/// （在途保护不能缩短来加速这个用例：这条假链路上实测有 2–3 秒的在途数据，3 秒的保护会重新触发「被退回时找不到原文」。）
// Windows runner 上调试构建慢一倍（一轮约 30 秒），在途数据超过 10 秒的在途保护，测到的是机器速度而不是发送方规则
#[cfg_attr(windows, ignore = "the Windows runner is too slow for the 10 s in-flight guard this scenario is sized for")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_concurrent_9_mib_reads_survive_a_6_mib_s_uplink_and_a_client_that_stalls() {
    scenario(2).await;
}

/// 完整验收：6 轮（约 90 秒），`just test` 不跑；
/// `cargo test -p anycon-daemon --test constrained_link -- --ignored` 跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "takes ~90 s"]
async fn six_rounds_of_three_concurrent_9_mib_reads_all_succeed() {
    scenario(6).await;
}
