//! 清单（`todo.*`，docs/spec/daemon.md § 21）端到端：校验与错误码、幂等、空操作、`closed_ms`、两个上限、翻页、
//! 来源和 `unreviewed`、`todo.changed`、审计、保留期清理、`node.info`。

use std::time::Duration;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn code(e: &RpcError) -> (ErrorCode, Option<String>, Option<String>) {
    let d = e.data.clone().expect("error data");
    (d.code, d.field, d.reason)
}

fn invalid_field(e: &RpcError) -> String {
    let (c, field, _) = code(e);
    assert_eq!(c, ErrorCode::Invalid, "{e:?}");
    field.unwrap_or_default()
}

async fn add(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_ADD, params).await
}

/// 一个预期失败的 `todo.add`。
async fn bad_add(td: &TestDaemon, params: Value) -> RpcError {
    let mut c = td.local_cli_client();
    add(td, &mut c, params).await.unwrap_err()
}

async fn add_ok(td: &TestDaemon, c: &mut TestClient, title: &str) -> Todo {
    add(td, c, json!({ "id": id(), "title": title })).await.unwrap()
}

async fn update(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_UPDATE, params).await
}

async fn list(td: &TestDaemon, c: &mut TestClient, params: Value) -> TodoListResult {
    c.call(td, methods::TODO_LIST, params).await.unwrap()
}

fn changes(c: &mut TestClient) -> Vec<TodoChanged> {
    c.drain_now()
        .into_iter()
        .filter(|n| n.method == methods::TODO_CHANGED)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect()
}

async fn audit(td: &TestDaemon, prefix: &str) -> Vec<AuditEntry> {
    td.ctx.audit.query(None, None, 1000).await.into_iter().filter(|e| e.action.starts_with(prefix)).collect()
}

/// 直接写库的一项（造大量数据和固定时间戳用）。
fn row(id: &str, updated_ms: u64, state: TodoState) -> Todo {
    Todo {
        id: id.to_string(),
        title: format!("item {id}"),
        body: String::new(),
        project: None,
        state,
        due_ms: None,
        source: TodoSource { kind: TodoSourceKind::Local, device_id: None, session_id: None },
        unreviewed: false,
        created_ms: updated_ms,
        updated_ms,
        closed_ms: state.is_closed().then_some(updated_ms),
        sessions: vec![],
        activity: None,
    }
}

// ---------------------------------------------------------------- todo.add

#[tokio::test]
async fn add_creates_an_open_item_and_trims_the_title() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let before = anycon_daemon::util::now_ms();
    let new_id = id();
    let t = add(
        &td,
        &mut c,
        json!({ "id": new_id, "title": "  Write the report \t", "body": "# notes", "project": " web ", "due_ms": 123 }),
    )
    .await
    .unwrap();
    assert_eq!((t.id.as_str(), t.title.as_str(), t.body.as_str()), (new_id.as_str(), "Write the report", "# notes"));
    assert_eq!((t.project.as_deref(), t.state, t.due_ms), (Some("web"), TodoState::Open, Some(123)));
    assert_eq!(t.source, TodoSource { kind: TodoSourceKind::Local, device_id: None, session_id: None });
    assert!(!t.unreviewed && t.closed_ms.is_none());
    assert!(t.created_ms >= before && t.created_ms == t.updated_ms);
    // body 缺省是空串，项目缺省没有
    let t2 = add_ok(&td, &mut c, "bare").await;
    assert_eq!((t2.body.as_str(), t2.project.clone()), ("", None));
    // 空白的项目名按没给处理
    let t3 = add(&td, &mut c, json!({ "id": id(), "title": "x", "project": "  " })).await.unwrap();
    assert_eq!(t3.project, None);
    // 读回来一样
    let all = list(&td, &mut c, json!({})).await;
    assert!(all.todos.contains(&t) && !all.has_more);
    td.shutdown().await;
}

#[tokio::test]
async fn add_validates_every_field() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let bad = |params: Value| bad_add(&td, params);
    // id：必须是带连字符的 UUID
    // 只收小写带连字符的 UUID：大写、花括号、没有连字符、urn 形式都是 `invalid`，不做规范化
    for not_uuid in [
        "",
        "abc",
        "not-a-uuid-at-all-not-a-uuid-at-all-1234",
        "0123456789abcdef0123456789abcdef",
        "7D1C1E5A-3B9F-4C2E-8A64-0F2B5D9E1A77",
        "{7d1c1e5a-3b9f-4c2e-8a64-0f2b5d9e1a77}",
        "urn:uuid:7d1c1e5a-3b9f-4c2e-8a64-0f2b5d9e1a77",
        " 7d1c1e5a-3b9f-4c2e-8a64-0f2b5d9e1a77",
    ] {
        assert_eq!(invalid_field(&bad(json!({ "id": not_uuid, "title": "x" })).await), "id", "{not_uuid:?}");
    }
    // title：空、纯空白、超长（按字符数，去掉首尾空白之后）、多行、控制字符
    for t in
        ["", "   \t ", &"a".repeat(201), "two\nlines", "cr\rhere", "esc\u{1b}[31m", "ls\u{2028}sep", "ps\u{2029}sep"]
    {
        assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": t })).await), "title", "{t:?}");
    }
    // 项目名同样单行
    assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": "x", "project": "a\u{2028}b" })).await), "project");
    // 边界：200 个字符（含多字节字符）可以；两端的空白不算
    add(&td, &mut c, json!({ "id": id(), "title": "字".repeat(200) })).await.unwrap();
    add(&td, &mut c, json!({ "id": id(), "title": format!("  {}  ", "a".repeat(200)) })).await.unwrap();
    // body：16 KiB（字节）
    assert_eq!(
        invalid_field(&bad(json!({ "id": id(), "title": "x", "body": "a".repeat(16 * 1024 + 1) })).await),
        "body"
    );
    assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": "x", "body": "字".repeat(5462) })).await), "body");
    add(&td, &mut c, json!({ "id": id(), "title": "x", "body": "a".repeat(16 * 1024) })).await.unwrap();
    // 正文里可以有换行
    add(&td, &mut c, json!({ "id": id(), "title": "x", "body": "line1\nline2\n" })).await.unwrap();
    // project / due_ms
    assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": "x", "project": "a\nb" })).await), "project");
    assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": "x", "project": "p".repeat(201) })).await), "project");
    assert_eq!(invalid_field(&bad(json!({ "id": id(), "title": "x", "due_ms": u64::MAX })).await), "due_ms");
    // 缺必填参数 / 类型不对
    let e = bad(json!({ "title": "x" })).await;
    assert_eq!(code(&e).0, ErrorCode::Invalid);
    let e = bad(json!({ "id": id(), "title": 5 })).await;
    assert_eq!(code(&e).0, ErrorCode::Invalid);
    // 失败的请求什么都没写
    let n = list(&td, &mut c, json!({})).await.todos.len();
    assert_eq!(n, 4, "only the four valid adds above are stored");
    td.shutdown().await;
}

