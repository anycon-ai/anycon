//! 守护测试：遍历 `methods::REGISTRY`，对每个请求方法用最小合法参数调用一次，把 daemon 的**实际响应**
//! 用 `schema/protocol.json` 里对应 `result` 类型的 JSON Schema 校验；daemon 实际发出的通知也按 `params` 类型校验。
//!
//! 注册表（`crates/protocol`）和实现之间以前漂移过：类型名对得上、字段对不上，TS / Python 按注册表生成的类型就是错的。
//! 这条测试让它再漂移时立刻红。
//!
//! **不能在这里调用的方法**列在 [`NOT_CALLED`]，每一项写明原因和别处的覆盖；清单本身也受检查（不能有注册表里没有的名字，
//! 也不能漏掉没调用又没列出的方法）。

use std::collections::BTreeSet;
use std::time::Duration;

use anycon_daemon::testutil::{FakeHubLink, TestClient, TestDaemon};
use anycon_protocol::methods::{self, Direction, REGISTRY};
use anycon_protocol::{DeviceKind, EnrollFrame, EnrollHint, EnrollMethod};
use jsonschema::Validator;
use serde_json::{json, Value};

mod common;
use common::Schemas;

const T: Duration = Duration::from_secs(10);

/// 没有在这里调用的请求方法，以及为什么。
const NOT_CALLED: &[(&str, &str)] = &[
    (
        methods::NODE_PAIR_BEGIN,
        "needs a hub connection; tests/hub_flow.rs validates its result against NodePairBeginResult (`first_pairing_issues_a_device_token_and_then_ik_rpc_works`)",
    ),
    (
        methods::NODE_REVOKE_DEVICE,
        "needs a paired device; its result is `Empty` like every other `result: None` method, and tests/hub_flow.rs covers the call",
    ),
    (
        methods::JOB_CANCEL,
        "needs a clone job that is still running; an offline test cannot keep one running (tests/socket.rs covers the NotFound path); its `Empty` result is validated through the other `result: None` methods",
    ),
];

struct Walk<'a> {
    td: &'a TestDaemon,
    c: TestClient,
    schemas: Schemas,
    called: BTreeSet<&'static str>,
}

impl<'a> Walk<'a> {
    fn new(td: &'a TestDaemon) -> Walk<'a> {
        Walk { td, c: td.client("schema"), schemas: Schemas::load(), called: BTreeSet::new() }
    }

    fn check(&mut self, ty: &str, value: &Value, what: &str) {
        self.schemas.check(ty, value, what);
    }

    fn validator(&mut self, ty: &str, what: &str) -> &Validator {
        self.schemas.validator(ty, what)
    }

    /// 调用 `method`，按注册表里它的 `result` 类型校验响应（没有 `result` 的是 `Empty`）。
    async fn call(&mut self, method: &'static str, params: Value) -> Value {
        let spec = REGISTRY.iter().find(|m| m.name == method).unwrap_or_else(|| panic!("{method} is not in REGISTRY"));
        assert_eq!(spec.direction, Direction::Request, "{method}");
        let v: Value = Box::pin(self.c.call(self.td, method, params))
            .await
            .unwrap_or_else(|e| panic!("{method} failed with minimal valid params: {e:?}"));
        self.check(spec.result.unwrap_or("Empty"), &v, method);
        self.called.insert(method);
        v
    }

    /// 取出到目前为止收到的通知，按注册表里它的 `params` 类型校验。返回出现过的通知方法。
    fn check_notifications(&mut self) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        for n in self.c.drain_now() {
            let spec = REGISTRY
                .iter()
                .find(|m| m.name == n.method)
                .unwrap_or_else(|| panic!("the daemon sent a notification that is not in REGISTRY: {}", n.method));
            assert_eq!(spec.direction, Direction::Notification, "{}", n.method);
            let ty = spec.params.expect("a notification has a params type");
            self.check(ty, &n.params, &format!("notification {}", n.method));
            seen.insert(n.method);
        }
        seen
    }
}

