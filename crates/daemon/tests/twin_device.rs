//! 被使用的节点（B）这一侧：分身设备（`DeviceKind::Twin`，另一个节点的分身）的方法白名单、推送过滤、归属、范围和名额
//! （docs/spec/daemon.md § 27.3）。
//!
//! 白名单和推送过滤是安全边界：下面两条遍历测试按方法注册表逐个核对，**新增的请求方法或通知没有在这里分类就会失败**，
//! 逼着加它的人决定它进不进名单。

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_daemon::twin_device::{self, Scope};
use anycon_protocol::methods::{self, Direction, REGISTRY};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);

/// 分身设备能调的方法，和每个方法对归属的要求。要和 `twin_device::ALLOWED` 一模一样。
const ALLOWED: &[(&str, Scope)] = &[
    (methods::NODE_INFO, Scope::Open),
    (methods::AGENT_LIST, Scope::Open),
    (methods::TWIN_PEER_STATUS, Scope::Open),
    (methods::TODO_ADD, Scope::NewTodo),
    (methods::TODO_GET, Scope::OwnTodo),
    (methods::TODO_LIST, Scope::OwnTodoList),
    (methods::TODO_HISTORY, Scope::OwnTodo),
    (methods::TODO_DISPATCH, Scope::OwnTodo),
    (methods::SESSION_GET, Scope::OwnSession),
    (methods::SESSION_HISTORY, Scope::OwnSession),
    (methods::SESSION_WATCH, Scope::OwnSession),
    (methods::SESSION_UNWATCH, Scope::OwnSession),
    (methods::SESSION_CANCEL, Scope::OwnSession),
    (methods::SESSION_DIFF, Scope::OwnSession),
];

/// 分身设备**不能**调的请求方法（每个都要被 `forbidden`，`data.reason = "twin_device"`）。注册表里新增了请求方法而这里没有，
/// 测试失败：先决定它进不进 `ALLOWED`，进了要写清它只碰什么、加一条归属检查；不进就加到这里。
const DENIED: &[&str] = &[
    methods::NODE_DEVICES,
    methods::NODE_REVOKE_DEVICE,
    methods::NODE_PAIR_BEGIN,
    methods::NODE_STATS,
    methods::NODE_DOCTOR,
    methods::NODE_AUDIT,
    methods::NODE_SET_AGENT_DEFAULTS,
    methods::NODE_TWIN_LINK_CODE,
    methods::NODE_GET_CONFIG,
    methods::NODE_SET_CONFIG,
    methods::NODE_LOGS,
    methods::NODE_ENROLL_LIST,
    methods::NODE_ENROLL_APPROVE,
    methods::NODE_ENROLL_DENY,
    methods::NODE_ENROLL_CODE,
    methods::NODE_ENROLL_STATUS,
    methods::NODE_SET_ENROLL_PASSWORD,
    methods::NODE_SET_ENROLL_ENABLED,
    methods::AGENT_DIAGNOSE,
    methods::WORKSPACE_LIST,
    methods::WORKSPACE_CREATE,
    methods::WORKSPACE_CLONE,
    methods::WORKSPACE_RENAME,
    methods::WORKSPACE_TRASH,
    methods::WORKSPACE_TRASH_LIST,
    methods::WORKSPACE_RESTORE,
    methods::WORKSPACE_PURGE,
    methods::WORKSPACE_LINK,
    methods::WORKSPACE_UNLINK,
    methods::WORKSPACE_BROWSE,
    methods::WORKSPACE_BRANCHES,
    methods::WORKSPACE_WORKTREES,
    methods::WORKSPACE_REMOVE_WORKTREE,
    methods::JOB_CANCEL,
    methods::FS_LIST,
    methods::FS_READ,
    methods::SESSION_LIST,
    methods::SESSION_CREATE,
    methods::SESSION_SEND,
    methods::SESSION_SET_SETTINGS,
    methods::SESSION_RENAME,
    methods::SESSION_ARCHIVE,
    methods::SESSION_UNARCHIVE,
    methods::SESSION_SET_MARKS,
    methods::SESSION_MARK_READ,
    methods::SESSION_DELETE,
    methods::SESSION_RESTART,
    methods::SESSION_SEARCH,
    methods::SESSION_PROMOTE,
    methods::SESSION_GIT_STATUS,
    methods::SESSION_SYNC_BASE,
    methods::TODO_UPDATE,
    methods::TODO_DELETE,
    methods::TODO_DISPATCH_PREVIEW,
    methods::RULE_LIST,
    methods::RULE_ADD,
    methods::RULE_UPDATE,
    methods::RULE_DELETE,
    methods::RULE_PREVIEW,
    methods::RULE_EXTRACT,
    methods::RULE_EXTRACT_RUNS,
    methods::TWIN_GET,
    methods::TWIN_SET,
    methods::TWIN_WAKE,
    methods::TWIN_DRY_RUN,
    methods::TWIN_RUNS,
    methods::TWIN_QUESTIONS,
    methods::TWIN_ANSWER,
    methods::TWIN_LINK,
    methods::TWIN_LINKS,
    methods::TWIN_UNLINK,
    methods::BLOB_PUT_BEGIN,
    methods::BLOB_PUT_CHUNK,
    methods::BLOB_PUT_END,
    methods::BLOB_GET,
    methods::BLOB_DISCARD,
    methods::PERMISSION_RESPOND,
    methods::PERMISSION_LIST,
    methods::PREVIEW_OPEN,
    methods::PREVIEW_CLOSE,
    methods::PREVIEW_LIST,
    methods::PREVIEW_PORTS,
];

/// 推给分身设备的通知：只有这四种，并且只限它自己的项和会话。
const PUSHED: &[&str] = &[methods::TODO_CHANGED, methods::TODO_EVENT, methods::SESSION_CHANGED, methods::SESSION_EVENT];