#[tokio::test]
async fn add_is_idempotent_on_id_and_the_repeat_is_silent() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let new_id = id();
    let first = add(&td, &mut c, json!({ "id": new_id, "title": "original", "body": "b" })).await.unwrap();
    assert_eq!(changes(&mut other).len(), 1);
    assert_eq!(audit(&td, "todo.add").await.len(), 1);

    // 同一个 id 再来（内容不同、甚至内容本身不合法）：原样返回已有的那一项，不改它
    tokio::time::sleep(Duration::from_millis(5)).await;
    let again = add(&td, &mut other, json!({ "id": new_id, "title": "changed", "agent": true })).await.unwrap();
    assert_eq!(again, first);
    let invalid_repeat = add(&td, &mut c, json!({ "id": new_id, "title": "" })).await.unwrap();
    assert_eq!(invalid_repeat, first);
    assert_eq!(list(&td, &mut c, json!({})).await.todos, vec![first]);
    // 没有通知、没有审计
    assert!(changes(&mut other).is_empty() && changes(&mut c).len() == 1, "only the first add was announced");
    assert_eq!(audit(&td, "todo.add").await.len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn the_open_item_cap_counts_only_unclosed_items() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut ids = Vec::new();
    for _ in 0..1000 {
        let i = id();
        td.ctx.store.todo_insert(&row(&i, 1_000, TodoState::Open)).await.unwrap();
        ids.push(i);
    }
    // 已关闭的不占名额
    td.ctx.store.todo_insert(&row(&id(), 1_000, TodoState::Done)).await.unwrap();
    td.ctx.store.todo_insert(&row(&id(), 1_000, TodoState::Dropped)).await.unwrap();

    let e = add(&td, &mut c, json!({ "id": id(), "title": "one too many" })).await.unwrap_err();
    assert_eq!(code(&e).0, ErrorCode::Invalid);
    assert_eq!(code(&e).2.as_deref(), Some("too_many"));
    // agent 加的同样受它限制（先判这条）
    let e = add(&td, &mut c, json!({ "id": id(), "title": "x", "agent": true })).await.unwrap_err();
    assert_eq!(code(&e).2.as_deref(), Some("too_many"));

    // 关掉一项就腾出一个名额
    update(&td, &mut c, json!({ "id": ids[0], "state": "done" })).await.unwrap();
    add_ok(&td, &mut c, "fits now").await;
    assert_eq!(
        add(&td, &mut c, json!({ "id": id(), "title": "full again" }))
            .await
            .unwrap_err()
            .data
            .unwrap()
            .reason
            .as_deref(),
        Some("too_many")
    );
    td.shutdown().await;
}

#[tokio::test]
async fn the_unreviewed_cap_only_limits_agent_added_items() {
    let td = TestDaemon::start().await;
    let mut agent = td.local_mcp_client();
    let mut human = td.local_cli_client();
    for _ in 0..100 {
        add(&td, &mut agent, json!({ "id": id(), "title": "from an agent" })).await.unwrap();
    }
    let e = add(&td, &mut agent, json!({ "id": id(), "title": "101st" })).await.unwrap_err();
    assert_eq!(
        (code(&e).0, code(&e).2.as_deref().map(str::to_string)),
        (ErrorCode::Invalid, Some("too_many_unreviewed".into()))
    );
    // `agent: true` 从别的连接来的一样
    let e = add(&td, &mut human, json!({ "id": id(), "title": "x", "agent": true })).await.unwrap_err();
    assert_eq!(code(&e).2.as_deref(), Some("too_many_unreviewed"));
    // 人照常能记
    add_ok(&td, &mut human, "mine").await;
    // 人收下一项，agent 又有一个名额
    let first = list(&td, &mut human, json!({})).await.todos.into_iter().find(|t| t.unreviewed).unwrap();
    update(&td, &mut human, json!({ "id": first.id, "reviewed": true })).await.unwrap();
    add(&td, &mut agent, json!({ "id": id(), "title": "fits" })).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- source / unreviewed

#[tokio::test]
async fn source_depends_on_the_connection_and_the_agent_flag() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    let mut dev = td.hub_client("dev-phone", "Phone");

    for (name, who, flag, want_kind, want_device) in [
        ("cli", "cli", None, TodoSourceKind::Local, None),
        ("cli+false", "cli", Some(false), TodoSourceKind::Local, None),
        ("cli+agent", "cli", Some(true), TodoSourceKind::Agent, None),
        ("mcp", "mcp", None, TodoSourceKind::Agent, None),
        // 自报了 mcp 的连接，`agent: false` 也改变不了
        ("mcp+false", "mcp", Some(false), TodoSourceKind::Agent, None),
        ("mcp+agent", "mcp", Some(true), TodoSourceKind::Agent, None),
        ("device", "dev", None, TodoSourceKind::Device, Some("dev-phone")),
        ("device+false", "dev", Some(false), TodoSourceKind::Device, Some("dev-phone")),
        ("device+agent", "dev", Some(true), TodoSourceKind::Agent, Some("dev-phone")),
    ] {
        let client = match who {
            "cli" => &mut cli,
            "mcp" => &mut mcp,
            _ => &mut dev,
        };
        let mut p = json!({ "id": id(), "title": name });
        if let Some(f) = flag {
            p["agent"] = json!(f);
        }
        let t = add(&td, client, p).await.unwrap();
        assert_eq!(t.source.kind, want_kind, "{name}");
        assert_eq!(t.source.device_id.as_deref(), want_device, "{name}");
        assert_eq!(t.source.session_id, None, "{name}");
        assert_eq!(t.unreviewed, want_kind == TodoSourceKind::Agent, "{name}: unreviewed iff agent");
    }
    td.shutdown().await;
}

