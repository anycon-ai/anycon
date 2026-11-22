//! 多节点长连接客户端（`anycon_daemon::client::Client`）：一个设备身份、一条 hub 连接，同时和两个真 daemon 配对、收发，
//! 节点下线又上线、hub 重启之后照常可用，token 轮换，关停。hub 和 daemon 都是进程内的真实实现。

use std::time::Duration;

use anycon_crypto::Keypair;
use anycon_daemon::client::{client_info, Client, ClientEvent, ClientOptions, HubState, NodeStatus};
use anycon_daemon::testutil::{eventually, FakeWeb, TestDaemon};
use anycon_hub::Config as HubConfig;
use anycon_protocol::*;
use serde_json::{json, Value};
use tokio::sync::broadcast;

const T: Duration = Duration::from_secs(30);
const CALL: Duration = Duration::from_secs(10);

struct TestHub {
    handle: anycon_hub::Handle,
    dir: tempfile::TempDir,
}

impl TestHub {
    async fn start_with(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = HubConfig::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        tweak(&mut c);
        let handle = anycon_hub::run(c).await.expect("hub starts");
        TestHub { handle, dir }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }

    async fn admin_cookie(&self) -> String {
        let token = self.handle.generated_admin_token.clone().unwrap();
        let res = reqwest::Client::new()
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }
}