/// **不**推给分身设备的通知。注册表里新增了通知而这里没有，测试失败：默认不推，要推得先改 `twin_device::PUSHED` 和归属判断。
const NEVER_PUSHED: &[&str] = &[
    methods::PAIR_ACK,
    methods::NODE_ENROLL_REQUESTED,
    methods::NODE_ENROLL_RESOLVED,
    methods::NODE_SECURITY_EVENT,
    methods::AGENT_CHANGED,
    methods::WORKSPACE_CHANGED,
    methods::JOB_PROGRESS,
    methods::JOB_DONE,
    methods::RULE_CHANGED,
    methods::RULE_EXTRACT_DONE,
    methods::TWIN_CHANGED,
    methods::TWIN_QUESTION,
    methods::PERMISSION_REQUEST,
    methods::PERMISSION_RESOLVED,
];

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn reason(e: &RpcError) -> Option<String> {
    e.data.as_ref().and_then(|d| d.reason.clone())
}

fn is_twin_device_forbidden(e: &RpcError) -> bool {
    e.stable_code() == Some(ErrorCode::Forbidden) && reason(e).as_deref() == Some("twin_device")
}

struct Fixture {
    td: TestDaemon,
    /// 这个节点的人（本机 CLI）。
    human: TestClient,
    /// 分身设备 `twin-a`（范围里开放了项目 `p`）。
    a: TestClient,
    /// 另一台分身设备 `twin-b`。
    b: TestClient,
}

/// 两个 git 项目 `p`、`q`，`twin-a` 的范围只开放 `p`；`twin-b` 什么都没开放。
async fn fixture() -> Fixture {
    let td = TestDaemon::start_git(&[]).await;
    td.fake.declare_no_push.store(true, Ordering::SeqCst);
    td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    td.new_repo("p");
    td.new_repo("q");
    let mut human = td.local_cli_client();
    let a = td.twin_device_client("twin-a", "A's twin").await;
    let b = td.twin_device_client("twin-b", "B's twin").await;
    let r: TwinGetResult = human
        .call(
            &td,
            methods::TWIN_SET,
            json!({
                "requester": "twin-a", "projects": ["p"], "agent": "fake", "max_sessions": 2, "daily_dispatches": 3
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.scope.projects, vec!["p".to_string()]);
    Fixture { td, human, a, b }
}

async fn add(td: &TestDaemon, c: &mut TestClient, title: &str, project: &str) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_ADD, json!({ "id": uuid(), "title": title, "project": project })).await
}

async fn dispatch(td: &TestDaemon, c: &mut TestClient, todo: &str) -> Result<TodoDispatchResult, RpcError> {
    c.call(td, methods::TODO_DISPATCH, json!({ "id": todo, "agent": "" })).await
}

// ====================================================================== 方法白名单

#[test]
fn the_whitelist_table_here_is_the_one_the_daemon_enforces() {
    let show = |t: &[(&str, Scope)]| t.iter().map(|(m, k)| format!("{m}={k:?}")).collect::<BTreeSet<_>>();
    assert_eq!(show(twin_device::ALLOWED), show(ALLOWED), "this table and the daemon's differ");
}

#[test]
fn every_request_method_is_classified_exactly_once() {
    let requests: BTreeSet<&str> =
        REGISTRY.iter().filter(|m| m.direction == Direction::Request).map(|m| m.name).collect();
    let allowed: BTreeSet<&str> = ALLOWED.iter().map(|(m, _)| *m).collect();
    let denied: BTreeSet<&str> = DENIED.iter().copied().collect();
    assert_eq!(allowed.len(), ALLOWED.len(), "a method is listed twice in ALLOWED");
    assert_eq!(denied.len(), DENIED.len(), "a method is listed twice in DENIED");
    assert!(allowed.is_disjoint(&denied), "a method is both allowed and denied: {:?}", allowed.intersection(&denied));
    let classified: BTreeSet<&str> = allowed.union(&denied).copied().collect();
    let unclassified: Vec<_> = requests.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "these request methods are neither in the twin device whitelist nor in the denied list; \
         decide which: {unclassified:?}"
    );
    let unknown: Vec<_> = classified.difference(&requests).collect();
    assert!(unknown.is_empty(), "these names are not request methods in the registry: {unknown:?}");
    // 方法名之外的东西也不在名单里
    for m in ["", "local.status", "local.export", "no.such_method", "session.list "] {
        assert!(twin_device::scope_of(m).is_none(), "{m:?}");
    }
}

#[tokio::test]
async fn every_method_outside_the_whitelist_is_forbidden_to_a_twin_device_before_anything_else_happens() {
    let mut f = fixture().await;
    // 名单之外的（包括根本不存在的方法名和只给本机用的 `local.*`）一律 `forbidden`，`data.reason = "twin_device"`
    let mut names: Vec<&str> = DENIED.to_vec();
    names.extend(["local.status", "local.rename_device", "local.export", "local.set_client", "no.such_method", ""]);
    for m in names {
        for params in [json!({}), json!({ "id": "x", "session_id": "y", "name": "z" })] {
            let e = f.a.call::<Value>(&f.td, m, params).await.expect_err(m);
            assert!(is_twin_device_forbidden(&e), "{m}: {e:?}");
        }
    }
    // 没有任何副作用：节点上没有多出项目、规则、会话
    assert_eq!(
        f.human.call::<SessionListResult>(&f.td, methods::SESSION_LIST, json!({})).await.unwrap().sessions.len(),
        0
    );
}