#[tokio::test]
async fn origin_session_is_only_kept_for_agent_items_and_existing_sessions() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    let s: SessionSummary = cli.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();

    // agent 项 + 存在的会话：记下
    let t = add(&td, &mut mcp, json!({ "id": id(), "title": "a", "origin_session": s.id })).await.unwrap();
    assert_eq!(t.source.session_id.as_deref(), Some(s.id.as_str()));
    let t =
        add(&td, &mut cli, json!({ "id": id(), "title": "b", "agent": true, "origin_session": s.id })).await.unwrap();
    assert_eq!(t.source.session_id.as_deref(), Some(s.id.as_str()));
    // 会话不存在：丢掉这个字段，照常建项
    let t = add(&td, &mut mcp, json!({ "id": id(), "title": "c", "origin_session": "sess_doesnotexist0000000000" }))
        .await
        .unwrap();
    assert_eq!((t.source.kind, t.source.session_id), (TodoSourceKind::Agent, None));
    // 不是 agent 项：忽略，来源里没有会话
    let t = add(&td, &mut cli, json!({ "id": id(), "title": "d", "origin_session": s.id })).await.unwrap();
    assert_eq!((t.source.kind, t.source.session_id), (TodoSourceKind::Local, None));
    let mut dev = td.hub_client("dev-1", "Phone");
    let t = add(&td, &mut dev, json!({ "id": id(), "title": "e", "origin_session": s.id })).await.unwrap();
    assert_eq!((t.source.kind, t.source.session_id), (TodoSourceKind::Device, None));

    // 会话之后被删，session_id 原样留着
    let kept = add(&td, &mut mcp, json!({ "id": id(), "title": "f", "origin_session": s.id })).await.unwrap();
    let _: Value = cli.call(&td, methods::SESSION_DELETE, json!({ "session_id": s.id })).await.unwrap();
    let back: Todo = list(&td, &mut cli, json!({})).await.todos.into_iter().find(|t| t.id == kept.id).unwrap();
    assert_eq!(back.source.session_id.as_deref(), Some(s.id.as_str()));
    td.shutdown().await;
}

#[tokio::test]
async fn unreviewed_is_cleared_by_a_person_and_never_by_an_mcp_connection() {
    let td = TestDaemon::start().await;
    let mut mcp = td.local_mcp_client();
    let mut cli = td.local_cli_client();
    let mut dev = td.hub_client("dev-phone", "Phone");
    let a = add(&td, &mut mcp, json!({ "id": id(), "title": "one" })).await.unwrap();
    assert!(a.unreviewed);

    // 自报 mcp 的连接：`reviewed: true` 被忽略（空操作）；它做的别的修改也不算「人改过」
    let r = update(&td, &mut mcp, json!({ "id": a.id, "reviewed": true })).await.unwrap();
    assert!(r.unreviewed);
    assert_eq!(r.updated_ms, a.updated_ms, "an ignored field is a no-op");
    let r =
        update(&td, &mut mcp, json!({ "id": a.id, "title": "renamed by the agent", "reviewed": true })).await.unwrap();
    assert_eq!(r.title, "renamed by the agent");
    assert!(r.unreviewed, "an MCP connection never clears it");

    // `reviewed: false` 被忽略
    let before = update(&td, &mut cli, json!({ "id": a.id })).await.unwrap();
    let r = update(&td, &mut cli, json!({ "id": a.id, "reviewed": false })).await.unwrap();
    assert_eq!(r, before);
    assert!(r.unreviewed);

    // 人收下：`reviewed: true` 清掉它，算一次修改（更新时间、通知）
    let mut watcher = td.hub_client("dev-w", "W");
    let r = update(&td, &mut cli, json!({ "id": a.id, "reviewed": true })).await.unwrap();
    assert!(!r.unreviewed);
    assert_eq!(r.title, "renamed by the agent", "nothing else changed");
    assert_eq!(changes(&mut watcher).len(), 1);
    // 之后不会再变回去：再给 true 是空操作
    let again = update(&td, &mut cli, json!({ "id": a.id, "reviewed": true })).await.unwrap();
    assert_eq!(again, r);
    assert!(changes(&mut watcher).is_empty());

    // 任何别的修改（经 hub 的设备、本机 CLI）也会清掉，各类修改都算
    for (who, patch) in [
        ("cli title", json!({ "title": "edited" })),
        ("device body", json!({ "body": "details" })),
        ("device state", json!({ "state": "working" })),
        ("cli project", json!({ "project": "web" })),
        ("device due", json!({ "due_ms": 5 })),
    ] {
        let t = add(&td, &mut mcp, json!({ "id": id(), "title": who })).await.unwrap();
        assert!(t.unreviewed);
        let mut p = patch.clone();
        p["id"] = json!(t.id);
        let r = if who.starts_with("cli") { update(&td, &mut cli, p).await } else { update(&td, &mut dev, p).await }
            .unwrap();
        assert!(!r.unreviewed, "{who}");
    }
    // 对已收下的项再改，当然仍是 false；一个空操作不会清掉别人的 unreviewed
    let b = add(&td, &mut mcp, json!({ "id": id(), "title": "keep me" })).await.unwrap();
    let r = update(&td, &mut cli, json!({ "id": b.id, "title": "keep me" })).await.unwrap();
    assert!(r.unreviewed, "setting the same title changes nothing");
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.update

#[tokio::test]
async fn update_merges_clears_and_detects_no_ops() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let t =
        add(&td, &mut c, json!({ "id": id(), "title": "t", "body": "b", "project": "p", "due_ms": 10 })).await.unwrap();
    changes(&mut other);
    let audits_before = audit(&td, "todo.update").await.len();
    tokio::time::sleep(Duration::from_millis(5)).await;

    // 没有任何字段真的变了：不改 updated_ms、不通知、不审计
    for noop in [
        json!({ "id": t.id }),
        json!({ "id": t.id, "title": "t" }),
        json!({ "id": t.id, "title": "  t  " }),
        json!({ "id": t.id, "body": "b", "project": "p", "due_ms": 10, "state": "open" }),
        json!({ "id": t.id, "clear": [] }),
    ] {
        let r = update(&td, &mut c, noop.clone()).await.unwrap();
        assert_eq!(r, t, "{noop}");
    }
    // 清空本来就没有的字段也是空操作
    let bare = add_ok(&td, &mut c, "bare").await;
    changes(&mut other);
    let r = update(&td, &mut c, json!({ "id": bare.id, "clear": ["project", "due_ms"] })).await.unwrap();
    assert_eq!(r, bare);
    assert!(changes(&mut other).is_empty());
    assert_eq!(audit(&td, "todo.update").await.len(), audits_before);

    // 合并：缺省的字段保持不变
    let r = update(&td, &mut c, json!({ "id": t.id, "title": "  new title " })).await.unwrap();
    assert_eq!(
        (r.title.as_str(), r.body.as_str(), r.project.as_deref(), r.due_ms),
        ("new title", "b", Some("p"), Some(10))
    );
    assert!(r.updated_ms > t.updated_ms);
    // body 可以改成空串（空串不是缺省）
    let r = update(&td, &mut c, json!({ "id": t.id, "body": "" })).await.unwrap();
    assert_eq!(r.body, "");
    // clear 在合并之后生效，两边都有时以 clear 为准
    let r = update(&td, &mut c, json!({ "id": t.id, "project": "q", "clear": ["project", "due_ms"] })).await.unwrap();
    assert_eq!((r.project.clone(), r.due_ms), (None, None));
    // 通知：每次真正的修改一条，带完整的项
    let notes = changes(&mut other);
    assert_eq!(notes.len(), 3);
    assert!(notes.iter().all(|n| !n.deleted && n.todo.as_ref().is_some_and(|x| x.id == t.id)));
    assert_eq!(notes.last().unwrap().todo.as_ref().unwrap(), &r);
    td.shutdown().await;
}