/// 账号内接入（ADR-36）：设置 / 开关 / 接入码 / 批准 / 拒绝；通知（enroll_requested、enroll_resolved、security_event）
/// 由调用方在最后按注册表校验。
async fn walk_enroll(w: &mut Walk<'_>, td: &TestDaemon) {
    w.call(methods::NODE_ENROLL_STATUS, json!({})).await;
    w.call(methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": "walk the registry" })).await;
    w.call(methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": true, "password": true })).await;
    w.call(methods::NODE_ENROLL_STATUS, json!({})).await;
    w.call(methods::NODE_ENROLL_CODE, json!({})).await;
    w.call(methods::NODE_ENROLL_LIST, json!({})).await;
    let link = FakeHubLink::attach(td);
    for (enroll_id, peer) in [("e1", "walk-peer-1"), ("e2", "walk-peer-2")] {
        link.request(td, enroll_id, peer).await;
        let hello = EnrollFrame::Hello {
            v: 1,
            method: EnrollMethod::Code,
            approval: true,
            nonce: "07".repeat(16),
            hint: EnrollHint { name: "Walker".into(), kind: DeviceKind::Cli, platform: "test".into() },
        };
        link.send_frame(td, peer, &hello).await;
    }
    let listed = w.call(methods::NODE_ENROLL_LIST, json!({})).await;
    assert_eq!(listed["enrolls"].as_array().unwrap().len(), 2);
    let approved = w.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": "e1" })).await;
    assert_eq!(approved["code"].as_str().map(str::len), Some(6));
    w.call(methods::NODE_ENROLL_DENY, json!({ "enroll_id": "e2" })).await;
    w.call(methods::NODE_SET_ENROLL_PASSWORD, json!({})).await;
}

async fn wait_idle(w: &mut Walk<'_>, sid: &str) {
    let t = std::time::Instant::now();
    loop {
        let s = w.call(methods::SESSION_GET, json!({ "session_id": sid })).await;
        if s["status"] == json!("idle") {
            return;
        }
        assert!(t.elapsed() < T, "the session never became idle: {s}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 下面每一段是独立的 async fn，调用处再 `Box::pin`：整条遍历写成一个 future 会在 debug 下撑爆测试线程的 2 MiB 栈，
/// 拆开并放到堆上，栈占用就只取决于最大的那一段，往注册表里加方法也不会再把它推过线。
async fn walk_workspace(w: &mut Walk<'_>, td: &TestDaemon) {
    // ---- workspace / fs / job
    let p = w.call(methods::WORKSPACE_CREATE, json!({ "name": "alpha" })).await;
    assert_eq!(p["name"], json!("alpha"));
    w.call(methods::WORKSPACE_LIST, json!({})).await;
    w.call(methods::WORKSPACE_RENAME, json!({ "from": "alpha", "to": "beta" })).await;
    let trashed = w.call(methods::WORKSPACE_TRASH, json!({ "name": "beta" })).await;
    w.call(methods::WORKSPACE_TRASH_LIST, json!({})).await;
    w.call(methods::WORKSPACE_RESTORE, json!({ "id": trashed["id"] })).await;
    let trashed = w.call(methods::WORKSPACE_TRASH, json!({ "name": "beta" })).await;
    w.call(methods::WORKSPACE_PURGE, json!({ "id": trashed["id"] })).await;
    w.call(methods::WORKSPACE_CREATE, json!({ "name": "proj" })).await;
    // clone：连不上的地址立刻起一个会失败的 job（离线可用），重点是 `JobStarted` 的形状
    w.call(methods::WORKSPACE_CLONE, json!({ "url": "ssh://git@127.0.0.1:1/x/y.git", "name": "cl" })).await;
    w.call(methods::FS_LIST, json!({ "path": "" })).await;
    let r = w.call(methods::FS_READ, json!({ "path": "hello.txt" })).await;
    assert_eq!(r["text"], json!("hello"));

    // ---- 关联已有目录（ADR-33）：link_roots 指向临时目录（热加载），不碰真实 HOME
    let machine = std::fs::canonicalize(td.dir.path()).unwrap().join("machine");
    std::fs::create_dir_all(machine.join("linked-proj")).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", &roots).unwrap();
    td.ctx.config.reload().unwrap();
    let b = w.call(methods::WORKSPACE_BROWSE, json!({})).await;
    assert_eq!(b["entries"][0]["name"], json!("linked-proj"));
    // 隐藏目录：`blocked: "hidden"` 也要符合 schema（BrowseEntry.blocked / WorkspaceBrowseResult.blocked）
    std::fs::create_dir_all(machine.join("linked-proj/.dot/inner")).unwrap();
    let b =
        w.call(methods::WORKSPACE_BROWSE, json!({ "path": machine.join("linked-proj"), "show_hidden": true })).await;
    assert_eq!(b["entries"][0]["blocked"], json!("hidden"));
    let b = w
        .call(methods::WORKSPACE_BROWSE, json!({ "path": machine.join("linked-proj/.dot"), "show_hidden": true }))
        .await;
    assert_eq!(b["blocked"], json!("hidden"), "the current directory is flagged too");
    let linked = w.call(methods::WORKSPACE_LINK, json!({ "path": machine.join("linked-proj") })).await;
    assert_eq!((linked["name"].as_str(), linked["kind"].as_str()), (Some("linked-proj"), Some("linked")));
    let l = w.call(methods::WORKSPACE_LIST, json!({})).await;
    assert!(l["projects"].as_array().unwrap().iter().any(|p| p["kind"] == json!("linked")));
    w.call(methods::WORKSPACE_UNLINK, json!({ "name": "linked-proj" })).await;
}

async fn walk_session(w: &mut Walk<'_>) {
    // ---- session
    let s = w
        .call(
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "hello", "message_id": "m1" }),
        )
        .await;
    let sid = s["id"].as_str().unwrap().to_string();
    wait_idle(w, &sid).await;
    w.call(methods::SESSION_LIST, json!({})).await;
    w.call(methods::SESSION_LIST, json!({ "project": "proj", "include_archived": true, "limit": 5 })).await;
    w.call(methods::SESSION_WATCH, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_SEND, json!({ "session_id": sid, "message_id": "m2", "content": "say hello again" })).await;
    wait_idle(w, &sid).await;
    // ---- blob（附件，ADR-35）：上传一张图、拉回来、丢掉一张没用的；带着附件发一条消息，下面的 history 里就有 `attachments`
    let png = anycon_daemon::testutil::png_fixture(64, 48, 1500, 1);
    let sha = anycon_daemon::testutil::sha256_hex(&png);
    let b64 = |b: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
    let begin = w
        .call(
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "walk.png", "mime": "image/png", "size": png.len(), "sha256": sha }),
        )
        .await;
    let blob_id = begin["blob_id"].as_str().unwrap().to_string();
    w.call(methods::BLOB_PUT_CHUNK, json!({ "blob_id": blob_id, "index": 0, "data": b64(&png) })).await;
    let att = w.call(methods::BLOB_PUT_END, json!({ "blob_id": blob_id })).await;
    assert_eq!((att["width"].clone(), att["height"].clone()), (json!(64), json!(48)));
    let got = w.call(methods::BLOB_GET, json!({ "blob_id": blob_id })).await;
    assert_eq!((got["eof"].clone(), got["data"].clone()), (json!(true), json!(b64(&png))));
    let spare = w
        .call(
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "spare.png", "mime": "image/png", "size": png.len(), "sha256": sha }),
        )
        .await;
    w.call(methods::BLOB_DISCARD, json!({ "blob_id": spare["blob_id"] })).await;
    w.call(
        methods::SESSION_SEND,
        json!({ "session_id": sid, "message_id": "m2-img", "content": "", "attachments": [att] }),
    )
    .await;
    wait_idle(w, &sid).await;
    w.call(methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 100 })).await;
    w.call(methods::SESSION_SET_SETTINGS, json!({ "session_id": sid, "settings": { "model": "fake-small" } })).await;
    w.call(methods::SESSION_RENAME, json!({ "session_id": sid, "title": "A title" })).await;
    w.call(methods::SESSION_SEARCH, json!({ "query": "hello" })).await;
    w.call(methods::SESSION_UNWATCH, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_CANCEL, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_RESTART, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_ARCHIVE, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_UNARCHIVE, json!({ "session_id": sid })).await;
    w.call(methods::SESSION_DELETE, json!({ "session_id": sid })).await;
    w.call(methods::NODE_AUDIT, json!({ "limit": 50 })).await;
    // 草稿会话（没有 project）升格成项目
    let scratch = w.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await;
    let scratch_id = scratch["id"].as_str().unwrap().to_string();
    w.call(methods::SESSION_PROMOTE, json!({ "session_id": scratch_id, "name": "promoted" })).await;
}