#[tokio::test]
async fn whitelisted_methods_work_on_its_own_items_and_sessions_and_are_forbidden_on_anyone_elses() {
    let mut f = fixture().await;
    // 自己的项和会话
    let mine = add(&f.td, &mut f.a, "mine", "p").await.unwrap();
    let r = dispatch(&f.td, &mut f.a, &mine.id).await.unwrap();
    let sid = r.session.id.clone();
    // 别人的：人记的项、另一台分身设备记的项、人开的会话
    let humans = add(&f.td, &mut f.human, "the human's", "p").await.unwrap();
    f.human
        .call::<TwinGetResult>(
            &f.td,
            methods::TWIN_SET,
            json!({ "requester": "twin-b", "projects": ["p"], "agent": "fake" }),
        )
        .await
        .unwrap();
    let others = add(&f.td, &mut f.b, "b's", "p").await.unwrap();
    let hs: SessionSummary = f
        .human
        .call(
            &f.td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "p", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let b_sid = dispatch(&f.td, &mut f.b, &others.id).await.unwrap().session.id;

    for (method, scope) in ALLOWED {
        let (own, foreign): (Vec<Value>, Vec<Value>) = match scope {
            Scope::Open | Scope::OwnTodoList => continue,
            Scope::NewTodo => (vec![], vec![json!({ "id": humans.id, "title": "x", "project": "p" })]),
            Scope::OwnTodo => (
                vec![json!({ "id": mine.id })],
                vec![json!({ "id": humans.id }), json!({ "id": others.id }), json!({ "id": uuid() })],
            ),
            Scope::OwnSession => (
                vec![json!({ "session_id": sid })],
                vec![
                    json!({ "session_id": hs.id }),
                    json!({ "session_id": b_sid }),
                    json!({ "session_id": "sess_nonexistent" }),
                ],
            ),
        };
        for p in foreign {
            let e = f.a.call::<Value>(&f.td, method, p.clone()).await.expect_err(method);
            assert!(is_twin_device_forbidden(&e), "{method} {p}: {e:?}");
        }
        for p in own {
            // `todo.dispatch` 再派一次要名额，下面单独测；这里只要没有被拒绝
            let res = f.a.call::<Value>(&f.td, method, p.clone()).await;
            if let Err(e) = res {
                assert!(!is_twin_device_forbidden(&e), "{method} {p} on its own object: {e:?}");
                assert_ne!(e.stable_code(), Some(ErrorCode::Forbidden), "{method} {p}: {e:?}");
            }
        }
    }

    // `forbidden` 先于 `not_found`：别人的项和根本不存在的项，答复一字不差，探测不出存在与否
    for method in [methods::TODO_GET, methods::TODO_HISTORY, methods::TODO_DISPATCH] {
        let existing = f.a.call::<Value>(&f.td, method, json!({ "id": humans.id })).await.unwrap_err();
        let missing = f.a.call::<Value>(&f.td, method, json!({ "id": uuid() })).await.unwrap_err();
        assert_eq!((existing.code, existing.message), (missing.code, missing.message), "{method}");
    }
    for method in [methods::SESSION_GET, methods::SESSION_HISTORY, methods::SESSION_WATCH, methods::SESSION_CANCEL] {
        let existing = f.a.call::<Value>(&f.td, method, json!({ "session_id": hs.id })).await.unwrap_err();
        let missing = f.a.call::<Value>(&f.td, method, json!({ "session_id": "sess_nonexistent" })).await.unwrap_err();
        assert_eq!((existing.code, existing.message), (missing.code, missing.message), "{method}");
    }

    // 它自己的项和会话读得到，也看得到改动
    let got: Todo = f.a.call(&f.td, methods::TODO_GET, json!({ "id": mine.id })).await.unwrap();
    assert_eq!(got.sessions, vec![sid.clone()]);
    let s: SessionSummary = f.a.call(&f.td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
    assert_eq!(s.todo_id.as_deref(), Some(mine.id.as_str()));
    let history: TodoHistoryResult = f.a.call(&f.td, methods::TODO_HISTORY, json!({ "id": mine.id })).await.unwrap();
    assert!(history.events.iter().any(|e| matches!(e.data, TodoEventData::Dispatched { .. })));
    let diff: SessionDiffResult = f.a.call(&f.td, methods::SESSION_DIFF, json!({ "session_id": sid })).await.unwrap();
    assert!(diff.files.is_empty());
    f.a.call::<Empty>(&f.td, methods::SESSION_WATCH, json!({ "session_id": sid })).await.unwrap();
    f.a.call::<Empty>(&f.td, methods::SESSION_UNWATCH, json!({ "session_id": sid })).await.unwrap();
    f.td.shutdown().await;
}

#[tokio::test]
async fn todo_list_only_shows_the_items_the_device_added() {
    let mut f = fixture().await;
    f.human
        .call::<TwinGetResult>(
            &f.td,
            methods::TWIN_SET,
            json!({ "requester": "twin-b", "projects": ["p"], "agent": "fake" }),
        )
        .await
        .unwrap();
    let mine = add(&f.td, &mut f.a, "mine", "p").await.unwrap();
    add(&f.td, &mut f.b, "b's", "p").await.unwrap();
    add(&f.td, &mut f.human, "human's", "q").await.unwrap();
    let list: TodoListResult = f.a.call(&f.td, methods::TODO_LIST, json!({ "include_closed": true })).await.unwrap();
    assert_eq!(list.todos.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![mine.id.clone()]);
    // 项目过滤、翻页不会漏出别人的
    let list: TodoListResult = f.a.call(&f.td, methods::TODO_LIST, json!({ "project": "q" })).await.unwrap();
    assert!(list.todos.is_empty());
    // 人看得到全部
    let all: TodoListResult = f.human.call(&f.td, methods::TODO_LIST, json!({})).await.unwrap();
    assert_eq!(all.todos.len(), 3);
    f.td.shutdown().await;
}

// ====================================================================== 记一项

#[tokio::test]
async fn an_item_from_a_twin_device_needs_a_project_in_its_scope_and_is_recorded_as_its_own() {
    let mut f = fixture().await;
    // 项目不在这条连接开放的范围里：`forbidden` / `out_of_scope`；`twin-b` 什么都没开放
    let e = add(&f.td, &mut f.a, "x", "q").await.unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Forbidden), Some("out_of_scope")));
    let e = add(&f.td, &mut f.b, "x", "p").await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("out_of_scope"));
    // 没有项目：不是「没有范围限制」
    let e = f.a.call::<Todo>(&f.td, methods::TODO_ADD, json!({ "id": uuid(), "title": "x" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    // 项目名大小写等写法按项目的同名规则比较，和本机分身一致
    let t = add(&f.td, &mut f.a, "do it", "p").await.unwrap();
    assert_eq!(t.source.kind, TodoSourceKind::Twin);
    assert_eq!(t.source.device_id.as_deref(), Some("twin-a"));
    assert_eq!((t.handler, t.unreviewed, t.state, t.repeat.is_none()), (TodoHandler::Me, false, TodoState::Open, true));
    assert!(t.remote.is_empty());
    // 不能带周期规则、不能交给本机的分身
    for extra in [json!({ "handler": "twin" }), json!({ "repeat": { "kind": "daily", "at": "09:00" } })] {
        let mut p = json!({ "id": uuid(), "title": "x", "project": "p" });
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        let e = f.a.call::<Todo>(&f.td, methods::TODO_ADD, p).await.unwrap_err();
        assert!(is_twin_device_forbidden(&e), "{e:?}");
    }
    // `agent: true` 不会让它的项变成「agent 记的、等人收下」：来源就是那台设备
    let t2: Todo =
        f.a.call(
            &f.td,
            methods::TODO_ADD,
            json!({ "id": uuid(), "title": "agent?", "project": "p", "agent": true, "origin_session": "sess_x" }),
        )
        .await
        .unwrap();
    assert_eq!((t2.source.kind, t2.unreviewed, t2.source.session_id.is_none()), (TodoSourceKind::Twin, false, true));
    // 幂等：自己的 id 原样返回，别人的 id 不能借它读到
    let again: Todo =
        f.a.call(&f.td, methods::TODO_ADD, json!({ "id": t.id, "title": "other", "project": "p" })).await.unwrap();
    assert_eq!(again, t);
    let humans = add(&f.td, &mut f.human, "private", "p").await.unwrap();
    let e =
        f.a.call::<Todo>(&f.td, methods::TODO_ADD, json!({ "id": humans.id, "title": "x", "project": "p" }))
            .await
            .unwrap_err();
    assert!(is_twin_device_forbidden(&e));
    // 审计：来源是分身设备
    let audit: NodeAuditResult = f.human.call(&f.td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    let entry = audit.entries.iter().find(|e| e.action == "todo.add" && e.device_id == "twin-a").expect("audited");
    assert_eq!(entry.detail["source_kind"], "twin");
    assert!(!entry.detail.to_string().contains("do it"), "titles stay out of the audit");
    f.td.shutdown().await;
}

#[tokio::test]
async fn this_nodes_own_twin_does_not_touch_items_from_a_twin_device() {
    let mut f = fixture().await;
    let t = add(&f.td, &mut f.a, "from the other node", "p").await.unwrap();
    // 人也不能把它交给本机的分身：那只会得到一项没人处理的「处理人 = 分身」
    let e =
        f.human.call::<Todo>(&f.td, methods::TODO_UPDATE, json!({ "id": t.id, "handler": "twin" })).await.unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Invalid), Some("twin_source")));
    // 就算库里是这样（比如更早的版本写的），本机分身的范围也不收它
    sqlx::query("UPDATE todos SET handler = 'twin' WHERE id = ?")
        .bind(&t.id)
        .execute(f.td.ctx.store.pool())
        .await
        .unwrap();
    assert!(f.td.ctx.store.twin_todos().await.unwrap().is_empty());
    assert!(f.td.ctx.store.twin_pr_targets().await.unwrap().is_empty());
    f.td.shutdown().await;
}