#[tokio::test]
async fn update_validates_and_reports_not_found() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    for (field, patch) in [
        ("title", json!({ "title": "" })),
        ("title", json!({ "title": "a\nb" })),
        ("title", json!({ "title": "a".repeat(201) })),
        ("body", json!({ "body": "a".repeat(16 * 1024 + 1) })),
        ("project", json!({ "project": "" })),
        ("project", json!({ "project": "a\nb" })),
        ("due_ms", json!({ "due_ms": u64::MAX })),
        ("id", json!({ "id": "nope" })),
    ] {
        let mut p = json!({ "id": t.id });
        for (k, v) in patch.as_object().unwrap() {
            p[k] = v.clone();
        }
        let e = update(&td, &mut c, p).await.unwrap_err();
        assert_eq!(invalid_field(&e), field, "{patch}");
    }
    // 不合法的请求一项都不改
    let e = update(&td, &mut c, json!({ "id": t.id, "body": "changed", "title": "" })).await.unwrap_err();
    assert_eq!(invalid_field(&e), "title");
    assert_eq!(list(&td, &mut c, json!({})).await.todos, vec![t.clone()]);
    // 未知的 state / clear 值
    for bad in [json!({ "id": t.id, "state": "finished" }), json!({ "id": t.id, "clear": ["title"] })] {
        assert_eq!(code(&update(&td, &mut c, bad).await.unwrap_err()).0, ErrorCode::Invalid);
    }
    // not_found
    let e = update(&td, &mut c, json!({ "id": id(), "title": "x" })).await.unwrap_err();
    assert_eq!(code(&e).0, ErrorCode::NotFound);
    td.shutdown().await;
}

#[tokio::test]
async fn closed_ms_follows_the_state() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    // 这一步不限制状态之间怎么转；开着的状态之间换不动 closed_ms
    for s in ["working", "waiting", "review", "open"] {
        let r = update(&td, &mut c, json!({ "id": t.id, "state": s })).await.unwrap();
        assert_eq!(r.closed_ms, None, "{s}");
    }
    tokio::time::sleep(Duration::from_millis(3)).await;
    let done = update(&td, &mut c, json!({ "id": t.id, "state": "done" })).await.unwrap();
    assert_eq!(done.state, TodoState::Done);
    let closed = done.closed_ms.expect("closed_ms is set on entering done");
    assert_eq!(closed, done.updated_ms);
    // done → dropped 还是关闭的：保持原来的关闭时间
    tokio::time::sleep(Duration::from_millis(3)).await;
    let dropped = update(&td, &mut c, json!({ "id": t.id, "state": "dropped" })).await.unwrap();
    assert_eq!((dropped.state, dropped.closed_ms), (TodoState::Dropped, Some(closed)));
    assert!(dropped.updated_ms > done.updated_ms);
    // 重新打开清掉
    let open = update(&td, &mut c, json!({ "id": t.id, "state": "open" })).await.unwrap();
    assert_eq!((open.state, open.closed_ms), (TodoState::Open, None));
    // 从 review 直接关闭
    update(&td, &mut c, json!({ "id": t.id, "state": "review" })).await.unwrap();
    let dropped = update(&td, &mut c, json!({ "id": t.id, "state": "dropped" })).await.unwrap();
    assert!(dropped.closed_ms.is_some());
    // 读回来一致
    let all = list(&td, &mut c, json!({ "include_closed": true })).await;
    assert_eq!(all.todos, vec![dropped]);
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.delete