async fn walk_worktree(w: &mut Walk<'_>, td: &TestDaemon) {
    // ---- worktree / git（ADR-34）：真实 git 仓库项目、假 gh
    let repo = td.new_repo("gitproj");
    let remote = td.dir.path().join("remote.git");
    td.git(td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    td.git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let br = w.call(methods::WORKSPACE_BRANCHES, json!({ "project": "gitproj" })).await;
    assert_eq!(br["default_base"], json!("main"));
    let ws = w
        .call(
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "gitproj", "prompt": "worktree walk", "message_id": "m-wt",
                    "workspace": { "mode": "worktree", "branch": "walk/branch" } }),
        )
        .await;
    assert_eq!(ws["worktree"]["state"], json!("creating"));
    let wsid = ws["id"].as_str().unwrap().to_string();
    wait_idle(w, &wsid).await;
    let wt_dir = repo.join(".anycon/worktrees/branch");
    std::fs::write(wt_dir.join("new.txt"), "x\ny\n").unwrap();
    // 假 `gh` 是 sh 脚本，Windows 上没有（testutil 的 git 夹具），那里 `pr` 为空
    #[cfg(unix)]
    td.set_gh(
        r#"{"number":3,"url":"https://example.test/pr/3","title":"t","state":"OPEN","isDraft":true,"baseRefName":"main","statusCheckRollup":[]}"#,
        "",
        0,
    );
    let st = w.call(methods::SESSION_GIT_STATUS, json!({ "session_id": wsid, "include_pr": true })).await;
    if cfg!(unix) {
        assert_eq!(st["pr"]["state"], json!("draft"));
    }
    assert_eq!(st["uncommitted"], json!(1));
    w.call(methods::SESSION_DIFF, json!({ "session_id": wsid })).await;
    let d =
        w.call(methods::SESSION_DIFF, json!({ "session_id": wsid, "against": "uncommitted", "path": "new.txt" })).await;
    assert!(d["files"][0]["patch"].as_str().unwrap().contains("+x"));
    td.git(&wt_dir, &["add", "-A"]);
    td.git(&wt_dir, &["commit", "-q", "-m", "walk"]);
    let sy = w.call(methods::SESSION_SYNC_BASE, json!({ "session_id": wsid })).await;
    assert_eq!(sy["outcome"], json!("up_to_date"));
    let wl = w.call(methods::WORKSPACE_WORKTREES, json!({ "project": "gitproj" })).await;
    assert_eq!(wl["worktrees"][0]["branch"], json!("walk/branch"));
    w.call(methods::SESSION_ARCHIVE, json!({ "session_id": wsid, "worktree": "keep" })).await;
    w.call(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": wl["worktrees"][0]["id"], "delete_branch": true })).await;
    w.call(methods::SESSION_UNARCHIVE, json!({ "session_id": wsid })).await;
    w.call(methods::SESSION_DELETE, json!({ "session_id": wsid, "worktree": "remove", "delete_branch": true })).await;
}

