//! 分身连接在被使用的节点（B）上的建立和断开（docs/spec/daemon.md § 27.2）：daemon ↔ 真实 hub（进程内）↔ 假的分身设备。
//!
//! `node.twin_link_code` 开一次只接受分身设备的短码配对；普通短码不接受 `twin` 种类；`[twin] accept_links` 关掉时断开已有的分身设备、
//! 拒绝新的；建立和吊销时发安全事件和 `notify { security }`、记审计；配对完成之后设备种类落盘，白名单和推送过滤在真实的加密通道上生效。

use std::sync::Arc;
use std::time::Duration;

use anycon_crypto::{derive_pair_secrets, Keypair};
use anycon_daemon::notify::testing::Recorder;
use anycon_daemon::testutil::{eventually, FakeWeb, TestClient, TestDaemon};
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

struct World {
    hub: TestHub,
    td: TestDaemon,
    cookie: String,
    local: TestClient,
}

async fn world_with(hub: TestHub) -> World {
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(eventually(T, || async { td.ctx.hub_state.get().connected }).await, "the daemon connects to the hub");
    let cookie = hub.admin_cookie().await;
    let local = td.client("cli");
    World { hub, td, cookie, local }
}

async fn world() -> World {
    world_with(TestHub::start().await).await
}

impl World {
    /// 一台普通的已配对设备（网页）：它收 `node.security_event`，也能吊销设备。
    async fn pair_web(&mut self, name: &str) -> FakeWeb {
        let begin: NodePairBeginResult = self.local.call(&self.td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
        let mut web = FakeWeb::connect(&self.hub.url(), None, Some(&self.cookie), name, Keypair::generate()).await;
        web.pair(&begin.code).await;
        web.ik().await.expect("IK handshake");
        web
    }

    async fn link_code(&mut self) -> NodeTwinLinkCodeResult {
        let raw: Value = self.local.call(&self.td, methods::NODE_TWIN_LINK_CODE, json!({})).await.unwrap();
        Schemas::load().check("NodeTwinLinkCodeResult", &raw, "node.twin_link_code");
        serde_json::from_value(raw).unwrap()
    }

    /// 以分身设备的身份和这个节点配对（用 `node.twin_link_code` 的短码），再做 IK 握手。
    async fn pair_twin(&self, code: &str, name: &str) -> FakeWeb {
        let mut twin = FakeWeb::connect_kind(
            &self.hub.url(),
            None,
            Some(&self.cookie),
            name,
            Keypair::generate(),
            None,
            DeviceKind::Twin,
        )
        .await;
        twin.pair(code).await;
        twin.ik().await.expect("IK handshake");
        twin
    }

    async fn audit(&mut self) -> Vec<AuditEntry> {
        let r: NodeAuditResult = self.local.call(&self.td, methods::NODE_AUDIT, json!({ "limit": 200 })).await.unwrap();
        r.entries
    }
}

#[tokio::test]
async fn a_twin_link_code_pairs_a_twin_device_whose_kind_is_stored_and_who_is_announced_and_audited() {
    let mut w = world().await;
    let recorder = Arc::new(Recorder::default());
    let mut web = w.pair_web("Phone").await;
    // 用 Recorder 接住 `notify`（出口换成它之后，发给设备的 notify 信封不再真的发）
    w.td.ctx.notifier.set_out(recorder.clone());

    // 短码：16 位，10 分钟，结果按 schema 校验
    let before = anycon_daemon::util::now_ms();
    let code = w.link_code().await;
    assert_eq!(code.code.len(), 19, "XXXX-XXXX-XXXX-XXXX");
    let ten_minutes = 10 * 60 * 1000;
    assert!(
        code.expires_ms >= before + ten_minutes - 1000 && code.expires_ms <= before + ten_minutes + 5000,
        "{code:?}"
    );

    // 分身设备以 `kind = twin` 配对：种类和公钥一起落盘（库、peers.json、设备列表）
    let mut twin = w.pair_twin(&code.code, "A's twin").await;
    let id = twin.device_id.clone();
    assert_eq!(w.td.ctx.peers.record(&id).unwrap().kind, DeviceKind::Twin);
    let rows = w.td.ctx.store.list_peers().await.unwrap();
    assert!(rows.iter().any(|r| r.device_id == id && r.kind == DeviceKind::Twin), "{rows:?}");
    let peers_json: Value = serde_json::from_str(&std::fs::read_to_string(w.td.paths.peers_json()).unwrap()).unwrap();
    assert!(peers_json["peers"].as_array().unwrap().iter().any(|p| p["device_id"] == json!(id) && p["kind"] == "twin"));
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.iter().find(|d| d.device_id == id).map(|d| d.kind), Some(DeviceKind::Twin));