#[tokio::test]
async fn delete_is_physical_and_reports_not_found() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let t = add_ok(&td, &mut c, "t").await;
    changes(&mut other);
    let _: Value = c.call(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap();
    assert!(list(&td, &mut c, json!({ "include_closed": true })).await.todos.is_empty());
    assert!(td.ctx.store.todo_get(&t.id).await.unwrap().is_none(), "the row is gone");
    assert_eq!(changes(&mut other), vec![TodoChanged { id: t.id.clone(), todo: None, deleted: true }]);
    let e = c.call::<Value>(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap_err();
    assert_eq!(code(&e).0, ErrorCode::NotFound);
    assert!(changes(&mut other).is_empty(), "a failed delete announces nothing");
    // 之后同一个 id 可以重新 add
    add(&td, &mut c, json!({ "id": t.id, "title": "again" })).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.get

#[tokio::test]
async fn get_returns_one_item_and_is_read_only() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let t = add_ok(&td, &mut c, "read me").await;
    changes(&mut other);
    let audits_before = audit(&td, "todo.").await.len();
    let got: Todo = c.call(&td, methods::TODO_GET, json!({ "id": t.id })).await.unwrap();
    assert_eq!(got, t);
    assert_eq!(audit(&td, "todo.").await.len(), audits_before, "a read is not audited");
    assert!(changes(&mut other).is_empty(), "a read announces nothing");
    td.shutdown().await;
}

#[tokio::test]
async fn get_reports_not_found_and_rejects_ids_that_are_not_lowercase_uuids() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let e = c.call::<Todo>(&td, methods::TODO_GET, json!({ "id": id() })).await.unwrap_err();
    assert_eq!(code(&e).0, ErrorCode::NotFound);
    let t = add_ok(&td, &mut c, "t").await;
    for bad in [t.id.to_uppercase(), "not-a-uuid".to_string(), String::new()] {
        let e = c.call::<Todo>(&td, methods::TODO_GET, json!({ "id": bad })).await.unwrap_err();
        assert_eq!(invalid_field(&e), "id", "{bad:?}");
    }
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.delete if_updated_ms

#[tokio::test]
async fn a_conditional_delete_with_the_current_updated_ms_deletes() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let t = add_ok(&td, &mut c, "t").await;
    changes(&mut other);
    let _: Value =
        c.call(&td, methods::TODO_DELETE, json!({ "id": t.id, "if_updated_ms": t.updated_ms })).await.unwrap();
    assert!(td.ctx.store.todo_get(&t.id).await.unwrap().is_none());
    assert_eq!(changes(&mut other), vec![TodoChanged { id: t.id.clone(), todo: None, deleted: true }]);
    assert_eq!(audit(&td, "todo.delete").await.len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn a_conditional_delete_that_no_longer_matches_is_refused_with_the_current_updated_ms_and_changes_nothing() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut other = td.hub_client("dev-phone", "Phone");
    let t = add_ok(&td, &mut c, "t").await;
    let edited = update(&td, &mut c, json!({ "id": t.id, "title": "edited" })).await.unwrap();
    assert!(edited.updated_ms > t.updated_ms);
    changes(&mut other);
    let audits_before = audit(&td, "todo.").await.len();
    let e = c
        .call::<Value>(&td, methods::TODO_DELETE, json!({ "id": t.id, "if_updated_ms": t.updated_ms }))
        .await
        .unwrap_err();
    let (c0, field, reason) = code(&e);
    assert_eq!((c0, field.as_deref(), reason.as_deref()), (ErrorCode::Invalid, Some("if_updated_ms"), Some("changed")));
    assert_eq!(e.data.unwrap().updated_ms, Some(edited.updated_ms));
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap(), edited, "nothing was deleted or changed");
    assert_eq!(audit(&td, "todo.").await.len(), audits_before, "a refused delete is not audited");
    assert!(changes(&mut other).is_empty(), "a refused delete announces nothing");
    // 用现在的值再来就删得掉
    let _: Value =
        c.call(&td, methods::TODO_DELETE, json!({ "id": t.id, "if_updated_ms": edited.updated_ms })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn a_conditional_delete_of_a_missing_item_is_not_found_and_without_the_condition_nothing_changes() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let e = c.call::<Value>(&td, methods::TODO_DELETE, json!({ "id": id(), "if_updated_ms": 1 })).await.unwrap_err();
    assert_eq!(code(&e).0, ErrorCode::NotFound);
    // 没有给条件时无条件地删：不管 updated_ms 是多少
    let t = add_ok(&td, &mut c, "t").await;
    let _: Value = c.call(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- updated_ms 严格递增

#[tokio::test]
async fn updated_ms_strictly_increases_even_for_writes_in_the_same_millisecond() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    let mut last = t.updated_ms;
    for i in 0..8 {
        // 紧挨着的几次写：多半落在同一毫秒里
        let u = update(&td, &mut c, json!({ "id": t.id, "title": format!("v{i}") })).await.unwrap();
        assert!(u.updated_ms > last, "update {i}: {} must be above {last}", u.updated_ms);
        last = u.updated_ms;
    }
    td.shutdown().await;
}

#[tokio::test]
async fn updated_ms_still_increases_when_the_clock_is_behind_the_stored_value() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    // 库里的这一项的 updated_ms 在「未来」（时钟往回拨过）
    let future = 4_000_000_000_000u64;
    let i = id();
    td.ctx.store.todo_insert(&row(&i, future, TodoState::Open)).await.unwrap();
    let u = update(&td, &mut c, json!({ "id": i, "title": "after the clock stepped back" })).await.unwrap();
    assert_eq!(u.updated_ms, future + 1);
    let u2 = update(&td, &mut c, json!({ "id": i, "state": "done" })).await.unwrap();
    assert_eq!(u2.updated_ms, future + 2);
    td.shutdown().await;
}

#[tokio::test]
async fn a_no_op_update_keeps_updated_ms() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    let same = update(&td, &mut c, json!({ "id": t.id, "title": "t" })).await.unwrap();
    assert_eq!(same.updated_ms, t.updated_ms);
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.list

#[tokio::test]
async fn list_orders_by_updated_then_id_and_pages_with_a_cursor() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    // 三项同一时刻 + 别的时刻，id 固定（比较的是字符串）
    let ids = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
        "00000000-0000-4000-8000-000000000003",
        "00000000-0000-4000-8000-000000000004",
        "00000000-0000-4000-8000-000000000005",
    ];
    for (i, ts) in ids.iter().zip([100, 200, 200, 200, 300]) {
        td.ctx.store.todo_insert(&row(i, ts, TodoState::Open)).await.unwrap();
    }
    let want = [ids[4], ids[3], ids[2], ids[1], ids[0]];
    let all = list(&td, &mut c, json!({})).await;
    assert_eq!(all.todos.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), want);
    assert!(!all.has_more);

    // limit 一页一页翻，游标是上一页最后一项的 (updated_ms, id)，中间落在同一个 updated_ms 上也不重不漏
    for page_size in [1usize, 2, 3] {
        let mut seen = Vec::new();
        let mut before: Option<Value> = None;
        for _ in 0..10 {
            let mut p = json!({ "limit": page_size });
            if let Some(b) = &before {
                p["before"] = b.clone();
            }
            let r = list(&td, &mut c, p).await;
            assert!(r.todos.len() <= page_size);
            seen.extend(r.todos.iter().map(|t| t.id.clone()));
            let Some(last) = r.todos.last() else { break };
            before = Some(json!({ "updated_ms": last.updated_ms, "id": last.id }));
            if !r.has_more {
                break;
            }
            assert_eq!(r.todos.len(), page_size, "a page that has more is full");
        }
        assert_eq!(seen, want, "page size {page_size}");
    }
    // 恰好翻完时 has_more 是 false，不是多一页空的
    let r = list(&td, &mut c, json!({ "limit": 5 })).await;
    assert!(!r.has_more && r.todos.len() == 5);
    let r = list(&td, &mut c, json!({ "limit": 4 })).await;
    assert!(r.has_more && r.todos.len() == 4);
    // limit 0 当作 1；超大的 limit 不报错
    assert_eq!(list(&td, &mut c, json!({ "limit": 0 })).await.todos.len(), 1);
    assert_eq!(list(&td, &mut c, json!({ "limit": 100_000 })).await.todos.len(), 5);
    // 游标之后没有了
    let r = list(&td, &mut c, json!({ "before": { "updated_ms": 100, "id": ids[0] } })).await;
    assert!(r.todos.is_empty() && !r.has_more);
    td.shutdown().await;
}