async fn walk_permission(w: &mut Walk<'_>) -> String {
    // ---- permission：让假 agent 发一个权限请求，列出，答复
    let ps = w
        .call(
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "run a permission", "message_id": "m3" }),
        )
        .await;
    let psid = ps["id"].as_str().unwrap().to_string();
    let t = std::time::Instant::now();
    let req = loop {
        let l = w.call(methods::PERMISSION_LIST, json!({ "session_id": psid })).await;
        if let Some(r) = l["requests"].as_array().and_then(|a| a.first()) {
            break r.clone();
        }
        assert!(t.elapsed() < T, "no permission request appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    w.call(methods::PERMISSION_LIST, json!({})).await;
    w.call(methods::PERMISSION_RESPOND, json!({ "request_id": req["request_id"], "option_id": "allow" })).await;
    wait_idle(w, &psid).await;
    psid
}

async fn walk_preview(w: &mut Walk<'_>, td: &TestDaemon, psid: &str) {
    // ---- 预览（ADR-39）：端口写进 allow_ports（热加载），签发、列出、列端口、作废
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let preview_port = listener.local_addr().unwrap().port();
    anycon_daemon::config::set_value(&td.paths, "preview.allow_ports", &format!("[{preview_port}]")).unwrap();
    td.ctx.config.reload().unwrap();
    let pt = w.call(methods::PREVIEW_OPEN, json!({ "port": preview_port, "session_id": psid })).await;
    assert_eq!(pt["port"], json!(preview_port));
    let pl = w.call(methods::PREVIEW_LIST, json!({})).await;
    assert_eq!(pl["previews"][0]["label"], pt["label"]);
    let pp = w.call(methods::PREVIEW_PORTS, json!({})).await;
    assert!(pp["ports"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["port"] == json!(preview_port) && p["source"] == json!("config")));
    w.call(methods::PREVIEW_CLOSE, json!({ "label": pt["label"] })).await;
    drop(listener);
}

#[tokio::test]
async fn every_request_in_the_registry_answers_with_its_declared_result_type() {
    // 带 git 夹具：worktree / git 的请求（ADR-34）也要走一遍
    let td = TestDaemon::start_git(&[]).await;
    std::fs::write(td.workspace.join("hello.txt"), "hello").unwrap();
    let mut w = Walk::new(&td);

    // ---- node / agent
    w.call(methods::NODE_INFO, json!({})).await;
    w.call(methods::NODE_DEVICES, json!({})).await;
    w.call(methods::NODE_STATS, json!({})).await;
    w.call(methods::NODE_DOCTOR, json!({})).await;
    w.call(methods::AGENT_LIST, json!({})).await;
    w.call(methods::AGENT_LIST, json!({ "refresh": true })).await;
    w.call(methods::AGENT_DIAGNOSE, json!({ "id": "fake" })).await;
    w.call(methods::NODE_SET_AGENT_DEFAULTS, json!({ "agent": "fake", "mode": "yolo" })).await;

    // ---- 账号内接入（ADR-36）：单独一个函数（这个遍历测试的 future 已经很大，再往里塞会在 debug 下把栈撑爆）
    Box::pin(walk_enroll(&mut w, &td)).await;

    Box::pin(walk_workspace(&mut w, &td)).await;
    Box::pin(walk_session(&mut w)).await;
    Box::pin(walk_worktree(&mut w, &td)).await;
    let psid = Box::pin(walk_permission(&mut w)).await;
    Box::pin(walk_preview(&mut w, &td, &psid)).await;

    // ---- 每个请求方法要么调用过，要么在清单里写明了原因
    let not_called: BTreeSet<&str> = NOT_CALLED.iter().map(|(m, _)| *m).collect();
    let registry_requests: BTreeSet<&str> =
        REGISTRY.iter().filter(|m| m.direction == Direction::Request).map(|m| m.name).collect();
    for m in &not_called {
        assert!(registry_requests.contains(m), "NOT_CALLED names a method that is not a registry request: {m}");
        assert!(!w.called.contains(m), "{m} is called here after all; take it off NOT_CALLED");
    }
    for m in &registry_requests {
        assert!(
            w.called.contains(m) || not_called.contains(m),
            "{m} is neither called by this test nor listed in NOT_CALLED with a reason"
        );
    }
    // ---- daemon 实际发出的通知也按注册表里的 params 类型校验
    let seen = w.check_notifications();
    for want in [
        methods::SESSION_EVENT,
        methods::SESSION_CHANGED,
        methods::PERMISSION_REQUEST,
        methods::PERMISSION_RESOLVED,
        methods::NODE_ENROLL_REQUESTED,
        methods::NODE_ENROLL_RESOLVED,
        methods::NODE_SECURITY_EVENT,
    ] {
        assert!(seen.contains(want), "the walk should have produced {want}: {seen:?}");
    }
    td.shutdown().await;
}

/// 校验器本身是认真的：形状漂移的响应必须被抓到（否则上面的测试永远绿，什么也没守住）。
#[tokio::test]
async fn the_validator_rejects_a_response_that_drifted_from_its_type() {
    let td = TestDaemon::start().await;
    let mut w = Walk::new(&td);
    let info = w.call(methods::NODE_INFO, json!({})).await;
    assert!(w.validator("NodeInfo", "x").is_valid(&info));
    let mut renamed = info.clone();
    let v = renamed.as_object_mut().unwrap().remove("node_id").unwrap();
    renamed["nodeId"] = v;
    assert!(!w.validator("NodeInfo", "x").is_valid(&renamed), "a renamed field");
    let mut wrong_type = info.clone();
    wrong_type["protocol_version"] = json!("1");
    assert!(!w.validator("NodeInfo", "x").is_valid(&wrong_type), "a field of the wrong type");
    // 嵌套的 `$ref`（capabilities）也解析得到
    let mut nested = info;
    nested["capabilities"] = json!({ "unexpected": 1 });
    assert!(!w.validator("NodeInfo", "x").is_valid(&nested), "a nested type through $ref");
    assert!(!w.validator("Empty", "x").is_valid(&Value::Null), "null is not an Empty result");
    td.shutdown().await;
}