// ====================================================================== 派发

#[tokio::test]
async fn a_dispatch_from_a_twin_device_follows_the_scope_of_its_own_connection() {
    let mut f = fixture().await;
    let one = add(&f.td, &mut f.a, "hold one", "p").await.unwrap();
    // 请求里带的字段只能落在范围允许的值里，否则 `invalid`
    for (extra, field) in [
        (json!({ "agent": "someone-else" }), "agent"),
        (json!({ "no_push": false }), "no_push"),
        (json!({ "settings": { "model": "other-model" } }), "settings"),
        (json!({ "settings": { "thinking": "high" } }), "settings"),
        (json!({ "settings": { "env": { "A": "1" } } }), "settings"),
        (json!({ "workspace": { "mode": "local" } }), "workspace"),
        (json!({ "workspace": { "mode": "worktree", "branch": "x/y" } }), "workspace"),
    ] {
        let mut p = json!({ "id": one.id, "agent": "" });
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        let e = f.a.call::<Value>(&f.td, methods::TODO_DISPATCH, p.clone()).await.expect_err(&p.to_string());
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid), "{p}: {e:?}");
        assert_eq!(e.data.as_ref().and_then(|d| d.field.clone()).as_deref(), Some(field), "{p}");
    }
    assert!(f.td.fake.handle_count() == 0, "nothing was started by a refused dispatch");
    // 项目：请求里指向范围之外的项目 → forbidden / out_of_scope
    let e =
        f.a.call::<Value>(&f.td, methods::TODO_DISPATCH, json!({ "id": one.id, "agent": "", "project": "q" }))
            .await
            .unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Forbidden), Some("out_of_scope")));
    // 范围收紧之后，已经记下的项也派不出去
    f.human
        .call::<TwinGetResult>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-a", "projects": [] }))
        .await
        .unwrap();
    let e = dispatch(&f.td, &mut f.a, &one.id).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("out_of_scope"));
    f.human
        .call::<TwinGetResult>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-a", "projects": ["p"] }))
        .await
        .unwrap();

    // 暂停：`invalid` / `paused`
    f.human
        .call::<TwinGetResult>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-a", "paused": true }))
        .await
        .unwrap();
    let e = dispatch(&f.td, &mut f.a, &one.id).await.unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Invalid), Some("paused")));
    f.human
        .call::<TwinGetResult>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-a", "paused": false }))
        .await
        .unwrap();

    // 成功：agent、模式由范围定，`no_push` 恒为 true，worktree 恒开；会话记在这台设备名下
    let r = dispatch(&f.td, &mut f.a, &one.id).await.unwrap();
    assert_eq!(r.session.agent, "fake");
    assert_eq!(r.session.settings.options.get("no_push"), Some(&json!(true)));
    assert!(r.session.worktree.is_some(), "a twin device's dispatch always gets its own worktree");
    assert_eq!(f.td.ctx.store.twin_session_owner(&r.session.id).await.unwrap().as_deref(), Some("twin-a"));
    assert_eq!(r.todo.source.device_id.as_deref(), Some("twin-a"));
    let audit: NodeAuditResult = f.human.call(&f.td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    assert!(audit.entries.iter().any(|e| e.action == "todo.dispatch" && e.device_id == "twin-a"));

    // 名额：这条连接派出的、还在处理中的会话 ≤ `max_sessions`
    let two = add(&f.td, &mut f.a, "hold two", "p").await.unwrap();
    let r2 = dispatch(&f.td, &mut f.a, &two.id).await.unwrap();
    let three = add(&f.td, &mut f.a, "hold three", "p").await.unwrap();
    let e = dispatch(&f.td, &mut f.a, &three.id).await.unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Invalid), Some("no_capacity")));
    // 每个本地日最多 `daily_dispatches` 次（和已经在处理中的无关）：放走一个会话、名额空出来，第四次超过每日次数
    wait_handles(&f.td, 2).await;
    for i in 0..2 {
        f.td.fake.handle(i).release();
    }
    for sid in [&r.session.id, &r2.session.id] {
        wait_not_working(&f.td, &mut f.a, sid).await;
    }
    let r3 = dispatch(&f.td, &mut f.a, &three.id).await.unwrap();
    wait_handles(&f.td, 3).await;
    f.td.fake.handle(2).release();
    wait_not_working(&f.td, &mut f.a, &r3.session.id).await;
    let four = add(&f.td, &mut f.a, "four", "p").await.unwrap();
    let e = dispatch(&f.td, &mut f.a, &four.id).await.unwrap_err();
    assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Invalid), Some("budget")));
    f.td.shutdown().await;
}