#[tokio::test]
async fn list_defaults_to_unclosed_items_and_filters_by_project() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let a = add(&td, &mut c, json!({ "id": id(), "title": "a", "project": "web" })).await.unwrap();
    let b = add(&td, &mut c, json!({ "id": id(), "title": "b", "project": "api" })).await.unwrap();
    let d = add(&td, &mut c, json!({ "id": id(), "title": "d", "project": "web" })).await.unwrap();
    let n = add_ok(&td, &mut c, "no project").await;
    update(&td, &mut c, json!({ "id": d.id, "state": "done" })).await.unwrap();
    update(&td, &mut c, json!({ "id": b.id, "state": "waiting" })).await.unwrap();

    let ids = |r: &TodoListResult| {
        let mut v: Vec<String> = r.todos.iter().map(|t| t.id.clone()).collect();
        v.sort();
        v
    };
    let sorted = |xs: &[&Todo]| {
        let mut v: Vec<String> = xs.iter().map(|t| t.id.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(ids(&list(&td, &mut c, json!({})).await), sorted(&[&a, &b, &n]), "closed items are left out");
    assert_eq!(ids(&list(&td, &mut c, json!({ "include_closed": false })).await), sorted(&[&a, &b, &n]));
    assert_eq!(ids(&list(&td, &mut c, json!({ "include_closed": true })).await), sorted(&[&a, &b, &d, &n]));
    assert_eq!(ids(&list(&td, &mut c, json!({ "project": "web" })).await), sorted(&[&a]));
    assert_eq!(ids(&list(&td, &mut c, json!({ "project": "web", "include_closed": true })).await), sorted(&[&a, &d]));
    assert!(list(&td, &mut c, json!({ "project": "nope" })).await.todos.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn a_page_stops_at_the_byte_budget_and_the_pages_add_up() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    // 80 项各带接近 16 KiB 的正文 = 约 1.3 MiB，一页装不下
    let body = "x".repeat(16 * 1024);
    let mut expected = Vec::new();
    for i in 0..80u64 {
        let mut t = row(&format!("00000000-0000-4000-8000-{i:012}"), 1_000 + i, TodoState::Open);
        t.body = body.clone();
        td.ctx.store.todo_insert(&t).await.unwrap();
        expected.push(t.id);
    }
    expected.reverse(); // 新的在前

    let first = list(&td, &mut c, json!({})).await;
    assert!(first.has_more, "the page is cut by the byte budget before the default limit of 200");
    let bytes: usize = first.todos.iter().map(|t| serde_json::to_vec(t).unwrap().len()).sum();
    assert!(bytes <= 1024 * 1024, "{bytes} bytes in one page");
    assert!(first.todos.len() >= 60 && first.todos.len() < 80, "{}", first.todos.len());

    // 一页页翻到底：不重不漏、顺序不变
    let mut seen: Vec<String> = Vec::new();
    let mut before: Option<Value> = None;
    loop {
        let mut p = json!({});
        if let Some(b) = &before {
            p["before"] = b.clone();
        }
        let r = list(&td, &mut c, p).await;
        assert!(!r.todos.is_empty());
        let last = r.todos.last().unwrap();
        before = Some(json!({ "updated_ms": last.updated_ms, "id": last.id }));
        seen.extend(r.todos.into_iter().map(|t| t.id));
        if !r.has_more {
            break;
        }
    }
    assert_eq!(seen, expected);
    td.shutdown().await;
}

// ---------------------------------------------------------------- todo.changed

#[tokio::test]
async fn changes_reach_every_online_client_including_the_caller() {
    let td = TestDaemon::start().await;
    let mut a = td.local_cli_client();
    let mut b = td.hub_client("dev-b", "B");
    let mut c = td.local_mcp_client();
    let t = add_ok(&td, &mut a, "t").await;
    let upd = update(&td, &mut b, json!({ "id": t.id, "state": "working" })).await.unwrap();
    let _: Value = c.call(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap();

    let expect = vec![
        TodoChanged { id: t.id.clone(), todo: Some(t.clone()), deleted: false },
        TodoChanged { id: t.id.clone(), todo: Some(upd), deleted: false },
        TodoChanged { id: t.id.clone(), todo: None, deleted: true },
    ];
    for (name, client) in [("a", &mut a), ("b", &mut b), ("c", &mut c)] {
        assert_eq!(changes(client), expect, "{name}");
    }
    // 通知体里没有 `todo` 键，而不是 null
    let n = TodoChanged { id: "x".into(), todo: None, deleted: true };
    assert!(serde_json::to_value(n).unwrap().get("todo").is_none());
    td.shutdown().await;
}

// ---------------------------------------------------------------- 审计

#[tokio::test]
async fn audit_records_lengths_and_field_names_never_content() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    let secret_title = "SECRET-TITLE-xyzzy";
    let secret_body = "SECRET-BODY-plugh";
    let t = add(&td, &mut cli, json!({ "id": id(), "title": secret_title, "body": secret_body, "project": "web" }))
        .await
        .unwrap();
    let agent_item = add(&td, &mut mcp, json!({ "id": id(), "title": "字字字" })).await.unwrap();
    update(
        &td,
        &mut cli,
        json!({ "id": t.id, "title": "SECRET-NEW-TITLE", "body": "SECRET-NEW-BODY", "state": "done" }),
    )
    .await
    .unwrap();
    update(&td, &mut cli, json!({ "id": t.id, "state": "dropped", "clear": ["project"] })).await.unwrap();
    update(&td, &mut cli, json!({ "id": agent_item.id, "reviewed": true })).await.unwrap();
    let _: Value = cli.call(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap();

    let entries = audit(&td, "todo.").await;
    // 全文里没有任何标题 / 正文
    let dump = serde_json::to_string(&entries).unwrap();
    for needle in ["SECRET", "xyzzy", "plugh", "字字字"] {
        assert!(!dump.contains(needle), "{needle} leaked into the audit log: {dump}");
    }
    let mut by_action: Vec<(&str, Option<&str>, &Value)> =
        entries.iter().map(|e| (e.action.as_str(), e.target.as_deref(), &e.detail)).collect();
    by_action.sort_by_key(|(a, ..)| *a);
    let adds: Vec<_> = by_action.iter().filter(|(a, ..)| *a == "todo.add").collect();
    assert_eq!(adds.len(), 2);
    let first = entries.iter().find(|e| e.target.as_deref() == Some(t.id.as_str()) && e.action == "todo.add").unwrap();
    assert_eq!(
        first.detail,
        json!({ "title_len": secret_title.chars().count(), "body_len": secret_body.len(), "project": "web", "source_kind": "local" })
    );
    assert_eq!(first.device_name, "local-cli");
    let agent_add =
        entries.iter().find(|e| e.target.as_deref() == Some(agent_item.id.as_str()) && e.action == "todo.add").unwrap();
    assert_eq!(agent_add.detail, json!({ "title_len": 3, "body_len": 0, "source_kind": "agent" }));
    assert_eq!(agent_add.device_name, "local-mcp");

    let updates: Vec<&AuditEntry> = entries.iter().filter(|e| e.action == "todo.update").collect();
    assert_eq!(updates.len(), 3);
    let u1 = updates.iter().find(|e| e.detail["fields"] == json!(["title", "body", "state"])).expect("first update");
    assert_eq!(u1.detail["state"], json!({ "from": "open", "to": "done" }));
    let u2 = updates.iter().find(|e| e.detail["fields"] == json!(["project", "state"])).expect("second update");
    assert_eq!(u2.detail["state"], json!({ "from": "done", "to": "dropped" }));
    let u3 = updates.iter().find(|e| e.target.as_deref() == Some(agent_item.id.as_str())).unwrap();
    assert_eq!(u3.detail, json!({ "fields": ["unreviewed"] }));
    let del = entries.iter().find(|e| e.action == "todo.delete").unwrap();
    assert_eq!((del.target.as_deref(), &del.detail), (Some(t.id.as_str()), &json!({})));
    td.shutdown().await;
}

#[tokio::test]
async fn failed_and_repeated_calls_leave_no_audit_rows() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    let _ = add(&td, &mut c, json!({ "id": id(), "title": "" })).await;
    let _ = update(&td, &mut c, json!({ "id": id(), "title": "x" })).await;
    let _ = c.call::<Value>(&td, methods::TODO_DELETE, json!({ "id": id() })).await;
    let _ = update(&td, &mut c, json!({ "id": t.id })).await;
    assert_eq!(audit(&td, "todo.").await.len(), 1, "just the one successful add");
    td.shutdown().await;
}

// ---------------------------------------------------------------- 保留期清理

#[tokio::test]
async fn retention_cleans_closed_items_from_their_closed_time() {
    let td = TestDaemon::start_with(&[("sessions.history_retention", "\"1h\"")], false).await;
    let mut c = td.local_cli_client();
    let mut watcher = td.hub_client("dev-w", "W");
    let now = anycon_daemon::util::now_ms();
    let hour = 3_600_000;

    // 关闭得早的（updated_ms 也早）、关闭得早但最近改过的（closed_ms 才是依据）、刚关闭的、没关闭但很旧的
    let old_closed = row(&id(), now - 5 * hour, TodoState::Done);
    let mut edited_since = row(&id(), now - 10, TodoState::Dropped);
    edited_since.closed_ms = Some(now - 2 * hour);
    let mut recent_closed = row(&id(), now - 10, TodoState::Done);
    recent_closed.closed_ms = Some(now - 10 * 60_000);
    let old_open = row(&id(), now - 100 * hour, TodoState::Open);
    let old_waiting = row(&id(), now - 100 * hour, TodoState::Waiting);
    for t in [&old_closed, &edited_since, &recent_closed, &old_open, &old_waiting] {
        td.ctx.store.todo_insert(t).await.unwrap();
    }

    td.ctx.sessions.cleanup().await;

    let left: Vec<String> = {
        let mut v: Vec<String> =
            list(&td, &mut c, json!({ "include_closed": true })).await.todos.into_iter().map(|t| t.id).collect();
        v.sort();
        v
    };
    let mut want = vec![recent_closed.id.clone(), old_open.id.clone(), old_waiting.id.clone()];
    want.sort();
    assert_eq!(left, want);
    // 删掉的项通知所有客户端
    let mut gone: Vec<String> = changes(&mut watcher).into_iter().filter(|n| n.deleted).map(|n| n.id).collect();
    gone.sort();
    let mut want_gone = vec![old_closed.id.clone(), edited_since.id.clone()];
    want_gone.sort();
    assert_eq!(gone, want_gone);
    td.shutdown().await;
}

#[tokio::test]
async fn forever_retention_keeps_everything() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    td.ctx.store.todo_insert(&row(&id(), 1, TodoState::Done)).await.unwrap();
    td.ctx.sessions.cleanup().await;
    assert_eq!(list(&td, &mut c, json!({ "include_closed": true })).await.todos.len(), 1);
    td.shutdown().await;
}

// ---------------------------------------------------------------- node.info

#[tokio::test]
async fn node_info_announces_the_capability() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.todo);
    assert_eq!(info.group, None, "no group by default");
    let raw: Value = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(raw.get("group").is_none(), "an empty group is left out of the wire format");
    td.shutdown().await;
}