    // 其他已配对设备收到安全事件，并且发了 `notify { security }`；审计里有记录
    let ev = web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT).await;
    Schemas::load().check("NodeSecurityEvent", &ev.params, "node.security_event");
    assert_eq!((ev.params["kind"].as_str(), ev.params["device_id"].as_str()), (Some("twin_linked"), Some(id.as_str())));
    assert_eq!(ev.params["device_name"], "A's twin");
    assert!(recorder.0.lock().unwrap().iter().any(|b| b.category == NotifyCategory::Security));
    let audit = w.audit().await;
    assert!(audit.iter().any(|e| e.action == "twin.linked" && e.target.as_deref() == Some(id.as_str())), "{audit:?}");
    assert!(audit.iter().any(|e| e.action == "node.twin_link_code"));
    assert!(!audit.iter().any(|e| e.detail.to_string().contains(&code.code)), "the code is never audited");

    // 经 IK 的真实加密通道：白名单里的能用，别的 `forbidden`，通知不会推过来
    let info: NodeInfo = twin.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.workspace_root.is_none() && info.twin_accept_links);
    let status: TwinPeerStatus = twin.call(methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert_eq!(status.projects.len(), 0, "a new twin connection is opened nothing");
    for (method, params) in [
        (methods::SESSION_LIST, json!({})),
        (methods::NODE_DEVICES, json!({})),
        (methods::NODE_TWIN_LINK_CODE, json!({})),
        (methods::TWIN_SET, json!({ "requester": id, "projects": ["x"] })),
        (methods::NODE_REVOKE_DEVICE, json!({ "device_id": id })),
        ("no.such_method", json!({})),
    ] {
        let e = twin.call_raw(method, params).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{method}");
        assert_eq!(e.data.as_ref().and_then(|d| d.reason.as_deref()), Some("twin_device"), "{method}");
    }
    assert_eq!(w.td.ctx.peers.record(&id).unwrap().kind, DeviceKind::Twin, "nothing it tried changed anything");
    // 节点上有别的事发生（人开了会话，其他设备收得到 session.changed）：分身设备什么都收不到
    let s: SessionSummary = web
        .call(methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hello", "message_id": "w1" }))
        .await
        .unwrap();
    web.next_note(|n| n.method == methods::SESSION_CHANGED && n.params["session_id"] == json!(s.id)).await;
    assert!(twin.recv_msg(Duration::from_millis(700)).await.is_none(), "no notification reached the twin device");

    // 吊销：和别的设备一样；其他设备收到 `twin_unlinked`，审计有记录；这台设备的握手不再被接受
    web.call::<Value>(methods::NODE_REVOKE_DEVICE, json!({ "device_id": id })).await.unwrap();
    web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT && n.params["kind"] == "twin_unlinked").await;
    assert!(w.td.ctx.peers.record(&id).is_none());
    let audit = w.audit().await;
    assert!(audit.iter().any(|e| e.action == "twin.unlinked" && e.target.as_deref() == Some(id.as_str())));
    assert!(twin.ik().await.is_err(), "a revoked twin device cannot handshake");
    w.td.shutdown().await;
}

