//! `anycon pair` 在 hub 拒绝这次配对时马上停下来，并说明原因（不再干等 5 分钟、最后报一个误导的「超时」）。
//!
//! 全程真实组件：进程内的 `multi` hub（设备配额只有 2）← 真实 `anycon daemon` 子进程 ← `FakeWeb` 当浏览器。
//! daemon 先和第一个浏览器配对（daemon + 浏览器 = 2 台设备，配额满）；第二个浏览器再配对时，两边都确认之后 hub
//! 回 `pair_failed { quota }`，`anycon pair` 必须据此退出，而不是等到短码过期。

mod common;

use std::path::Path;

use anycon_crypto::Keypair;
use anycon_daemon::testutil::FakeWeb;
use anycon_hub::config::{OAuthClient, QuotaLimits};
use anycon_hub::store::CreateAccountOutcome;
use anycon_hub::util::{hash_token, new_secret, now_ms};
use anycon_protocol::{Control, PairFailReason};
use common::*;

const NODE_NAME: &str = "test-node"; // FakeWeb::pair 校验节点名

struct MultiHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
}

impl MultiHub {
    /// `multi` 的 hub，每个账号最多 `devices` 台设备、滚动 24 小时内最多 `daily` 次发起配对。
    /// 登录方式只有（假的）GitHub 凭据：测试用库里直接建的会话。
    async fn start(devices: u32, daily: u32) -> MultiHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = anycon_hub::Config::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        c.oauth_github = Some(OAuthClient { client_id: "id".into(), client_secret: "secret".into() });
        c.enable_multi(QuotaLimits { devices, daemons: 3, connections: 10 }, daily);
        let handle = anycon_hub::run(c).await.expect("hub starts");
        MultiHub { handle, _dir: dir }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }

    /// 库里直接建一个账号和它的登录会话，返回 `anycon_session=…`。
    async fn account_cookie(&self) -> String {
        let store = self.handle.store();
        let CreateAccountOutcome::Created(account) =
            store.create_account_with_email("alice", None, now_ms()).await.unwrap()
        else {
            panic!("account created")
        };
        let raw = new_secret();
        store
            .create_web_session(&account.id, &hash_token(&raw), "test", "127.0.0.0/24", now_ms(), now_ms() + 3_600_000)
            .await
            .unwrap();
        format!("anycon_session={raw}")
    }
}

/// 第二个浏览器走到「两边都确认」，返回它从 hub 收到的结果。
async fn second_browser_confirms(hub: &MultiHub, cookie: &str, code: &str) -> Control {
    let mut web = FakeWeb::connect(&hub.url(), None, Some(cookie), "Second Browser", Keypair::generate()).await;
    let (host_temp, _pub, _transport) = web.pair_handshake(code).await;
    web.send_control(Control::PairConfirm { peer: host_temp }).await;
    web.wait_control(|c| matches!(c, Control::PairDone { .. } | Control::PairFailed { .. })).await
}