#[tokio::test]
async fn the_table_survives_a_restart() {
    let mut td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "persisted", "body": "b", "project": "p", "due_ms": 7, "agent": true }),
    )
    .await
    .unwrap();
    td.restart().await;
    let mut c = td.local_cli_client();
    assert_eq!(list(&td, &mut c, json!({})).await.todos, vec![t]);
    td.shutdown().await;
}

// ---------------------------------------------------------------- id 规范、重开上限、并发

#[tokio::test]
async fn update_and_delete_reject_ids_that_are_not_canonical() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add_ok(&td, &mut c, "t").await;
    let upper = t.id.to_uppercase();
    let e = update(&td, &mut c, json!({ "id": upper, "title": "x" })).await.unwrap_err();
    assert_eq!(invalid_field(&e), "id");
    let e = c.call::<Value>(&td, methods::TODO_DELETE, json!({ "id": upper })).await.unwrap_err();
    assert_eq!(invalid_field(&e), "id");
    assert_eq!(list(&td, &mut c, json!({})).await.todos, vec![t], "nothing changed");
    td.shutdown().await;
}

#[tokio::test]
async fn reopening_a_closed_item_counts_against_the_open_cap() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let closed = id();
    td.ctx.store.todo_insert(&row(&closed, 1_000, TodoState::Done)).await.unwrap();
    let mut open_ids = Vec::new();
    for _ in 0..1000 {
        let i = id();
        td.ctx.store.todo_insert(&row(&i, 1_000, TodoState::Open)).await.unwrap();
        open_ids.push(i);
    }
    let e = update(&td, &mut c, json!({ "id": closed, "state": "open" })).await.unwrap_err();
    assert_eq!(
        (code(&e).0, code(&e).1.as_deref().map(str::to_string), code(&e).2),
        (ErrorCode::Invalid, Some("todos".into()), Some("too_many".into()))
    );
    // 别的修改、关闭状态之间的转换、开着的状态之间的转换不受限制
    update(&td, &mut c, json!({ "id": closed, "state": "dropped" })).await.unwrap();
    update(&td, &mut c, json!({ "id": open_ids[1], "state": "working" })).await.unwrap();
    assert_eq!(
        td.ctx.store.todo_get(&closed).await.unwrap().unwrap().state,
        TodoState::Dropped,
        "a refused reopen changed nothing"
    );
    // 999 个没关闭：可以重新打开
    update(&td, &mut c, json!({ "id": open_ids[0], "state": "done" })).await.unwrap();
    let r = update(&td, &mut c, json!({ "id": closed, "state": "open" })).await.unwrap();
    assert_eq!((r.state, r.closed_ms), (TodoState::Open, None));
    td.shutdown().await;
}