#[tokio::test]
async fn an_ordinary_pairing_code_does_not_accept_a_twin_device() {
    let mut w = world().await;
    let begin: NodePairBeginResult = w.local.call(&w.td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut twin = FakeWeb::connect_kind(
        &w.hub.url(),
        None,
        Some(&w.cookie),
        "A's twin",
        Keypair::generate(),
        None,
        DeviceKind::Twin,
    )
    .await;
    // 握手走到第三条（带着 `ClientInfo.kind = twin`）：daemon 作废这次配对、不发 `pair_confirm`
    let _ = twin.pair_handshake(&begin.code).await;
    let reported = eventually(T, || {
        let begin = begin.clone();
        let mut local = w.td.client("poll");
        let td = &w.td;
        async move {
            let got: Value = local
                .call(td, anycon_daemon::local_methods::PAIR_OUTCOME, json!({ "expires_at_ms": begin.expires_at_ms }))
                .await
                .unwrap();
            got["failed"]["reason"] == "twin_not_allowed"
        }
    })
    .await;
    assert!(reported, "local.pair_outcome says twin_not_allowed");
    assert!(w.td.ctx.peers.list().is_empty(), "no device was recorded");
    assert!(w.td.ctx.store.list_peers().await.unwrap().is_empty());
    let audit = w.audit().await;
    assert!(audit.iter().any(|e| e.action == "pair.refused" && e.detail["reason"] == "twin_not_allowed"), "{audit:?}");
    // 状态机没卡住：普通设备照常配对
    let mut web = w.pair_web("Phone").await;
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    w.td.shutdown().await;
}

#[tokio::test]
async fn a_twin_link_code_only_accepts_a_twin_device() {
    let mut w = world().await;
    let code = w.link_code().await;
    let mut web = FakeWeb::connect(&w.hub.url(), None, Some(&w.cookie), "Phone", Keypair::generate()).await;
    let _ = web.pair_handshake(&code.code).await;
    let refused = eventually(T, || {
        let mut local = w.td.client("poll");
        let td = &w.td;
        async move {
            let r: NodeAuditResult = local.call(td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
            r.entries.iter().any(|e| e.action == "pair.refused" && e.detail["reason"] == "twin_not_allowed")
        }
    })
    .await;
    assert!(refused);
    assert!(w.td.ctx.peers.list().is_empty());
    w.td.shutdown().await;
}

#[tokio::test]
async fn only_one_twin_link_code_is_valid_at_a_time_and_it_lives_ten_minutes() {
    // hub 的登记只有 3 秒：daemon 要在它到期之前续上，这个码才能活 10 分钟
    let mut w = world_with(TestHub::start_with(|c| c.timing.pair_ttl = Duration::from_secs(3)).await).await;
    // 节点已经有了设备身份（第一次配对用的临时连接有 `pair_begin` 次数的上限，和这里无关）
    let _phone = w.pair_web("Phone").await;
    let first = w.link_code().await;
    // 再要一个：旧的作废
    let second = w.link_code().await;
    assert_ne!(first.code, second.code);
    let mut probe = FakeWeb::connect_kind(
        &w.hub.url(),
        None,
        Some(&w.cookie),
        "A's twin",
        Keypair::generate(),
        None,
        DeviceKind::Twin,
    )
    .await;
    probe
        .send_control(Control::PairJoin { match_id: derive_pair_secrets(&first.code).unwrap().match_id.clone() })
        .await;
    let failed = probe.wait_control(|c| matches!(c, Control::PairMatched { .. } | Control::PairFailed { .. })).await;
    assert!(matches!(failed, Control::PairFailed { reason: PairFailReason::NotFound }), "{failed:?}");
    drop(probe);
    // 过了 hub 登记的时间，新的码仍然有效
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let twin = w.pair_twin(&second.code, "A's twin").await;
    assert_eq!(w.td.ctx.peers.record(&twin.device_id).unwrap().kind, DeviceKind::Twin);
    w.td.shutdown().await;
}

#[tokio::test]
async fn accept_links_off_refuses_new_codes_disconnects_existing_twin_devices_and_keeps_their_records_and_scopes() {
    let mut w = world().await;
    let _phone = w.pair_web("Phone").await;
    w.td.fake.declare_no_push.store(true, std::sync::atomic::Ordering::SeqCst);
    w.td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    let code = w.link_code().await;
    let mut twin = w.pair_twin(&code.code, "A's twin").await;
    let id = twin.device_id.clone();
    // B 的人给这条连接开放一个项目
    std::fs::create_dir_all(w.td.workspace.join("p")).unwrap();
    w.local
        .call::<TwinGetResult>(&w.td, methods::TWIN_SET, json!({ "requester": id, "projects": ["p"] }))
        .await
        .unwrap();
    assert_eq!(twin.call::<TwinPeerStatus>(methods::TWIN_PEER_STATUS, json!({})).await.unwrap().projects, vec!["p"]);
    // 一个还没用掉的码，关掉之后也作废
    let open_code = w.link_code().await;

    // 关掉：`node.info` 反映；已有的传输断开、握手被拒；记录和范围保留
    anycon_daemon::config::set_value(&w.td.paths, "twin.accept_links", "false").unwrap();
    w.td.ctx.config.reload().unwrap();
    let info: NodeInfo = w.local.call(&w.td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(!info.twin_accept_links);
    assert!(
        // 只剩手机一条传输
        eventually(T, || async { w.td.ctx.peers.online_count() == 1 }).await,
        "the twin device's transport was dropped"
    );
    assert!(twin.ik().await.is_err(), "its handshake is refused while accept_links is off");
    assert_eq!(w.td.ctx.peers.record(&id).unwrap().kind, DeviceKind::Twin, "the record stays");
    let scope: TwinGetResult = w.local.call(&w.td, methods::TWIN_GET, json!({ "requester": id })).await.unwrap();
    assert_eq!((scope.scope.projects, scope.state, scope.enabled), (vec!["p".to_string()], TwinState::Disabled, false));
    // 新的码要不到
    let e = w.local.call::<Value>(&w.td, methods::NODE_TWIN_LINK_CODE, json!({})).await.unwrap_err();
    assert_eq!(
        (e.stable_code(), e.data.as_ref().and_then(|d| d.reason.clone()).as_deref()),
        (Some(ErrorCode::Forbidden), Some("accept_links_off"))
    );
    // 关掉之前发出、还没用掉的码：作废了（hub 照常撮合，但 daemon 不再回握手，加入方什么都得不到）
    let mut late = FakeWeb::connect_kind(
        &w.hub.url(),
        None,
        Some(&w.cookie),
        "Late twin",
        Keypair::generate(),
        None,
        DeviceKind::Twin,
    )
    .await;
    let attempt = tokio::time::timeout(Duration::from_secs(3), late.pair_handshake(&open_code.code)).await;
    assert!(attempt.is_err(), "the daemon does not answer a pairing handshake after accept_links went off");
    assert_eq!(w.td.ctx.peers.list().len(), 2, "only the phone and the existing twin device");
    // `twin.enabled` 是另一个键，互不影响
    assert!(w.td.ctx.config.get().twin.enabled);

    // 重新打开：照常可用，范围还在
    anycon_daemon::config::set_value(&w.td.paths, "twin.accept_links", "true").unwrap();
    w.td.ctx.config.reload().unwrap();
    twin.ik().await.expect("the handshake works again");
    let status: TwinPeerStatus = twin.call(methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert_eq!((status.projects, status.accept_links), (vec!["p".to_string()], true));
    w.td.shutdown().await;
}

#[tokio::test]
async fn twin_link_codes_and_pairing_are_for_people_only() {
    let w = world().await;
    let mut mcp = w.td.local_mcp_client();
    let e = mcp.call::<Value>(&w.td, methods::NODE_TWIN_LINK_CODE, json!({})).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
    for method in [methods::TWIN_LINK, methods::TWIN_LINKS, methods::TWIN_UNLINK] {
        let e = mcp.call::<Value>(&w.td, method, json!({ "code": "x", "node_id": "y" })).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{method}");
    }
    // 人调用：A 侧的处理在 tests/twin_links.rs 里；这里只看这个节点自己没有分身连接时的回答
    let mut human = w.td.local_cli_client();
    let e = human.call::<Value>(&w.td, methods::TWIN_LINK, json!({ "code": "x", "node_id": "y" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid), "a malformed code is a parameter error");
    let links: TwinLinksResult = human.call(&w.td, methods::TWIN_LINKS, json!({})).await.unwrap();
    assert!(links.links.is_empty());
    let e = human.call::<Value>(&w.td, methods::TWIN_UNLINK, json!({ "node_id": "y" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    w.td.shutdown().await;
}

#[tokio::test]
async fn revoking_a_twin_device_announces_it_to_the_other_devices_and_the_audit() {
    let w = world().await;
    let recorder = Arc::new(Recorder::default());
    w.td.ctx.notifier.set_out(recorder.clone());
    let twin = w.td.twin_device_client("twin-1", "A's twin").await;
    let mut human = w.td.local_cli_client();
    let mut web = w.td.hub_client("web-1", "Phone");
    human.call::<Empty>(&w.td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": "twin-1" })).await.unwrap();
    let ev = web.wait_notification(T, |n| n.method == methods::NODE_SECURITY_EVENT).await.expect("security event");
    assert_eq!(ev.params["kind"], "twin_unlinked");
    assert_eq!(ev.params["device_id"], "twin-1");
    assert!(recorder.0.lock().unwrap().iter().any(|b| b.category == NotifyCategory::Security));
    let r: NodeAuditResult = human.call(&w.td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    assert!(r.entries.iter().any(|e| e.action == "twin.unlinked" && e.target.as_deref() == Some("twin-1")));
    // 吊销一台普通设备不发这个事件
    drop(twin);
    w.td.shutdown().await;
}