/// 派发返回时 agent 进程还不一定拉起来了：等到有这么多个 handle 再放行。
async fn wait_handles(td: &TestDaemon, n: usize) {
    let start = std::time::Instant::now();
    while td.fake.handle_count() < n {
        assert!(start.elapsed() < T, "only {} agent processes were started", td.fake.handle_count());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_retried_dispatch_gets_the_first_result_even_when_the_budget_is_used_up() {
    let mut f = fixture().await;
    f.human
        .call::<TwinGetResult>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-a", "daily_dispatches": 1 }))
        .await
        .unwrap();
    let t = add(&f.td, &mut f.a, "retry me", "p").await.unwrap();
    let params = json!({ "id": t.id, "agent": "", "message_id": "dispatch-1" });
    let first: TodoDispatchResult = f.a.call(&f.td, methods::TODO_DISPATCH, params.clone()).await.unwrap();
    let again: TodoDispatchResult = f.a.call(&f.td, methods::TODO_DISPATCH, params).await.unwrap();
    assert_eq!(again.session.id, first.session.id, "the retry returns the first dispatch");
    assert_eq!(f.td.ctx.store.twin_sessions_of("twin-a").await.unwrap().len(), 1, "and opens no second session");
    // 另一项没有这个 id：次数已经用完
    let other = add(&f.td, &mut f.a, "another", "p").await.unwrap();
    let e = dispatch(&f.td, &mut f.a, &other.id).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("budget"));
    f.td.shutdown().await;
}

async fn wait_not_working(td: &TestDaemon, c: &mut TestClient, sid: &str) {
    let start = std::time::Instant::now();
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if matches!(s.status, SessionStatus::Idle | SessionStatus::Ended | SessionStatus::Error) {
            return;
        }
        assert!(start.elapsed() < T, "the session never stopped: {:?}", s.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn the_connections_have_their_own_scope_and_one_cannot_use_the_others_items() {
    let mut f = fixture().await;
    f.human
        .call::<TwinGetResult>(
            &f.td,
            methods::TWIN_SET,
            json!({ "requester": "twin-b", "projects": ["q"], "agent": "fake" }),
        )
        .await
        .unwrap();
    let mine = add(&f.td, &mut f.a, "a's", "p").await.unwrap();
    // 项目只在 a 的范围里；b 派发 a 的项：forbidden，不是 out_of_scope
    let e = dispatch(&f.td, &mut f.b, &mine.id).await.unwrap_err();
    assert!(is_twin_device_forbidden(&e));
    // 各自的范围互不影响
    let a: TwinGetResult = f.human.call(&f.td, methods::TWIN_GET, json!({ "requester": "twin-a" })).await.unwrap();
    let b: TwinGetResult = f.human.call(&f.td, methods::TWIN_GET, json!({ "requester": "twin-b" })).await.unwrap();
    let local: TwinGetResult = f.human.call(&f.td, methods::TWIN_GET, json!({})).await.unwrap();
    assert_eq!(
        (a.scope.projects, b.scope.projects, local.scope.projects),
        (vec!["p".into()], vec!["q".into()], vec![])
    );
    f.td.shutdown().await;
}

// ====================================================================== twin.peer_status、twin.get / twin.set 的 requester

#[tokio::test]
async fn peer_status_is_the_callers_own_and_only_for_twin_devices() {
    let mut f = fixture().await;
    let s: TwinPeerStatus = f.a.call(&f.td, methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert_eq!(
        (s.name.as_str(), s.projects.clone(), s.paused, s.accept_links),
        ("test-node", vec!["p".to_string()], false, true)
    );
    assert_eq!((s.remaining_sessions, s.remaining_dispatches), (2, 3));
    assert!(s.agents.iter().any(|a| a.id == "fake" && a.ready));
    // 派出一个以后，剩余的减少；暂停会反映
    let t = add(&f.td, &mut f.a, "hold it", "p").await.unwrap();
    dispatch(&f.td, &mut f.a, &t.id).await.unwrap();
    let s: TwinPeerStatus = f.a.call(&f.td, methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert_eq!((s.remaining_sessions, s.remaining_dispatches), (1, 2));
    // 另一台设备看到的是它自己的（什么都没开放）
    let other: TwinPeerStatus = f.b.call(&f.td, methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert_eq!((other.projects.len(), other.remaining_dispatches), (0, 10));
    // `accept_links` 反映配置
    anycon_daemon::config::set_value(&f.td.paths, "twin.accept_links", "false").unwrap();
    f.td.ctx.config.reload().unwrap();
    let s: TwinPeerStatus = f.a.call(&f.td, methods::TWIN_PEER_STATUS, json!({})).await.unwrap();
    assert!(!s.accept_links);
    // 别的调用方：不是分身设备，没有「这条连接的范围」
    for c in [&mut f.human] {
        let e = c.call::<Value>(&f.td, methods::TWIN_PEER_STATUS, json!({})).await.unwrap_err();
        assert_eq!((e.stable_code(), reason(&e).as_deref()), (Some(ErrorCode::Forbidden), Some("not_twin_device")));
    }
    f.td.shutdown().await;
}

#[tokio::test]
async fn twin_get_and_set_take_a_requester_that_must_be_an_existing_twin_device() {
    let mut f = fixture().await;
    let web = f.td.hub_client("web-1", "Phone");
    f.td.ctx
        .peers
        .add_test_peer(anycon_daemon::store::PeerRow {
            device_id: "web-1".into(),
            pubkey: [9; 32],
            name: "Phone".into(),
            kind: DeviceKind::Web,
            paired_at_ms: 1,
            last_seen_ms: None,
        })
        .await;
    drop(web);
    for requester in ["nobody", "web-1", "twin", "twin-", "LOCAL", ""] {
        for (method, params) in [
            (methods::TWIN_GET, json!({ "requester": requester })),
            (methods::TWIN_SET, json!({ "requester": requester, "paused": true })),
        ] {
            let e = f.human.call::<Value>(&f.td, method, params).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::NotFound), "{method} {requester:?}: {e:?}");
        }
    }
    // 缺省和 `local` 是本机分身；改一台设备的范围不动本机分身，也不进本机分身的状态
    let before: TwinGetResult = f.human.call(&f.td, methods::TWIN_GET, json!({})).await.unwrap();
    let r: TwinGetResult = f
        .human
        .call(
            &f.td,
            methods::TWIN_SET,
            json!({ "requester": "twin-b", "projects": ["p"], "paused": true, "daily_dispatches": 7 }),
        )
        .await
        .unwrap();
    assert_eq!((r.scope.paused, r.scope.daily_dispatches, r.state), (true, 7, TwinState::Paused));
    let local: TwinGetResult = f.human.call(&f.td, methods::TWIN_GET, json!({ "requester": "local" })).await.unwrap();
    assert_eq!(local, before);
    // 设备范围的校验和本机分身一样：不能超过给人留名额之后的上限
    let e = f
        .human
        .call::<Value>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-b", "max_sessions": 99 }))
        .await
        .unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("no_capacity"));
    // 只由人改：自报了 mcp 的连接和分身设备自己都不行
    let mut mcp = f.td.local_mcp_client();
    let e = mcp
        .call::<Value>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-b", "paused": false }))
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
    let e =
        f.b.call::<Value>(&f.td, methods::TWIN_SET, json!({ "requester": "twin-b", "paused": false }))
            .await
            .unwrap_err();
    assert!(is_twin_device_forbidden(&e));
    let e = f.b.call::<Value>(&f.td, methods::TWIN_GET, json!({})).await.unwrap_err();
    assert!(is_twin_device_forbidden(&e));
    // 审计：改的是谁的范围
    let audit: NodeAuditResult = f.human.call(&f.td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    assert!(audit.entries.iter().any(|e| e.action == "twin.set" && e.target.as_deref() == Some("twin-b")));
    f.td.shutdown().await;
}

#[tokio::test]
async fn node_info_and_agent_list_show_a_twin_device_what_it_needs_and_no_paths() {
    let mut f = fixture().await;
    let full: NodeInfo = f.human.call(&f.td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(full.workspace_root.is_some() && !full.workspace_roots.is_empty() && full.judge.is_some());
    let info: NodeInfo = f.a.call(&f.td, methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!((info.node_id.clone(), info.name.clone()), (full.node_id.clone(), full.name.clone()));
    assert!(info.workspace_root.is_none() && info.workspace_roots.is_empty() && info.judge.is_none());
    assert!(info.update_available.is_none() && info.twin_accept_links && info.capabilities.twin);
    let agents: AgentListResult = f.a.call(&f.td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    assert!(agents.agents.iter().all(|a| a.path.is_none()));
    // 重新探测只有人能触发：它的 `refresh` 不会让 agent 的命令行工具被再跑一遍
    let before = f.td.fake.probe_calls.load(Ordering::SeqCst);
    f.a.call::<AgentListResult>(&f.td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    assert_eq!(f.td.fake.probe_calls.load(Ordering::SeqCst), before);
    f.td.shutdown().await;
}

// ====================================================================== 推送过滤

#[test]
fn every_notification_is_classified_exactly_once() {
    let notifications: BTreeSet<&str> =
        REGISTRY.iter().filter(|m| m.direction == Direction::Notification).map(|m| m.name).collect();
    let pushed: BTreeSet<&str> = PUSHED.iter().copied().collect();
    let never: BTreeSet<&str> = NEVER_PUSHED.iter().copied().collect();
    assert!(pushed.is_disjoint(&never));
    assert_eq!(
        pushed,
        twin_device::PUSHED.iter().copied().collect::<BTreeSet<_>>(),
        "this table and the daemon's differ"
    );
    let classified: BTreeSet<&str> = pushed.union(&never).copied().collect();
    let unclassified: Vec<_> = notifications.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "these notifications are neither pushed to twin devices nor in the never-pushed list; \
         decide which: {unclassified:?}"
    );
    let unknown: Vec<_> = classified.difference(&notifications).collect();
    assert!(unknown.is_empty(), "not notifications in the registry: {unknown:?}");
}

#[tokio::test]
async fn no_notification_but_the_four_owned_kinds_reaches_a_twin_device() {
    let mut f = fixture().await;
    // 它自己的项和会话（watch 着）、它自己的第二个会话（没 watch）、别人的项和会话
    let mine = add(&f.td, &mut f.a, "hold mine", "p").await.unwrap();
    let my_sid = dispatch(&f.td, &mut f.a, &mine.id).await.unwrap().session.id;
    f.a.call::<Empty>(&f.td, methods::SESSION_WATCH, json!({ "session_id": my_sid })).await.unwrap();
    let unwatched = add(&f.td, &mut f.a, "hold unwatched", "p").await.unwrap();
    let unwatched_sid = dispatch(&f.td, &mut f.a, &unwatched.id).await.unwrap().session.id;
    let humans = add(&f.td, &mut f.human, "the human's", "p").await.unwrap();
    let foreign_session: SessionSummary = f
        .human
        .call(
            &f.td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "p", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let mut other_web = f.td.hub_client("web-1", "Phone");
    // 之前的真实通知先收掉：它自己的项和会话的变化是推的
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before: Vec<String> = f.a.drain_now().into_iter().map(|n| n.method).collect();
    assert!(before.iter().any(|m| m == methods::TODO_CHANGED), "its own todo.changed was pushed: {before:?}");
    assert!(before.iter().all(|m| PUSHED.contains(&m.as_str())), "only the four kinds were pushed: {before:?}");
    other_web.drain_now();

    // 不推的：载荷里带着它自己的所有 id 也一样
    let everything = json!({
        "id": mine.id, "todo_id": mine.id, "session_id": my_sid, "request_id": "r", "name": "p",
        "event": { "seq": 1 }, "kind": "device_enrolled", "ts_ms": 1,
    });
    for m in NEVER_PUSHED {
        f.td.ctx.clients.broadcast(m, everything.clone());
        let got: Vec<String> = f.a.drain_now().into_iter().map(|n| n.method).collect();
        assert!(!got.contains(&m.to_string()), "{m} reached a twin device");
        // 别的客户端照常收到（广播本身是通的）
        assert!(other_web.drain_now().iter().any(|n| n.method == *m), "{m} did not reach an ordinary client");
    }
    // 推的：自己的送到，别人的不送
    let mut expect_pushed = |method: &str, params: Value, delivered: bool, what: &str| {
        f.td.ctx.clients.broadcast(method, params);
        let got = f.a.drain_now().into_iter().filter(|n| n.method == method).count();
        assert_eq!(got == 1, delivered, "{method} {what}");
    };
    expect_pushed(methods::TODO_CHANGED, json!({ "id": mine.id, "deleted": false }), true, "own item");
    expect_pushed(methods::TODO_CHANGED, json!({ "id": humans.id, "deleted": false }), false, "someone else's item");
    expect_pushed(methods::TODO_CHANGED, json!({ "deleted": true }), false, "no id");
    expect_pushed(methods::TODO_EVENT, json!({ "todo_id": mine.id, "event": {} }), true, "own item");
    expect_pushed(methods::TODO_EVENT, json!({ "todo_id": humans.id, "event": {} }), false, "someone else's item");
    expect_pushed(methods::SESSION_CHANGED, json!({ "session_id": my_sid }), true, "own session");
    expect_pushed(methods::SESSION_CHANGED, json!({ "session_id": unwatched_sid }), true, "own session, not watched");
    expect_pushed(
        methods::SESSION_CHANGED,
        json!({ "session_id": foreign_session.id }),
        false,
        "someone else's session",
    );
    // `session.event` 只给 watch 着的、并且是它自己的会话
    let n =
        |sid: &str| json!({ "session_id": sid, "seq": 1, "ts_ms": 1, "event": { "type": "text_delta", "text": "x" } });
    assert_eq!(f.td.ctx.clients.to_watchers(&my_sid, methods::SESSION_EVENT, n(&my_sid)), 1);
    assert_eq!(f.a.drain_now().iter().filter(|x| x.method == methods::SESSION_EVENT).count(), 1);
    f.td.ctx.clients.broadcast(methods::SESSION_EVENT, n(&unwatched_sid));
    f.td.ctx.clients.broadcast(methods::SESSION_EVENT, n(&foreign_session.id));
    assert_eq!(f.a.drain_now().iter().filter(|x| x.method == methods::SESSION_EVENT).count(), 0);
    // 别人 watch 的会话的事件也不会送到它
    other_web.handle.watch(&foreign_session.id).unwrap();
    f.td.ctx.clients.to_watchers(&foreign_session.id, methods::SESSION_EVENT, n(&foreign_session.id));
    assert!(f.a.drain_now().is_empty());
    // 另一台分身设备看不到 a 的项和会话
    let mut b = f.b;
    for (m, p) in [
        (methods::TODO_CHANGED, json!({ "id": mine.id })),
        (methods::TODO_EVENT, json!({ "todo_id": mine.id })),
        (methods::SESSION_CHANGED, json!({ "session_id": my_sid })),
    ] {
        f.td.ctx.clients.broadcast(m, p);
    }
    assert!(b.drain_now().is_empty(), "twin-b sees nothing of twin-a");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_twin_device_does_not_get_a_notify_envelope_and_the_others_do() {
    let f = fixture().await;
    f.td.ctx
        .peers
        .add_test_peer(anycon_daemon::store::PeerRow {
            device_id: "phone".into(),
            pubkey: [3; 32],
            name: "Phone".into(),
            kind: DeviceKind::Web,
            paired_at_ms: 1,
            last_seen_ms: None,
        })
        .await;
    let mut ids = f.td.ctx.peers.notify_ids();
    ids.sort();
    assert_eq!(ids, vec!["phone".to_string()]);
    let mut paired = f.td.ctx.peers.paired_ids();
    paired.sort();
    assert_eq!(paired, vec!["phone".to_string(), "twin-a".to_string(), "twin-b".to_string()]);
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_twin_device_can_have_at_most_100_open_items_and_people_are_not_affected() {
    let mut f = fixture().await;
    f.human
        .call::<TwinGetResult>(
            &f.td,
            methods::TWIN_SET,
            json!({ "requester": "twin-b", "projects": ["p"], "agent": "fake" }),
        )
        .await
        .unwrap();
    let mut first = None;
    for i in 0..100 {
        let t = add(&f.td, &mut f.a, &format!("item {i}"), "p").await.unwrap();
        first.get_or_insert(t.id);
    }
    let e = add(&f.td, &mut f.a, "one too many", "p").await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    assert_eq!(e.data.as_ref().and_then(|d| d.field.clone()).as_deref(), Some("todos"));
    assert_eq!(reason(&e).as_deref(), Some("too_many_twin"));
    // the cap is per device, and people keep adding
    add(&f.td, &mut f.b, "b's own", "p").await.unwrap();
    add(&f.td, &mut f.human, "the human's", "p").await.unwrap();
    // closing one frees a slot (a person closes it; the device cannot)
    f.human.call::<Todo>(&f.td, methods::TODO_UPDATE, json!({ "id": first.unwrap(), "state": "done" })).await.unwrap();
    add(&f.td, &mut f.a, "fits again", "p").await.unwrap();
    f.td.shutdown().await;
}

async fn taken_over_events(td: &TestDaemon, human: &mut TestClient, todo: &str) -> Vec<TodoEventData> {
    let h: TodoHistoryResult = human.call(td, methods::TODO_HISTORY, json!({ "id": todo })).await.unwrap();
    h.events.into_iter().map(|e| e.data).filter(|d| matches!(d, TodoEventData::TakenOver { .. })).collect()
}

#[tokio::test]
async fn a_person_speaking_in_a_session_a_twin_device_dispatched_is_recorded_once_per_session() {
    let mut f = fixture().await;
    let t = add(&f.td, &mut f.a, "hold the line", "p").await.unwrap();
    let first = dispatch(&f.td, &mut f.a, &t.id).await.unwrap().session.id;
    wait_handles(&f.td, 1).await;
    let say = |sid: &str, id: &str| json!({ "session_id": sid, "message_id": id, "content": "let me take a look" });

    // 第一条消息：「经过」里记一条 `taken_over`（没有 `node_id`，来源是这台设备的人），推给分身设备；`handler` 不动
    f.human.call::<Value>(&f.td, methods::SESSION_SEND, say(&first, "m1")).await.unwrap();
    let pushed =
        f.a.wait_notification(T, |n| n.method == methods::TODO_EVENT && n.params["event"]["kind"] == "taken_over")
            .await
            .expect("the twin device is told about the takeover");
    assert_eq!(pushed.params["todo_id"], json!(t.id));
    assert!(pushed.params["event"].get("node_id").is_none(), "the node that recorded it does not name itself");
    let events = taken_over_events(&f.td, &mut f.human, &t.id).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0], TodoEventData::TakenOver { by, node_id: None } if by.kind == TodoSourceKind::Local));
    let todo: Todo = f.human.call(&f.td, methods::TODO_GET, json!({ "id": t.id })).await.unwrap();
    assert_eq!(todo.handler, TodoHandler::Me, "the handler says what this node's own twin does, and it is untouched");

    // 同一个会话里再说一条：不再记
    f.human.call::<Value>(&f.td, methods::SESSION_SEND, say(&first, "m2")).await.unwrap();
    assert_eq!(taken_over_events(&f.td, &mut f.human, &t.id).await.len(), 1);
    // 重发同一条（幂等）也不记
    f.human.call::<Value>(&f.td, methods::SESSION_SEND, say(&first, "m2")).await.unwrap();
    assert_eq!(taken_over_events(&f.td, &mut f.human, &t.id).await.len(), 1);

    // 分身设备又派了一个新的会话，人又在新会话里接手：新的一次
    f.td.fake.last_handle().release();
    f.a.call::<Empty>(&f.td, methods::SESSION_CANCEL, json!({ "session_id": first })).await.unwrap();
    wait_not_working(&f.td, &mut f.a, &first).await;
    let second = dispatch(&f.td, &mut f.a, &t.id).await.unwrap().session.id;
    assert_ne!(first, second);
    f.human.call::<Value>(&f.td, methods::SESSION_SEND, say(&second, "m3")).await.unwrap();
    assert_eq!(taken_over_events(&f.td, &mut f.human, &t.id).await.len(), 2);
    f.td.shutdown().await;
}