#[tokio::test]
async fn the_project_filter_is_trimmed_like_stored_names() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t = add(&td, &mut c, json!({ "id": id(), "title": "t", "project": "  web " })).await.unwrap();
    assert_eq!(t.project.as_deref(), Some("web"));
    for filter in ["web", " web", "web \t", "  web  "] {
        assert_eq!(list(&td, &mut c, json!({ "project": filter })).await.todos.len(), 1, "{filter:?}");
    }
    assert!(list(&td, &mut c, json!({ "project": "WEB" })).await.todos.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn the_cap_counts_use_the_partial_indexes() {
    let td = TestDaemon::start().await;
    for (plan_sql, index) in [
        ("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM todos WHERE state NOT IN ('done','dropped')", "todos_open"),
        ("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM todos WHERE unreviewed = 1", "todos_unreviewed"),
    ] {
        let rows = sqlx::query(plan_sql).fetch_all(td.ctx.store.pool()).await.unwrap();
        let plan: Vec<String> = rows.iter().map(|r| sqlx::Row::get::<String, _>(r, "detail")).collect();
        eprintln!("{plan_sql}\n  -> {plan:?}");
        assert!(plan.iter().any(|d| d.contains(index)), "{plan_sql}: {plan:?}");
    }
    td.shutdown().await;
}

#[tokio::test]
async fn an_mcp_declaration_is_a_guard_rail_the_connection_can_lift_itself() {
    let td = TestDaemon::start().await;
    let mut agent = td.local_mcp_client();
    let t = add(&td, &mut agent, json!({ "id": id(), "title": "from an agent" })).await.unwrap();
    // 声明了 mcp：reviewed 不起作用
    let r = update(&td, &mut agent, json!({ "id": t.id, "reviewed": true })).await.unwrap();
    assert!(r.unreviewed);
    // 同一条连接重新声明成 cli：和人敲的命令一样，能收下。这是接口上的护栏，不是边界（自报的）
    assert!(agent.handle.set_local_kind(anycon_daemon::clients::LocalClientKind::Cli));
    let r = update(&td, &mut agent, json!({ "id": t.id, "reviewed": true })).await.unwrap();
    assert!(!r.unreviewed);
    td.shutdown().await;
}

#[tokio::test]
async fn parallel_adds_of_one_id_make_exactly_one_item() {
    let td = TestDaemon::start().await;
    let mut watcher = td.hub_client("dev-w", "W");
    let new_id = id();
    let mut clients: Vec<TestClient> = (0..8)
        .map(|i| match i % 3 {
            0 => td.local_cli_client(),
            1 => td.local_mcp_client(),
            _ => td.hub_client(&format!("dev-{i}"), "P"),
        })
        .collect();
    let calls = clients.iter_mut().enumerate().map(|(i, c)| {
        let p = json!({ "id": new_id, "title": format!("racer {i}") });
        let td = &td;
        async move { add(td, c, p).await }
    });
    let results = futures::future::join_all(calls).await;
    let first = results[0].as_ref().unwrap().clone();
    assert!(results.iter().all(|r| r.as_ref().unwrap() == &first), "every caller sees the one stored item");
    let mut any = td.local_cli_client();
    assert_eq!(list(&td, &mut any, json!({})).await.todos.len(), 1);
    assert_eq!(audit(&td, "todo.add").await.len(), 1);
    assert_eq!(changes(&mut watcher).len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn parallel_adds_at_the_caps_let_exactly_one_through() {
    // 999 个没关闭的
    let td = TestDaemon::start().await;
    for _ in 0..999 {
        td.ctx.store.todo_insert(&row(&id(), 1_000, TodoState::Open)).await.unwrap();
    }
    let mut clients: Vec<TestClient> = (0..8).map(|_| td.local_cli_client()).collect();
    let calls = clients.iter_mut().map(|c| {
        let td = &td;
        async move { add(td, c, json!({ "id": id(), "title": "racer" })).await }
    });
    let results = futures::future::join_all(calls).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for e in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(code(e).2.as_deref(), Some("too_many"));
    }
    assert_eq!(td.ctx.store.todo_count_open().await.unwrap(), 1000);
    td.shutdown().await;

    // 99 个没被收下的
    let td = TestDaemon::start().await;
    for _ in 0..99 {
        let mut t = row(&id(), 1_000, TodoState::Open);
        t.unreviewed = true;
        t.source.kind = TodoSourceKind::Agent;
        td.ctx.store.todo_insert(&t).await.unwrap();
    }
    let mut clients: Vec<TestClient> = (0..8).map(|_| td.local_mcp_client()).collect();
    let calls = clients.iter_mut().map(|c| {
        let td = &td;
        async move { add(td, c, json!({ "id": id(), "title": "racer" })).await }
    });
    let results = futures::future::join_all(calls).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for e in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(code(e).2.as_deref(), Some("too_many_unreviewed"));
    }
    assert_eq!(td.ctx.store.todo_count_unreviewed().await.unwrap(), 100);
    td.shutdown().await;
}

#[tokio::test]
async fn an_unknown_source_kind_is_never_written_to_the_store() {
    let td = TestDaemon::start().await;
    let mut t = row(&id(), 5, TodoState::Open);
    t.source.kind = TodoSourceKind::Unknown;
    assert!(td.ctx.store.todo_insert(&t).await.is_err());
    assert!(td.ctx.store.todo_get(&t.id).await.unwrap().is_none(), "nothing was stored");
    td.shutdown().await;
}