async fn start_node(hub: &MultiHub, dir: &Path) -> (std::path::PathBuf, DaemonProc) {
    let home = dir.join("daemon-home");
    init(&home, &dir.join("workspace"), NODE_NAME, Some(&hub.url())).await;
    for agent in ["claude-code", "codex"] {
        let r = anycon(&home, &["config", "set", &format!("agents.{agent}.enabled"), "false"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    let daemon = start_daemon(&home, &[]).await;
    (home, daemon)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_device_quota_stops_anycon_pair_at_once_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let hub = MultiHub::start(2, 20).await;
    let cookie = hub.account_cookie().await;
    let (home, daemon) = start_node(&hub, dir.path()).await;

    // 第一次配对：daemon 有了账号和 device token，库里 2 台设备（daemon + 浏览器），配额满了
    let first = PairProc::begin(&home).await;
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "First Browser", Keypair::generate()).await;
    web.pair(&first.code).await;
    assert_eq!(first.finish().await["paired"]["name"], "First Browser");

    // --json：hub 拒绝收尾之后，`anycon pair` 在几秒之内退出（退出码 1），最后一行 stdout 带机器可读的原因
    let started = std::time::Instant::now();
    let pair = PairProc::begin(&home).await;
    let result = second_browser_confirms(&hub, &cookie, &pair.code).await;
    assert_eq!(result, Control::PairFailed { reason: PairFailReason::Quota }, "the hub refuses the third device");
    let (rest, stderr, exit) = pair.finish_failed().await;
    assert_eq!(exit, 1, "quota is a general error; stderr: {stderr}");
    assert!(started.elapsed().as_secs() < 30, "it must not wait for the code to expire");
    let last: serde_json::Value = serde_json::from_str(rest.last().expect("a final JSON line")).unwrap();
    assert_eq!(last["failed"]["reason"], "quota", "{rest:?}");
    assert!(last["failed"]["message"].as_str().unwrap().contains("limit on devices"), "{last}");
    assert!(stderr.contains("error: quota: ") && stderr.contains("quota exceeded on the hub"), "{stderr}");
    assert!(!stderr.contains("timed out"), "not the misleading timeout: {stderr}");

    // 人读的输出：同样退出码 1，说明配额满了、怎么办
    let pair = PairProc::begin_with(&home, &[]).await;
    let result = second_browser_confirms(&hub, &cookie, &pair.code).await;
    assert_eq!(result, Control::PairFailed { reason: PairFailReason::Quota });
    let (rest, stderr, exit) = pair.finish_failed().await;
    assert_eq!(exit, 1, "stderr: {stderr}");
    assert!(rest.iter().all(|l| !l.contains("Paired with")), "{rest:?}");
    assert!(
        stderr.contains("limit on devices or nodes")
            && stderr.contains("Settings → Devices")
            && stderr.contains("anycon pair"),
        "the message says what to do: {stderr}"
    );

    // 腾出位置（吊销第一个浏览器）之后同样的操作又能成功：上一次的失败不会被当成这一次的
    let devices = anycon(&home, &["devices", "--json"], None, &[]).await.json();
    let first_browser = devices["devices"][0]["device_id"].as_str().unwrap().to_string();
    let r = anycon(&home, &["revoke", &first_browser, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let pair = PairProc::begin(&home).await;
    let mut web2 = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Third Browser", Keypair::generate()).await;
    web2.pair(&pair.code).await;
    assert_eq!(pair.finish().await["paired"]["name"], "Third Browser");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_pair_begin_is_reported_as_the_hub_refusing_not_as_the_node_being_offline() {
    let dir = tempfile::tempdir().unwrap();
    // 每个账号 24 小时内 2 个名额
    let hub = MultiHub::start(10, 2).await;
    let cookie = hub.account_cookie().await;
    let (home, daemon) = start_node(&hub, dir.path()).await;

    // 第一次配对：daemon 在临时连接上（还没有账号），名额记在加入的浏览器的账号上（1/2）
    let first = PairProc::begin(&home).await;
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "First Browser", Keypair::generate()).await;
    web.pair(&first.code).await;
    assert_eq!(first.finish().await["paired"]["name"], "First Browser");

    // 现在 daemon 是正式设备：这一次用掉最后一个名额（2/2）
    let r = anycon(&home, &["pair", "--no-wait", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);

    // 再来一次：hub 在登记时就拒绝了。退出码 1（不是「离线」的 3），原因 quota，--json 的 stdout 里有机器可读的原因
    let r = anycon(&home, &["pair", "--no-wait", "--json"], None, &[]).await;
    assert_eq!(r.code, 1, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    let lines = r.json_lines();
    assert_eq!(lines.len(), 1, "no pairing code was printed: {lines:?}");
    assert_eq!(lines[0]["failed"]["reason"], "quota");
    assert!(r.stderr.contains("error: quota: "), "{}", r.stderr);
    assert!(r.stderr.contains("daily pairing attempts"), "{}", r.stderr);
    assert!(!r.stderr.contains("offline") && !r.stderr.contains("not connected"), "{}", r.stderr);

    // 人读的输出：同样的退出码和说明
    let r = anycon(&home, &["pair", "--no-wait"], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    // 登记时的 quota 只可能是每日配对次数：不叫用户去移除设备，名额随 24 小时滚动窗口回来
    assert!(r.stderr.contains("used up its daily pairing attempts") && r.stderr.contains("24 hours"), "{}", r.stderr);
    assert!(r.stderr.contains("Settings → Quota"), "{}", r.stderr);
    assert!(!r.stderr.contains("remove a device") && !r.stderr.contains("Settings → Devices"), "{}", r.stderr);

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_pairing_stops_the_anycon_pair_that_was_waiting_on_the_replaced_one() {
    let dir = tempfile::tempdir().unwrap();
    let hub = MultiHub::start(10, 20).await;
    let (home, daemon) = start_node(&hub, dir.path()).await;

    // 一个 `anycon pair` 在等它的短码……
    let waiting = PairProc::begin(&home).await;
    // ……这时别的客户端（网页的「添加设备」、另一个 `anycon pair`）又开了一次配对，把它顶掉了
    let r = anycon(&home, &["pair", "--no-wait", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let newest = r.json()["code"].as_str().unwrap().to_string();
    assert_ne!(newest, waiting.code);

    // 等着的那个马上退出（退出码 1），说明短码作废了，而不是等 5 分钟
    let started = std::time::Instant::now();
    let (rest, stderr, exit) = waiting.finish_failed().await;
    assert_eq!(exit, 1, "stderr: {stderr}");
    assert!(started.elapsed().as_secs() < 30);
    let last: serde_json::Value = serde_json::from_str(rest.last().expect("a final JSON line")).unwrap();
    assert_eq!(last["failed"]["reason"], "replaced", "{rest:?}");
    assert!(
        stderr.contains("error: replaced: ") && stderr.contains("another pairing was started on this node"),
        "{stderr}"
    );
    assert!(stderr.contains("run `anycon pair` again"), "{stderr}");

    // 最新的那次不受影响：还能配对成功
    let cookie = hub.account_cookie().await;
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Browser", Keypair::generate()).await;
    web.pair(&newest).await;

    daemon.stop().await;
}