/// 起一个连着 hub 的 daemon，并让一台带账号的「浏览器」和它配好对（hub 要求第一次配对来自有账号的设备，之后别的设备才能加入）。
async fn node(hub: &TestHub) -> TestDaemon {
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(eventually(T, || async { td.ctx.hub_state.get().connected }).await);
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web =
        FakeWeb::connect(&hub.url(), None, Some(&hub.admin_cookie().await), "Setup", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    td
}

async fn code_of(td: &TestDaemon) -> String {
    let begin: NodePairBeginResult = td.client("code").call(td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    begin.code
}

fn options() -> ClientOptions {
    let mut o = ClientOptions::new(client_info("test-client", DeviceKind::Cli));
    o.reconnect_initial = Duration::from_millis(200);
    o
}

async fn node_id_of(td: &TestDaemon) -> String {
    td.ctx.identity.device_id().unwrap()
}

async fn info(client: &Client, node_id: &str) -> NodeInfo {
    serde_json::from_value(client.request(node_id, methods::NODE_INFO, json!({}), CALL).await.unwrap()).unwrap()
}

async fn wait_ready(client: &Client, node_id: &str) {
    assert!(
        eventually(T, || async { client.node_status(node_id) == NodeStatus::Ready }).await,
        "{node_id} did not become ready: {:?} / {:?}",
        client.node_status(node_id),
        client.last_error()
    );
}

/// 等一条满足条件的事件。
async fn wait_event(rx: &mut broadcast::Receiver<ClientEvent>, pred: impl Fn(&ClientEvent) -> bool) -> ClientEvent {
    tokio::time::timeout(T, async {
        loop {
            match rx.recv().await {
                Ok(e) if pred(&e) => return e,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("event stream ended: {e}"),
            }
        }
    })
    .await
    .expect("the expected event arrived")
}

fn todo_add(title: &str) -> Value {
    json!({ "id": uuid::Uuid::new_v4().to_string(), "title": title })
}

struct Rig {
    hub: TestHub,
    a: TestDaemon,
    b: TestDaemon,
    home: tempfile::TempDir,
    client: Client,
    a_id: String,
    b_id: String,
}

impl Rig {
    fn state(&self, hub_url: &str) -> HubState {
        HubState::load(&self.home.path().join("client"), hub_url)
    }
}

/// 两个节点，一个设备身份，先后和两个节点配对（第一次用临时连接，第二次在已经开着的长连接上）。
async fn rig(tweak: impl FnOnce(&mut HubConfig)) -> Rig {
    let hub = TestHub::start_with(tweak).await;
    let a = node(&hub).await;
    let b = node(&hub).await;
    let (a_id, b_id) = (node_id_of(&a).await, node_id_of(&b).await);
    let home = tempfile::tempdir().unwrap();
    let state = HubState::load(&home.path().join("client"), &hub.url());
    let client = Client::start(state, options()).unwrap();
    assert!(!client.hub_connected(), "a device without a token holds no hub connection");
    let entry = client.pair(&code_of(&a).await, Some(&a_id)).await.unwrap();
    assert_eq!(entry.id, a_id);
    assert!(eventually(T, || async { client.hub_connected() }).await, "the long connection opens after pairing");
    let entry = client.pair(&code_of(&b).await, Some(&b_id)).await.unwrap();
    assert_eq!(entry.id, b_id);
    Rig { hub, a, b, home, client, a_id, b_id }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_identity_pairs_with_two_nodes_and_talks_to_each() {
    let r = rig(|_| {}).await;
    let c = &r.client;
    let mut events = c.subscribe();
    wait_ready(c, &r.a_id).await;
    wait_ready(c, &r.b_id).await;
    assert_eq!(c.nodes().len(), 2);

    // 各自的请求各自的回应
    assert_eq!(info(c, &r.a_id).await.node_id, r.a_id);
    assert_eq!(info(c, &r.b_id).await.node_id, r.b_id);
    // 没配对过的节点：未配对错误；节点回的错误响应带着原始的 RpcError
    let e = c.request("nobody", methods::NODE_INFO, json!({}), CALL).await.unwrap_err();
    assert_eq!(e.code, "not_paired");
    let e = c.request(&r.a_id, "no.such.method", json!({}), CALL).await.unwrap_err();
    assert!(e.rpc.is_some(), "{e}");

    // 各自的通知，带节点 id
    let _: Value = r.a.client("x").call(&r.a, methods::TODO_ADD, todo_add("on a")).await.unwrap();
    let ev = wait_event(&mut events, |e| matches!(e, ClientEvent::Notification { .. })).await;
    let ClientEvent::Notification { node_id, notification } = ev else { unreachable!() };
    assert_eq!((node_id.as_str(), notification.method.as_str()), (r.a_id.as_str(), methods::TODO_CHANGED));
    let _: Value = r.b.client("x").call(&r.b, methods::TODO_ADD, todo_add("on b")).await.unwrap();
    let ev = wait_event(&mut events, |e| matches!(e, ClientEvent::Notification { .. })).await;
    let ClientEvent::Notification { node_id, .. } = ev else { unreachable!() };
    assert_eq!(node_id, r.b_id);

    // 一个设备身份：本地状态里只有一份 device id，两个节点各自把它记成已配对设备
    let st = r.state(&r.hub.url());
    assert_eq!(st.nodes.len(), 2);
    let me = st.device_id.clone().unwrap();
    for id in [&r.a_id, &r.b_id] {
        let devices: NodeDevicesResult =
            serde_json::from_value(c.request(id, methods::NODE_DEVICES, json!({}), CALL).await.unwrap()).unwrap();
        assert!(devices.devices.iter().any(|d| d.device_id == me), "{me} should be a paired device of {id}");
    }
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_goes_offline_and_comes_back_is_usable_again_and_the_other_is_unaffected() {
    let mut r = rig(|_| {}).await;
    wait_ready(&r.client, &r.a_id).await;
    wait_ready(&r.client, &r.b_id).await;
    let mut events = r.client.subscribe();
    r.a.restart().await;
    let (cid, aid) = (r.client.clone(), r.a_id.clone());
    wait_event(&mut events, |e| matches!(e, ClientEvent::NodeOffline { node_id } if node_id == &aid)).await;
    // 下线期间：A 的请求立刻失败，B 照常
    if r.client.node_status(&r.a_id) != NodeStatus::Ready {
        let e = cid.request(&r.a_id, methods::NODE_INFO, json!({}), CALL).await;
        assert!(e.is_err() || cid.node_status(&r.a_id) == NodeStatus::Ready);
    }
    assert_eq!(info(&r.client, &r.b_id).await.node_id, r.b_id);
    // 上线后重新握手，重新可用
    wait_event(&mut events, |e| matches!(e, ClientEvent::NodeReady { node_id } if node_id == &r.a_id)).await;
    assert_eq!(info(&r.client, &r.a_id).await.node_id, r.a_id);
    // 重启后的通知也到
    let _: Value = r.a.client("x").call(&r.a, methods::TODO_ADD, todo_add("after restart")).await.unwrap();
    let ev = wait_event(&mut events, |e| matches!(e, ClientEvent::Notification { .. })).await;
    let ClientEvent::Notification { node_id, .. } = ev else { unreachable!() };
    assert_eq!(node_id, r.a_id);
    r.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_the_hub_restarts_the_client_reconnects_and_rehandshakes_every_node() {
    let r = rig(|_| {}).await;
    wait_ready(&r.client, &r.a_id).await;
    wait_ready(&r.client, &r.b_id).await;
    let mut events = r.client.subscribe();
    let (addr, dir) = (r.hub.handle.addr, r.hub.dir);
    r.hub.handle.shutdown().await;
    wait_event(&mut events, |e| matches!(e, ClientEvent::HubDisconnected)).await;
    assert!(!r.client.hub_connected());
    assert_eq!(r.client.node_status(&r.a_id), NodeStatus::Offline);
    // 断线期间的请求立刻以离线收场，不会挂着
    let e = r.client.request(&r.a_id, methods::NODE_INFO, json!({}), CALL).await.unwrap_err();
    assert_eq!(e.code, "offline");
    let mut c = HubConfig::new("http://127.0.0.1", dir.path()).unwrap();
    c.listen = addr;
    c.admin_token = Some("x".repeat(32));
    let hub2 = anycon_hub::run(c).await.expect("hub restarts on the same port");
    wait_ready(&r.client, &r.a_id).await;
    wait_ready(&r.client, &r.b_id).await;
    assert_eq!(info(&r.client, &r.a_id).await.node_id, r.a_id);
    assert_eq!(info(&r.client, &r.b_id).await.node_id, r.b_id);
    r.client.shutdown().await;
    hub2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_device_token_is_rotated_on_the_long_connection_and_the_client_keeps_working() {
    let hub_tweak = |c: &mut HubConfig| c.timing.token_rotate_min_interval = Duration::ZERO;
    let r = rig(hub_tweak).await;
    wait_ready(&r.client, &r.a_id).await;
    r.client.shutdown().await;
    // 让 token 看起来是很久以前签发的，重新启动：连上之后就轮换
    let hub_dir = r.home.path().join("client");
    let token_file = |d: &std::path::Path| std::fs::read_to_string(d.join("device-token")).unwrap();
    let old = token_file(&hub_dir);
    std::fs::write(hub_dir.join("device-token-issued-at"), "1000").unwrap();
    let client = Client::start(HubState::load(&hub_dir, &r.hub.url()), options()).unwrap();
    assert!(
        eventually(T, || async { token_file(&hub_dir) != old }).await,
        "the hub issued a new token and the client saved it"
    );
    // 先落 token、后落签发时间：两个文件之间有一个很短的间隙
    let issued =
        || std::fs::read_to_string(hub_dir.join("device-token-issued-at")).unwrap().trim().parse::<u64>().unwrap();
    assert!(eventually(T, || async { issued() > 1000 }).await, "the issue time was refreshed after the token");
    wait_ready(&client, &r.a_id).await;
    assert_eq!(info(&client, &r.a_id).await.node_id, r.a_id);
    client.shutdown().await;
    // 用轮换后的 token 能再连上
    let client = Client::start(HubState::load(&hub_dir, &r.hub.url()), options()).unwrap();
    wait_ready(&client, &r.b_id).await;
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_disconnects_cleanly_and_later_calls_fail_without_hanging() {
    let r = rig(|_| {}).await;
    wait_ready(&r.client, &r.a_id).await;
    r.client.shutdown().await;
    assert!(!r.client.hub_connected());
    let e = r.client.request(&r.a_id, methods::NODE_INFO, json!({}), CALL).await.unwrap_err();
    assert_eq!(e.code, "offline");
    // hub 那边看到设备下线了：daemon 收到 peer_offline（再起一个客户端不会被「被顶掉」）
    let client = Client::start(r.state(&r.hub.url()), options()).unwrap();
    wait_ready(&client, &r.a_id).await;
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_with_the_wrong_expected_node_discards_the_pairing_locally() {
    let hub = TestHub::start_with(|_| {}).await;
    let a = node(&hub).await;
    let home = tempfile::tempdir().unwrap();
    let client = Client::start(HubState::load(&home.path().join("client"), &hub.url()), options()).unwrap();
    let e = client.pair(&code_of(&a).await, Some("someone-else")).await.unwrap_err();
    assert_eq!(e.code, "node_mismatch");
    assert!(client.nodes().is_empty());
    client.shutdown().await;
}
