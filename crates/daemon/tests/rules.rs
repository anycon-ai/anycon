//! 决策档案（`rule.*`，docs/spec/daemon.md § 23）端到端：校验与错误码、幂等、空操作、`if_updated_ms`、条数和字数上限、
//! 谁能写、`rule.changed`、审计、保留期、注入（派发的会话、手动的会话、agent 不支持、16 KiB、快照、`last_injected_ms`）、
//! `dispatched` 事件、`dispatch_preview.rules_text`、`rule.preview`。规则的文字不进日志的检查在 `rules_logs.rs`。

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anycon_daemon::rules::{BUDGET_CAP, TEMPLATE_HEADER};
use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn code(e: &RpcError) -> (ErrorCode, Option<String>, Option<String>) {
    let d = e.data.clone().expect("error data");
    (d.code, d.field, d.reason)
}

fn invalid(field: &str, reason: Option<&str>) -> (ErrorCode, Option<String>, Option<String>) {
    (ErrorCode::Invalid, Some(field.into()), reason.map(str::to_string))
}

async fn add(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Rule, RpcError> {
    c.call(td, methods::RULE_ADD, params).await
}

async fn add_ok(td: &TestDaemon, c: &mut TestClient, text: &str) -> Rule {
    add(td, c, json!({ "id": id(), "text": text })).await.unwrap()
}

async fn add_in(td: &TestDaemon, c: &mut TestClient, text: &str, project: &str) -> Rule {
    add(td, c, json!({ "id": id(), "text": text, "project": project })).await.unwrap()
}

async fn update(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Rule, RpcError> {
    c.call(td, methods::RULE_UPDATE, params).await
}

async fn delete(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Empty, RpcError> {
    c.call(td, methods::RULE_DELETE, params).await
}

async fn list(td: &TestDaemon, c: &mut TestClient, params: Value) -> RuleListResult {
    c.call(td, methods::RULE_LIST, params).await.unwrap()
}

async fn rule_of(td: &TestDaemon, c: &mut TestClient, id: &str) -> Rule {
    list(td, c, json!({})).await.rules.into_iter().find(|r| r.id == id).expect("the rule exists")
}

async fn preview(td: &TestDaemon, c: &mut TestClient, project: Option<&str>) -> RulePreviewResult {
    let p = match project {
        Some(p) => json!({ "project": p }),
        None => json!({}),
    };
    c.call(td, methods::RULE_PREVIEW, p).await.unwrap()
}

fn changes(c: &mut TestClient) -> Vec<RuleChanged> {
    c.drain_now()
        .into_iter()
        .filter(|n| n.method == methods::RULE_CHANGED)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect()
}

async fn audit(td: &TestDaemon, prefix: &str) -> Vec<AuditEntry> {
    td.ctx.audit.query(None, None, 1000).await.into_iter().filter(|e| e.action.starts_with(prefix)).collect()
}

/// 直接写库的一条（造大量数据和固定时间戳用）。
fn row(id: &str, text: &str, project: Option<&str>, status: RuleStatus, created_ms: u64) -> Rule {
    Rule {
        id: id.into(),
        text: text.into(),
        project: project.map(str::to_string),
        status,
        sync: RuleSync::Node,
        source: RuleSource {
            kind: RuleSourceKind::Manual,
            session_id: None,
            seq: None,
            request_id: None,
            device_id: None,
        },
        created_ms,
        updated_ms: created_ms,
        last_injected_ms: None,
    }
}

/// 项目里的一条 600 个字符的规则占的字节数：`\n- ` 加上文字。
const ITEM_600: usize = 603;

fn header() -> usize {
    TEMPLATE_HEADER.len()
}

// ---------------------------------------------------------------- 校验

#[tokio::test]
async fn add_validates_the_id_the_text_the_project_and_the_enums() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();

    // id：小写带连字符的 UUID，不做规范化
    let upper = id().to_uppercase();
    for bad in
        ["", "abc", "{00000000-0000-0000-0000-000000000000}", "urn:uuid:00000000-0000-0000-0000-000000000000", &upper]
    {
        let e = add(&td, &mut c, json!({ "id": bad, "text": "t" })).await.unwrap_err();
        assert_eq!(code(&e), invalid("id", None), "{bad}");
    }
    let no_hyphens = id().replace('-', "");
    let e = add(&td, &mut c, json!({ "id": no_hyphens, "text": "t" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("id", None));

    // text：1–600 个码点（去掉首尾空白之后），可以多行，除换行外没有控制字符，没有 U+2028 / U+2029
    for bad in ["", "   ", "\n\n"] {
        let e = add(&td, &mut c, json!({ "id": id(), "text": bad })).await.unwrap_err();
        assert_eq!(code(&e), invalid("text", None), "{bad:?}");
    }
    let e = add(&td, &mut c, json!({ "id": id(), "text": "a".repeat(601) })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", None));
    let e = add(&td, &mut c, json!({ "id": id(), "text": "中".repeat(601) })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", None), "characters, not bytes");
    for bad in ["a\tb", "a\rb", "a\r\nb", "a\u{0}b", "a\u{1b}[31mb", "a\u{7f}b", "a\u{85}b", "a\u{2028}b", "a\u{2029}b"]
    {
        let e = add(&td, &mut c, json!({ "id": id(), "text": bad })).await.unwrap_err();
        assert_eq!(code(&e), invalid("text", None), "{bad:?}");
    }
    let ok600 = add_ok(&td, &mut c, &"a".repeat(600)).await;
    assert_eq!(ok600.text.len(), 600);
    // 600 个汉字是 1800 字节，码点数没超
    assert_eq!(add_ok(&td, &mut c, &"中".repeat(600)).await.text.chars().count(), 600);
    // 多行照存，首尾空白去掉
    let multi = add_ok(&td, &mut c, "  first line\n\n  second line\n").await;
    assert_eq!(multi.text, "first line\n\n  second line");

    // project：同 `todo.add`：去掉首尾空白，空白按没给，1–200 个码点，没有控制字符
    for bad in [json!("a\nb"), json!("a".repeat(201)), json!("a\u{2028}b")] {
        let e = add(&td, &mut c, json!({ "id": id(), "text": "t", "project": bad })).await.unwrap_err();
        assert_eq!(code(&e), invalid("project", None));
    }
    let r = add(&td, &mut c, json!({ "id": id(), "text": "t", "project": "  " })).await.unwrap();
    assert_eq!(r.project, None);
    let r = add(&td, &mut c, json!({ "id": id(), "text": "t", "project": "  web  " })).await.unwrap();
    assert_eq!(r.project.as_deref(), Some("web"));
    // 项目不存在也可以：不校验，项目之后才出现的话规则就生效
    assert!(!td.workspace.join("web").exists());

    // status / source 只接受调用方可以给的值
    for bad in [
        json!({ "status": "disabled" }),
        json!({ "status": "rejected" }),
        json!({ "status": "unknown" }),
        json!({ "status": "bogus" }),
        json!({ "source": { "kind": "extracted" } }),
        json!({ "source": { "kind": "agent" } }),
        json!({ "source": { "kind": "bogus" } }),
    ] {
        let mut p = json!({ "id": id(), "text": "t" });
        for (k, v) in bad.as_object().unwrap() {
            p[k] = v.clone();
        }
        let e = add(&td, &mut c, p.clone()).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid), "{p}");
    }
    // 出处里的 id 不能为空、超长或带控制字符
    for bad in [json!(""), json!("x".repeat(129)), json!("a\nb")] {
        let e = add(
            &td,
            &mut c,
            json!({ "id": id(), "text": "t", "source": { "kind": "answer", "session_id": bad, "request_id": "r" } }),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), invalid("source.session_id", None));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn update_and_delete_validate_the_id_and_the_fields() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = add_ok(&td, &mut c, "keep it short").await;

    let e = update(&td, &mut c, json!({ "id": "nope", "text": "t" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("id", None));
    let e = delete(&td, &mut c, json!({ "id": "nope" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("id", None));
    let e = update(&td, &mut c, json!({ "id": id(), "text": "t" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    let e = delete(&td, &mut c, json!({ "id": id() })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));

    let e = update(&td, &mut c, json!({ "id": r.id, "text": "  " })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", None));
    let e = update(&td, &mut c, json!({ "id": r.id, "text": "a\tb" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", None));
    let e = update(&td, &mut c, json!({ "id": r.id, "project": "  " })).await.unwrap_err();
    assert_eq!(code(&e), invalid("project", None));
    // 一个这个版本不认识的 status 解析成 `unknown`，daemon 不接受它作为写入的值
    let e = update(&td, &mut c, json!({ "id": r.id, "status": "bogus" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("status", None));
    // 失败的修改什么都没改
    assert_eq!(rule_of(&td, &mut c, &r.id).await, r);
    td.shutdown().await;
}

// ---------------------------------------------------------------- 幂等、空操作、版本

#[tokio::test]
async fn add_is_idempotent_by_id_before_any_other_validation() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut watcher = td.hub_client("dev-w", "W");
    let rid = id();
    let first = add(&td, &mut c, json!({ "id": rid, "text": "the first text", "project": "web" })).await.unwrap();
    assert_eq!(changes(&mut watcher).len(), 1);

    // 重发（回执丢了）：原样返回已有的，哪怕这次的参数不一样、甚至不合法，也哪怕调用方是 agent
    let again = add(&td, &mut c, json!({ "id": rid, "text": "a different text" })).await.unwrap();
    assert_eq!(again, first);
    let again = add(&td, &mut c, json!({ "id": rid, "text": "" })).await.unwrap();
    assert_eq!(again, first);
    let mut agent = td.local_mcp_client();
    let again = add(&td, &mut agent, json!({ "id": rid, "text": "an agent says so", "agent": true })).await.unwrap();
    assert_eq!(again, first);

    assert_eq!(list(&td, &mut c, json!({})).await.rules, vec![first]);
    // 重复的 add 不进审计、不通知
    assert_eq!(audit(&td, "rule.").await.len(), 1);
    assert!(changes(&mut watcher).is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn an_update_that_changes_nothing_is_a_no_op() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = add_in(&td, &mut c, "same text", "web").await;
    let mut watcher = td.hub_client("dev-w", "W");

    for same in [
        json!({ "id": r.id }),
        json!({ "id": r.id, "text": "same text" }),
        json!({ "id": r.id, "text": "  same text  " }),
        json!({ "id": r.id, "project": "web" }),
        json!({ "id": r.id, "status": "active" }),
        json!({ "id": r.id, "text": "same text", "project": " web ", "status": "active", "if_updated_ms": r.updated_ms }),
    ] {
        let got = update(&td, &mut c, same.clone()).await.unwrap();
        assert_eq!(got, r, "{same}");
    }
    // 清空本来就没有的字段也是空操作
    let node_wide = add_ok(&td, &mut c, "node wide").await;
    let got = update(&td, &mut c, json!({ "id": node_wide.id, "clear": ["project"] })).await.unwrap();
    assert_eq!(got, node_wide);
    let _ = changes(&mut watcher);
    let before = audit(&td, "rule.").await.len();
    update(&td, &mut c, json!({ "id": r.id, "text": "same text" })).await.unwrap();
    assert!(changes(&mut watcher).is_empty());
    assert_eq!(audit(&td, "rule.").await.len(), before);
    assert_eq!(rule_of(&td, &mut c, &r.id).await.updated_ms, r.updated_ms);
    td.shutdown().await;
}

#[tokio::test]
async fn updated_ms_increases_strictly_on_every_real_change() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = add_ok(&td, &mut c, "v0").await;
    let mut last = r.updated_ms;
    for i in 1..=12 {
        let u = update(&td, &mut c, json!({ "id": r.id, "text": format!("v{i}") })).await.unwrap();
        assert!(u.updated_ms > last, "change {i}: {} is not after {last}", u.updated_ms);
        last = u.updated_ms;
    }
    // 状态、项目的修改同样
    let u = update(&td, &mut c, json!({ "id": r.id, "status": "disabled" })).await.unwrap();
    assert!(u.updated_ms > last);
    last = u.updated_ms;
    let u = update(&td, &mut c, json!({ "id": r.id, "project": "web" })).await.unwrap();
    assert!(u.updated_ms > last);
    // 时钟往回拨过（库里的 updated_ms 在「未来」）：仍然严格递增
    let future = anycon_daemon::util::now_ms() + 3_600_000;
    let mut ahead = td.ctx.store.rule_get(&r.id).await.unwrap().unwrap();
    ahead.updated_ms = future;
    td.ctx.store.rule_save(&ahead).await.unwrap();
    let u = update(&td, &mut c, json!({ "id": r.id, "text": "after the clock jump" })).await.unwrap();
    assert_eq!(u.updated_ms, future + 1);
    // created_ms 不变
    assert_eq!(u.created_ms, r.created_ms);
    td.shutdown().await;
}

#[tokio::test]
async fn if_updated_ms_makes_update_and_delete_conditional() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut watcher = td.hub_client("dev-w", "W");
    let r = add_ok(&td, &mut c, "original").await;
    let u = update(&td, &mut c, json!({ "id": r.id, "text": "edited elsewhere" })).await.unwrap();
    let _ = changes(&mut watcher);
    let audits = audit(&td, "rule.").await.len();

    // 过期的版本：什么都不改，`invalid` / `changed`，带现在的 `updated_ms`
    let e =
        update(&td, &mut c, json!({ "id": r.id, "text": "mine", "if_updated_ms": r.updated_ms })).await.unwrap_err();
    assert_eq!(code(&e), invalid("if_updated_ms", Some("changed")));
    assert_eq!(e.data.as_ref().unwrap().updated_ms, Some(u.updated_ms));
    // 就算这次的修改本来是空操作，版本不对也算失败
    let e = update(&td, &mut c, json!({ "id": r.id, "text": "edited elsewhere", "if_updated_ms": r.updated_ms }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), invalid("if_updated_ms", Some("changed")));
    let e = delete(&td, &mut c, json!({ "id": r.id, "if_updated_ms": r.updated_ms })).await.unwrap_err();
    assert_eq!(code(&e), invalid("if_updated_ms", Some("changed")));
    assert_eq!(e.data.as_ref().unwrap().updated_ms, Some(u.updated_ms));
    assert_eq!(rule_of(&td, &mut c, &r.id).await.text, "edited elsewhere");
    assert!(changes(&mut watcher).is_empty());
    assert_eq!(audit(&td, "rule.").await.len(), audits);

    // 对的版本：生效
    let u2 = update(&td, &mut c, json!({ "id": r.id, "text": "mine", "if_updated_ms": u.updated_ms })).await.unwrap();
    assert_eq!(u2.text, "mine");
    delete(&td, &mut c, json!({ "id": r.id, "if_updated_ms": u2.updated_ms })).await.unwrap();
    assert!(list(&td, &mut c, json!({})).await.rules.is_empty());
    // 不存在的规则带条件也是 not_found
    let e = delete(&td, &mut c, json!({ "id": r.id, "if_updated_ms": 1 })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    // 没给条件就无条件
    let r2 = add_ok(&td, &mut c, "another").await;
    update(&td, &mut c, json!({ "id": r2.id, "text": "changed" })).await.unwrap();
    delete(&td, &mut c, json!({ "id": r2.id })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn update_merges_fields_and_clear_wins_after_the_merge() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = add_in(&td, &mut c, "scoped", "web").await;
    let u = update(&td, &mut c, json!({ "id": r.id, "clear": ["project"] })).await.unwrap();
    assert_eq!(u.project, None);
    assert_eq!(u.text, "scoped");
    let u = update(&td, &mut c, json!({ "id": r.id, "project": "api" })).await.unwrap();
    assert_eq!(u.project.as_deref(), Some("api"));
    let u =
        update(&td, &mut c, json!({ "id": r.id, "project": "web", "clear": ["project"], "text": "x" })).await.unwrap();
    assert_eq!((u.project, u.text.as_str()), (None, "x"));
    // 每个状态都可以互相转（人的操作）
    for s in ["pending", "rejected", "active", "disabled", "pending", "active"] {
        let u = update(&td, &mut c, json!({ "id": r.id, "status": s })).await.unwrap();
        assert_eq!(serde_json::to_value(u.status).unwrap(), json!(s));
    }
    td.shutdown().await;
}

// ---------------------------------------------------------------- 上限

#[tokio::test]
async fn at_most_300_rules_in_any_status() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let statuses = [RuleStatus::Pending, RuleStatus::Disabled, RuleStatus::Active, RuleStatus::Disabled];
    for i in 0..300u64 {
        td.ctx.store.rule_insert(&row(&id(), "x", None, statuses[i as usize % 4], i + 1)).await.unwrap();
    }
    let e = add(&td, &mut c, json!({ "id": id(), "text": "one too many" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("rules", Some("too_many")));
    // 重发已有的 id 不受影响
    let existing = list(&td, &mut c, json!({})).await.rules[0].clone();
    assert_eq!(add(&td, &mut c, json!({ "id": existing.id, "text": "whatever" })).await.unwrap(), existing);
    // agent 的待确认规则也占名额（这里是总数先到）
    let e = add(&td, &mut c, json!({ "id": id(), "text": "x", "agent": true })).await.unwrap_err();
    assert_eq!(code(&e), invalid("rules", Some("too_many")));
    // 删一条就又能加
    delete(&td, &mut c, json!({ "id": existing.id })).await.unwrap();
    add_ok(&td, &mut c, "fits now").await;
    assert_eq!(list(&td, &mut c, json!({})).await.rules.len(), 300);
    td.shutdown().await;
}

/// 直接写库的一条别的来源的规则。
fn row_from(id: &str, status: RuleStatus, kind: RuleSourceKind, created_ms: u64) -> Rule {
    let mut r = row(id, "x", None, status, created_ms);
    r.source.kind = kind;
    r
}

#[tokio::test]
async fn agents_cannot_fill_the_list_with_pending_rules_and_lock_the_person_out() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    // 50 条 agent 提的（一半是提炼出来的）待确认规则：到上限
    for i in 0..50u64 {
        let kind = if i % 2 == 0 { RuleSourceKind::Agent } else { RuleSourceKind::Extracted };
        td.ctx.store.rule_insert(&row_from(&id(), RuleStatus::Pending, kind, i + 1)).await.unwrap();
    }
    for c in [&mut cli, &mut mcp] {
        let e = add(&td, c, json!({ "id": id(), "text": "one more", "agent": true })).await.unwrap_err();
        assert_eq!(code(&e), invalid("rules", Some("too_many_pending")));
    }
    // 人照常能写：待确认的也行（来源是 manual，不算）、直接生效的也行
    add(&td, &mut cli, json!({ "id": id(), "text": "mine", "status": "pending" })).await.unwrap();
    let mine = add_ok(&td, &mut cli, "mine too").await;
    assert_eq!(mine.status, RuleStatus::Active);
    // 重发已有的 id 不受影响
    let existing = list(&td, &mut cli, json!({})).await.rules[0].clone();
    assert_eq!(add(&td, &mut cli, json!({ "id": existing.id, "text": "x", "agent": true })).await.unwrap(), existing);
    // 人否决、确认一条之后，agent 又有名额
    update(&td, &mut cli, json!({ "id": existing.id, "status": "rejected" })).await.unwrap();
    let a = add(&td, &mut mcp, json!({ "id": id(), "text": "fits again" })).await.unwrap();
    assert_eq!(a.status, RuleStatus::Pending);
    let e = add(&td, &mut mcp, json!({ "id": id(), "text": "full again" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("rules", Some("too_many_pending")));
    // 总数还剩很多：人没被挡住
    assert!(list(&td, &mut cli, json!({})).await.rules.len() < 300);
    td.shutdown().await;
}

#[tokio::test]
async fn rejected_rules_do_not_count_toward_the_300_and_have_a_cap_of_their_own() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut watcher = td.hub_client("dev-w", "W");
    // 300 条否决的不占那 300 个名额
    let mut rejected = Vec::new();
    for i in 0..300u64 {
        let r = row(&id(), "old", None, RuleStatus::Rejected, i + 1);
        td.ctx.store.rule_insert(&r).await.unwrap();
        rejected.push(r.id);
    }
    // 另有生效、停用、待确认的各一条，自动删除永远不能碰它们（它们的时间比否决的还早）
    let keep_active = add_ok(&td, &mut c, "keep active").await;
    let keep_off = add_ok(&td, &mut c, "keep disabled").await;
    update(&td, &mut c, json!({ "id": keep_off.id, "status": "disabled" })).await.unwrap();
    let keep_pending =
        add(&td, &mut c, json!({ "id": id(), "text": "keep pending", "status": "pending" })).await.unwrap();
    let _ = changes(&mut watcher);
    let before = audit(&td, "rule.delete").await.len();

    // 再否决一条：最早否决的（updated_ms 最小）被删掉，通知所有客户端，审计里记着是自动的
    update(&td, &mut c, json!({ "id": keep_active.id, "status": "rejected" })).await.unwrap();
    let ids: Vec<String> = list(&td, &mut c, json!({})).await.rules.into_iter().map(|r| r.id).collect();
    assert!(!ids.contains(&rejected[0]), "the oldest rejected rule is gone");
    assert!(ids.contains(&rejected[1]) && ids.contains(&keep_active.id));
    assert!(ids.contains(&keep_off.id) && ids.contains(&keep_pending.id));
    let rej = list(&td, &mut c, json!({ "status": "rejected" })).await.rules.len();
    assert_eq!(rej, 300);
    let gone: Vec<_> = changes(&mut watcher).into_iter().filter(|n| n.deleted).map(|n| n.id).collect();
    assert_eq!(gone, vec![rejected[0].clone()]);
    let d = audit(&td, "rule.delete").await;
    assert_eq!(d.len(), before + 1);
    assert_eq!(d[0].detail, json!({ "auto": "rejected_cap" }));
    // 否决的和别的合计超过 300 也照样能加：只有不含否决的才占名额
    for i in 0..10 {
        add_ok(&td, &mut c, &format!("fine {i}")).await;
    }
    // 把否决的改回别的状态要占名额：名额满了就不行
    td.shutdown().await;
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = row(&id(), "old", None, RuleStatus::Rejected, 1);
    td.ctx.store.rule_insert(&r).await.unwrap();
    for i in 0..300u64 {
        td.ctx.store.rule_insert(&row(&id(), "x", None, RuleStatus::Disabled, i + 2)).await.unwrap();
    }
    let e = update(&td, &mut c, json!({ "id": r.id, "status": "disabled" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("rules", Some("too_many")));
    assert_eq!(rule_of(&td, &mut c, &r.id).await.status, RuleStatus::Rejected);
    td.shutdown().await;
}

#[tokio::test]
async fn invisible_characters_are_refused_in_a_rule() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    for bad in [
        "a\u{202A}b",
        "a\u{202E}b",
        "a\u{2066}b",
        "a\u{2069}b",
        "a\u{200E}b",
        "a\u{200F}b",
        "a\u{061C}b",
        "a\u{200B}b",
        "a\u{200C}b",
        "a\u{200D}b",
        "a\u{2060}b",
        "a\u{FEFF}b",
        "\u{200B}lead",
        "tail\u{202E}",
    ] {
        let e = add(&td, &mut c, json!({ "id": id(), "text": bad })).await.unwrap_err();
        assert_eq!(code(&e), invalid("text", Some("invisible")), "{bad:?}");
    }
    let r = add_ok(&td, &mut c, "ok 中文 emoji 🙂 and a normal line\nbreak").await;
    let e = update(&td, &mut c, json!({ "id": r.id, "text": "now\u{200B}bad" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("invisible")));
    assert_eq!(rule_of(&td, &mut c, &r.id).await.text, r.text);
    td.shutdown().await;
}

#[tokio::test]
async fn an_agent_rule_records_the_session_that_proposed_it_when_that_session_exists() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": "m1" }))
        .await
        .unwrap();
    let r = add(&td, &mut c, json!({ "id": id(), "text": "t", "agent": true, "origin_session": s.id })).await.unwrap();
    assert_eq!((r.source.kind, r.source.session_id.as_deref()), (RuleSourceKind::Agent, Some(s.id.as_str())));
    // 没有这个会话：丢掉这个字段，照常建
    let r = add(&td, &mut c, json!({ "id": id(), "text": "t", "agent": true, "origin_session": "sess_nope" }))
        .await
        .unwrap();
    assert_eq!((r.source.kind, r.source.session_id), (RuleSourceKind::Agent, None));
    // 自报了 mcp 的连接同样
    let mut mcp = td.local_mcp_client();
    let r = add(&td, &mut mcp, json!({ "id": id(), "text": "t", "origin_session": s.id })).await.unwrap();
    assert_eq!(r.source.session_id.as_deref(), Some(s.id.as_str()));
    // 人写的规则不记：它不是 agent 提的
    let r = add(&td, &mut c, json!({ "id": id(), "text": "t", "origin_session": s.id })).await.unwrap();
    assert_eq!((r.source.kind, r.source.session_id), (RuleSourceKind::Manual, None));
    td.shutdown().await;
}

#[tokio::test]
async fn a_node_wide_rule_counts_against_the_largest_project_not_just_its_own_total() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t600 = "x".repeat(600);
    for _ in 0..3 {
        add_ok(&td, &mut c, &t600).await;
    }
    for _ in 0..3 {
        add_in(&td, &mut c, &t600, "alpha").await;
    }
    add_in(&td, &mut c, &t600, "beta").await;

    let l = list(&td, &mut c, json!({})).await;
    assert_eq!(l.budget.cap, BUDGET_CAP as u64);
    assert_eq!(l.budget.node_bytes as usize, header() + 3 * ITEM_600);
    assert_eq!(l.budget.projects.get("alpha"), Some(&(3 * ITEM_600 as u64)));
    assert_eq!(l.budget.projects.get("beta"), Some(&(ITEM_600 as u64)));
    // 一个 alpha 的会话拿到的文字就是 node_bytes + alpha，一字节不差
    let p = preview(&td, &mut c, Some("alpha")).await;
    assert_eq!(p.bytes, l.budget.node_bytes + l.budget.projects["alpha"]);

    // 第 4 条全节点规则：全节点的 4 条 + 最大的项目（alpha）的 3 条放不下，尽管它和 beta 一起放得下
    let e = add(&td, &mut c, json!({ "id": id(), "text": t600 })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    let d = e.data.unwrap();
    assert_eq!((d.bytes, d.cap), (Some((header() + 7 * ITEM_600) as u64), Some(BUDGET_CAP as u64)));
    assert!(header() + 4 * ITEM_600 + ITEM_600 <= BUDGET_CAP, "beta alone would have fit");

    // 刚好用满上限的那一条放得下，多一个字节就不行
    let room = BUDGET_CAP - (header() + 3 * ITEM_600 + 3 * ITEM_600);
    add_ok(&td, &mut c, &"y".repeat(room - 3)).await;
    let e = add(&td, &mut c, json!({ "id": id(), "text": "z" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    assert_eq!(e.data.unwrap().bytes, Some(BUDGET_CAP as u64 + 4));
    td.shutdown().await;
}

#[tokio::test]
async fn a_project_rule_counts_against_the_node_wide_total_plus_its_own_project_only() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t600 = "x".repeat(600);
    for _ in 0..3 {
        add_ok(&td, &mut c, &t600).await;
    }
    for _ in 0..3 {
        add_in(&td, &mut c, &t600, "alpha").await;
    }
    // alpha 满了；beta 不受 alpha 影响，到它自己也满的时候才拒绝
    let e = add(&td, &mut c, json!({ "id": id(), "text": t600, "project": "alpha" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    assert_eq!(e.data.unwrap().bytes, Some((header() + 6 * ITEM_600 + ITEM_600) as u64));
    for _ in 0..3 {
        add_in(&td, &mut c, &t600, "beta").await;
    }
    let e = add(&td, &mut c, json!({ "id": id(), "text": t600, "project": "beta" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    // 项目名的写法不同但是同一个项目（去掉首尾空白）归在一起
    let e = add(&td, &mut c, json!({ "id": id(), "text": t600, "project": "  beta " })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    // 别的项目、没有项目的会话不受影响
    add_in(&td, &mut c, "short", "gamma").await;
    td.shutdown().await;
}

#[tokio::test]
async fn chinese_text_is_measured_in_bytes() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    // 100 个汉字是 300 字节，不是 100
    let r = add_ok(&td, &mut c, &"中".repeat(100)).await;
    let l = list(&td, &mut c, json!({})).await;
    assert_eq!(l.budget.node_bytes as usize, header() + 3 + 300);
    assert_eq!(preview(&td, &mut c, None).await.bytes, l.budget.node_bytes);
    delete(&td, &mut c, json!({ "id": r.id })).await.unwrap();

    // 600 个汉字 = 1800 字节：两条放得下，第三条放不下（尽管只有 1800 个字符）
    let t = "中".repeat(600);
    let item = 3 + 1800;
    add_ok(&td, &mut c, &t).await;
    add_ok(&td, &mut c, &t).await;
    let e = add(&td, &mut c, json!({ "id": id(), "text": t })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    assert_eq!(e.data.unwrap().bytes, Some((header() + 3 * item) as u64));
    td.shutdown().await;
}

#[tokio::test]
async fn the_budget_is_checked_when_a_rule_becomes_active_and_when_an_active_rule_changes() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let t600 = "x".repeat(600);
    // 不生效的规则不占字数：待确认、停用、否决的随便放
    let mut parked = Vec::new();
    for (i, s) in
        ["pending", "pending", "pending", "pending", "pending", "pending", "pending", "pending"].iter().enumerate()
    {
        let r = add(&td, &mut c, json!({ "id": id(), "text": t600, "status": s, "project": format!("p{i}") }))
            .await
            .unwrap();
        parked.push(r);
    }
    assert_eq!(list(&td, &mut c, json!({})).await.budget.node_bytes, 0, "nothing is active: no template either");
    assert_eq!(list(&td, &mut c, json!({})).await.budget.projects.len(), 0);
    assert_eq!(
        preview(&td, &mut c, Some("p0")).await,
        RulePreviewResult { text: String::new(), rule_ids: vec![], bytes: 0 }
    );

    // 确认（pending → active）逐条检查：一个项目里放得下 6 条
    for r in parked.iter().take(6) {
        update(&td, &mut c, json!({ "id": r.id, "project": "same" })).await.unwrap();
    }
    for r in parked.iter().take(6) {
        update(&td, &mut c, json!({ "id": r.id, "status": "active" })).await.unwrap();
    }
    // 第 7 条：确认被拒绝，状态不变
    update(&td, &mut c, json!({ "id": parked[6].id, "project": "same" })).await.unwrap();
    let e = update(&td, &mut c, json!({ "id": parked[6].id, "status": "active" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("status", Some("over_budget")));
    assert_eq!(e.data.unwrap().bytes, Some((header() + 7 * ITEM_600) as u64));
    assert_eq!(rule_of(&td, &mut c, &parked[6].id).await.status, RuleStatus::Pending);

    // 停用一条之后就能确认
    update(&td, &mut c, json!({ "id": parked[0].id, "status": "disabled" })).await.unwrap();
    update(&td, &mut c, json!({ "id": parked[6].id, "status": "active" })).await.unwrap();
    // 再把停用的那条打开：又超了
    let e = update(&td, &mut c, json!({ "id": parked[0].id, "status": "active" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("status", Some("over_budget")));
    // 否决的也一样不能直接生效
    update(&td, &mut c, json!({ "id": parked[0].id, "status": "rejected" })).await.unwrap();
    let e = update(&td, &mut c, json!({ "id": parked[0].id, "status": "active" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("status", Some("over_budget")));

    // 生效的规则改成更长的文字：超了就拒绝；更短的、不变的没问题
    let r = add_in(&td, &mut c, "short", "other").await;
    let big = "y".repeat(600);
    update(&td, &mut c, json!({ "id": r.id, "text": big })).await.unwrap();
    // 把它挪进已经满了的项目：拒绝
    let e = update(&td, &mut c, json!({ "id": r.id, "project": "same" })).await.unwrap_err();
    assert_eq!(code(&e), invalid("project", Some("over_budget")));
    assert_eq!(rule_of(&td, &mut c, &r.id).await.project.as_deref(), Some("other"));
    // 清掉项目变成全节点规则：全节点 + 最大的项目放不下，拒绝
    let e = update(&td, &mut c, json!({ "id": r.id, "clear": ["project"] })).await.unwrap_err();
    assert_eq!(code(&e), invalid("project", Some("over_budget")));
    // 改短没问题
    update(&td, &mut c, json!({ "id": parked[6].id, "text": "now short" })).await.unwrap();
    // 没有生效的规则（待确认的）改文字不检查
    let pend = add(&td, &mut c, json!({ "id": id(), "text": "p", "status": "pending" })).await.unwrap();
    update(&td, &mut c, json!({ "id": pend.id, "text": "z".repeat(600), "project": "same" })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn growing_the_text_of_an_active_rule_is_checked_too() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    for _ in 0..6 {
        add_ok(&td, &mut c, &"x".repeat(600)).await;
    }
    let small = add_ok(&td, &mut c, "short").await;
    // 7 条 600 字符的放不下
    let e = update(&td, &mut c, json!({ "id": small.id, "text": "x".repeat(600) })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    assert_eq!(e.data.unwrap().bytes, Some((header() + 7 * ITEM_600) as u64));
    assert_eq!(rule_of(&td, &mut c, &small.id).await, small, "refused: nothing changed");
    // 放得下的长度可以；刚好用满上限的可以，多一个字节不行
    let room = BUDGET_CAP - (header() + 6 * ITEM_600);
    let u = update(&td, &mut c, json!({ "id": small.id, "text": "y".repeat(room - 3) })).await.unwrap();
    assert_eq!(u.text.len(), room - 3);
    let e = update(&td, &mut c, json!({ "id": small.id, "text": "y".repeat(room - 2) })).await.unwrap_err();
    assert_eq!(code(&e), invalid("text", Some("over_budget")));
    // 改短永远可以
    update(&td, &mut c, json!({ "id": small.id, "text": "short again" })).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- 谁能写

#[tokio::test]
async fn an_agent_flagged_add_is_pending_with_an_agent_source_whatever_it_asks_for() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut hub = td.hub_client("dev-pixel", "Pixel");
    let r = add(
        &td,
        &mut cli,
        json!({ "id": id(), "text": "from an agent", "agent": true, "status": "active", "source": { "kind": "manual" } }),
    )
    .await
    .unwrap();
    assert_eq!((r.status, r.source.kind), (RuleStatus::Pending, RuleSourceKind::Agent));
    assert_eq!(r.source.device_id, None, "a local connection has no device id");
    assert_eq!(r.sync, RuleSync::Node);

    // 经 hub：带设备 id
    let r = add(&td, &mut hub, json!({ "id": id(), "text": "via hub", "agent": true })).await.unwrap();
    assert_eq!(
        (r.status, r.source.kind, r.source.device_id.as_deref()),
        (RuleStatus::Pending, RuleSourceKind::Agent, Some("dev-pixel"))
    );
    // `agent: false` 不改变什么：这是一个人的写入
    let r = add(&td, &mut hub, json!({ "id": id(), "text": "a person", "agent": false })).await.unwrap();
    assert_eq!(
        (r.status, r.source.kind, r.source.device_id.as_deref()),
        (RuleStatus::Active, RuleSourceKind::Manual, Some("dev-pixel"))
    );
    // 不在注入里
    let p = preview(&td, &mut cli, None).await;
    assert_eq!(p.rule_ids, vec![r.id.clone()]);
    td.shutdown().await;
}

#[tokio::test]
async fn a_connection_that_declared_mcp_can_only_add_and_only_pending() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    let human = add_ok(&td, &mut cli, "a person wrote this").await;

    // 不带 `agent` 也是 agent 写的
    let r = add(&td, &mut mcp, json!({ "id": id(), "text": "declared by mcp", "status": "active" })).await.unwrap();
    assert_eq!((r.status, r.source.kind), (RuleStatus::Pending, RuleSourceKind::Agent));
    // 改和删：forbidden（自己的、别人的都一样）
    for target in [&r.id, &human.id] {
        let e = update(&td, &mut mcp, json!({ "id": target, "status": "active" })).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
        let e = delete(&td, &mut mcp, json!({ "id": target })).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
    }
    // 不存在的 id 也是 forbidden，不是 not_found：不让它探测
    let e = update(&td, &mut mcp, json!({ "id": id(), "text": "t" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
    assert_eq!(rule_of(&td, &mut cli, &r.id).await, r);
    // 读是允许的
    assert_eq!(list(&td, &mut mcp, json!({})).await.rules.len(), 2);
    assert_eq!(preview(&td, &mut mcp, None).await.rule_ids, vec![human.id.clone()]);
    // 人（本机 CLI）可以确认它
    let u = update(&td, &mut cli, json!({ "id": r.id, "status": "active" })).await.unwrap();
    assert_eq!(u.status, RuleStatus::Active);
    assert_eq!(u.source.kind, RuleSourceKind::Agent, "confirming does not launder the source");
    td.shutdown().await;
}

#[tokio::test]
async fn the_source_of_a_person_s_rule_is_filled_in_by_the_daemon() {
    let td = TestDaemon::start().await;
    let mut cli = td.local_cli_client();
    let mut hub = td.hub_client("dev-pixel", "Pixel");
    let r = add_ok(&td, &mut cli, "typed in a terminal").await;
    assert_eq!(
        r.source,
        RuleSource { kind: RuleSourceKind::Manual, session_id: None, seq: None, request_id: None, device_id: None }
    );
    // 「记成规则」：answer，带会话和请求
    let r = add(
        &td,
        &mut hub,
        json!({ "id": id(), "text": "from an answer", "source": { "kind": "answer", "session_id": "sess_abc", "request_id": "req_1" } }),
    )
    .await
    .unwrap();
    assert_eq!(
        r.source,
        RuleSource {
            kind: RuleSourceKind::Answer,
            session_id: Some("sess_abc".into()),
            seq: None,
            request_id: Some("req_1".into()),
            device_id: Some("dev-pixel".into())
        }
    );
    assert_eq!(r.status, RuleStatus::Active, "a person wrote it, so it is active");
    // `manual` 带着出处也不记
    let r =
        add(&td, &mut hub, json!({ "id": id(), "text": "t", "source": { "kind": "manual", "session_id": "sess_x" } }))
            .await
            .unwrap();
    assert_eq!((r.source.kind, r.source.session_id), (RuleSourceKind::Manual, None));
    // 人也可以直接写待确认的
    let r = add(&td, &mut hub, json!({ "id": id(), "text": "t", "status": "pending" })).await.unwrap();
    assert_eq!((r.status, r.source.kind), (RuleStatus::Pending, RuleSourceKind::Manual));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 通知、列表、审计

#[tokio::test]
async fn rule_changed_goes_to_every_client_including_the_one_that_asked() {
    let td = TestDaemon::start().await;
    let mut a = td.local_cli_client();
    let mut b = td.hub_client("dev-b", "B");
    let r = add_ok(&td, &mut a, "hello").await;
    for c in [&mut a, &mut b] {
        let n = changes(c);
        assert_eq!(n.len(), 1);
        assert_eq!((n[0].id.as_str(), n[0].deleted), (r.id.as_str(), false));
        assert_eq!(n[0].rule.as_ref(), Some(&r));
    }
    let u = update(&td, &mut b, json!({ "id": r.id, "text": "changed" })).await.unwrap();
    for c in [&mut a, &mut b] {
        let n = changes(c);
        assert_eq!((n.len(), n[0].rule.as_ref()), (1, Some(&u)));
    }
    delete(&td, &mut a, json!({ "id": r.id })).await.unwrap();
    for c in [&mut a, &mut b] {
        let n = changes(c);
        assert_eq!((n.len(), n[0].id.as_str(), n[0].deleted, n[0].rule.is_none()), (1, r.id.as_str(), true, true));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn list_filters_by_status_and_project_and_always_reports_the_whole_budget() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let a = add_ok(&td, &mut c, "node wide").await;
    let b = add_in(&td, &mut c, "in web", "web").await;
    let p = add(&td, &mut c, json!({ "id": id(), "text": "waiting", "status": "pending", "project": "web" }))
        .await
        .unwrap();
    let d = add_ok(&td, &mut c, "to disable").await;
    update(&td, &mut c, json!({ "id": d.id, "status": "disabled" })).await.unwrap();

    let ids = |l: &RuleListResult| l.rules.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    let all = list(&td, &mut c, json!({})).await;
    assert_eq!(ids(&all), vec![a.id.clone(), b.id.clone(), p.id.clone(), d.id.clone()], "oldest first");
    assert_eq!(ids(&list(&td, &mut c, json!({ "status": "pending" })).await), vec![p.id.clone()]);
    assert_eq!(ids(&list(&td, &mut c, json!({ "status": "active" })).await), vec![a.id.clone(), b.id.clone()]);
    assert_eq!(ids(&list(&td, &mut c, json!({ "project": "web" })).await), vec![b.id.clone(), p.id.clone()]);
    assert_eq!(ids(&list(&td, &mut c, json!({ "project": " web ", "status": "active" })).await), vec![b.id.clone()]);
    assert!(list(&td, &mut c, json!({ "project": "nothing" })).await.rules.is_empty());
    // 预算只看生效的规则，不随过滤条件变
    let filtered = list(&td, &mut c, json!({ "status": "pending" })).await;
    assert_eq!(filtered.budget, all.budget);
    let want_node = header() + 3 + "node wide".len();
    assert_eq!(all.budget.node_bytes as usize, want_node);
    assert_eq!(all.budget.projects.get("web"), Some(&(3 + "in web".len() as u64)));
    td.shutdown().await;
}

#[tokio::test]
async fn the_audit_log_records_what_changed_without_the_text() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut hub = td.hub_client("dev-pixel", "Pixel");
    let r = add(&td, &mut hub, json!({ "id": id(), "text": "AUDITSECRET 中文", "project": "web" })).await.unwrap();
    update(&td, &mut c, json!({ "id": r.id, "text": "AUDITSECRET-2 longer", "status": "disabled" })).await.unwrap();
    update(&td, &mut c, json!({ "id": r.id, "clear": ["project"] })).await.unwrap();
    // 空操作和重复的 add 不进审计
    update(&td, &mut c, json!({ "id": r.id, "status": "disabled" })).await.unwrap();
    add(&td, &mut c, json!({ "id": r.id, "text": "again" })).await.unwrap();
    // 失败的不进审计
    let _ = update(&td, &mut c, json!({ "id": r.id, "text": "AUDITSECRET-ba\td" })).await;
    let _ = add(&td, &mut c, json!({ "id": id(), "text": "AUDITSECRET-ba\td" })).await;
    delete(&td, &mut c, json!({ "id": r.id })).await.unwrap();

    // 审计查询是最新的在前
    let mut entries = audit(&td, "rule.").await;
    entries.reverse();
    let actions: Vec<_> = entries.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, ["rule.add", "rule.update", "rule.update", "rule.delete"]);
    assert!(entries.iter().all(|e| e.target.as_deref() == Some(r.id.as_str())));
    // 中文按字符数算：「AUDITSECRET 中文」= 14
    assert_eq!(
        entries[0].detail,
        json!({ "text_len": 14, "source_kind": "manual", "status": "active", "project": "web" })
    );
    assert_eq!(entries[0].device_id, "dev-pixel");
    assert_eq!(
        entries[1].detail,
        json!({ "fields": ["text", "status"], "text_len": 20, "status": { "from": "active", "to": "disabled" } })
    );
    assert_eq!(entries[2].detail, json!({ "fields": ["project"] }));
    assert_eq!(entries[3].detail, json!({}));
    let all = serde_json::to_string(&td.ctx.audit.query(None, None, 1000).await).unwrap().to_lowercase();
    assert!(!all.contains("auditsecret") && !all.contains("中文"), "text reached the audit log: {all}");
    td.shutdown().await;
}

#[tokio::test]
async fn rules_are_never_deleted_automatically_and_retention_does_not_touch_them() {
    let td = TestDaemon::start_with(&[("sessions.history_retention", "\"1h\"")], false).await;
    let mut c = td.local_cli_client();
    for (i, s) in
        [RuleStatus::Active, RuleStatus::Pending, RuleStatus::Disabled, RuleStatus::Rejected].into_iter().enumerate()
    {
        // 创建和最后修改都在很久以前
        td.ctx.store.rule_insert(&row(&id(), "old", None, s, 1 + i as u64)).await.unwrap();
    }
    td.ctx.sessions.cleanup().await;
    assert_eq!(list(&td, &mut c, json!({})).await.rules.len(), 4);
    td.shutdown().await;
}

#[tokio::test]
async fn rules_survive_a_restart_with_everything_intact() {
    let mut td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let r = add(&td, &mut c, json!({ "id": id(), "text": "multi\nline", "project": "web" })).await.unwrap();
    let agent = add(&td, &mut c, json!({ "id": id(), "text": "a", "agent": true })).await.unwrap();
    td.restart().await;
    let mut c = td.local_cli_client();
    let l = list(&td, &mut c, json!({})).await;
    assert_eq!(l.rules, vec![r, agent]);
    td.shutdown().await;
}

#[tokio::test]
async fn the_node_announces_the_rules_and_system_prompt_capabilities() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.rules);
    let agents: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({})).await.unwrap();
    assert!(agents.agents.iter().find(|a| a.id == "fake").unwrap().capabilities.system_prompt);
    td.shutdown().await;
    // mock adapter 会把 `system_prompt_append` 回显出来，所以声明支持
    let td = TestDaemon::start_mock(&[], false).await;
    let mut c = td.local_cli_client();
    let agents: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({})).await.unwrap();
    assert!(agents.agents.iter().find(|a| a.id == "mock").unwrap().capabilities.system_prompt);
    td.shutdown().await;
}

// ---------------------------------------------------------------- 预览

#[tokio::test]
async fn rule_preview_is_the_exact_text_a_session_of_that_project_would_get() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    assert_eq!(preview(&td, &mut c, None).await, RulePreviewResult { text: String::new(), rule_ids: vec![], bytes: 0 });

    // 创建的先后顺序：项目规则 a、全节点规则 b、全节点规则 c、别的项目 d、项目规则 e
    let a = add_in(&td, &mut c, "web one", "web").await;
    let b = add_ok(&td, &mut c, "node one").await;
    let cc = add_ok(&td, &mut c, "node two\nsecond line").await;
    let d = add_in(&td, &mut c, "api one", "api").await;
    let e = add_in(&td, &mut c, "web two", "web").await;
    // 不生效的不进来
    add(&td, &mut c, json!({ "id": id(), "text": "pending one", "status": "pending" })).await.unwrap();
    let off = add_ok(&td, &mut c, "disabled one").await;
    update(&td, &mut c, json!({ "id": off.id, "status": "disabled" })).await.unwrap();

    let p = preview(&td, &mut c, Some("web")).await;
    assert_eq!(p.text, format!("{TEMPLATE_HEADER}\n- node one\n- node two\n  second line\n- web one\n- web two"));
    assert_eq!(p.text, "Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:\n- node one\n- node two\n  second line\n- web one\n- web two");
    assert_eq!(p.rule_ids, vec![b.id.clone(), cc.id.clone(), a.id.clone(), e.id.clone()]);
    assert_eq!(p.bytes as usize, p.text.len());
    let q = preview(&td, &mut c, Some("api")).await;
    assert_eq!(q.rule_ids, vec![b.id.clone(), cc.id.clone(), d.id.clone()]);
    // 没有项目 / 没有这个项目的规则：只有全节点的
    for none in [preview(&td, &mut c, None).await, preview(&td, &mut c, Some("elsewhere")).await] {
        assert_eq!(none.rule_ids, vec![b.id.clone(), cc.id.clone()]);
    }
    // 只读：不改 last_injected_ms
    assert!(list(&td, &mut c, json!({})).await.rules.iter().all(|r| r.last_injected_ms.is_none()));
    // 不合法的项目名
    let err = c.call::<Value>(&td, methods::RULE_PREVIEW, json!({ "project": "a\nb" })).await.unwrap_err();
    assert_eq!(code(&err), invalid("project", None));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 注入

fn project_dir(td: &TestDaemon, name: &str) {
    std::fs::create_dir_all(td.workspace.join(name)).unwrap();
}

async fn new_todo(td: &TestDaemon, c: &mut TestClient, title: &str, project: Option<&str>) -> Todo {
    let mut p = json!({ "id": id(), "title": title });
    if let Some(project) = project {
        p["project"] = json!(project);
    }
    c.call(td, methods::TODO_ADD, p).await.unwrap()
}

async fn dispatch_raw(
    td: &TestDaemon,
    c: &mut TestClient,
    todo: &str,
    extra: Value,
) -> Result<TodoDispatchResult, RpcError> {
    let mut p = json!({ "id": todo, "agent": "fake", "no_push": false });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    c.call(td, methods::TODO_DISPATCH, p).await
}

async fn dispatch(td: &TestDaemon, c: &mut TestClient, todo: &str, extra: Value) -> TodoDispatchResult {
    dispatch_raw(td, c, todo, extra).await.unwrap()
}

/// 等假 adapter 为这个会话拉起一次进程，返回它收到的设置。
async fn spawned_settings(td: &TestDaemon, sid: &str) -> SessionSettings {
    let t = Instant::now();
    loop {
        if let Some(r) = td.fake.spawns.lock().unwrap().iter().find(|r| r.session_id == sid) {
            return r.settings.clone();
        }
        assert!(t.elapsed() < T, "the agent for {sid} was never spawned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn session_get(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
}

async fn history(td: &TestDaemon, c: &mut TestClient, todo: &str) -> Vec<TodoEvent> {
    let r: TodoHistoryResult = c.call(td, methods::TODO_HISTORY, json!({ "id": todo, "limit": 200 })).await.unwrap();
    r.events
}

fn dispatched_rules(events: &[TodoEvent], sid: &str) -> (Vec<String>, Option<RulesSkipped>) {
    events
        .iter()
        .find_map(|e| match &e.data {
            TodoEventData::Dispatched { session_id, rules, rules_skipped, .. } if session_id == sid => {
                Some((rules.clone(), *rules_skipped))
            }
            _ => None,
        })
        .expect("a dispatched event for the session")
}

async fn session_count(td: &TestDaemon, c: &mut TestClient) -> usize {
    let r: SessionListResult = c.call(td, methods::SESSION_LIST, json!({ "include_archived": true })).await.unwrap();
    r.sessions.len()
}

fn text_of(rules: &[&str]) -> String {
    let mut s = String::from(TEMPLATE_HEADER);
    for r in rules {
        s.push_str("\n- ");
        s.push_str(r);
    }
    s
}

#[tokio::test]
async fn a_dispatched_session_gets_the_rules_in_its_system_prompt_append() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    project_dir(&td, "api");
    let w = add_in(&td, &mut c, "web only", "web").await;
    let n1 = add_ok(&td, &mut c, "node first").await;
    let a = add_in(&td, &mut c, "api only", "api").await;
    let n2 = add_ok(&td, &mut c, "node second").await;
    let pending = add(&td, &mut c, json!({ "id": id(), "text": "pending", "status": "pending" })).await.unwrap();

    let todo = new_todo(&td, &mut c, "do the thing", Some("web")).await;
    let r = dispatch(&td, &mut c, &todo.id, json!({})).await;
    // 全节点规则在前、项目规则在后，各按创建的先后；别的项目的、待确认的不在
    let want = text_of(&["node first", "node second", "web only"]);
    let got = spawned_settings(&td, &r.session.id).await;
    assert_eq!(got.system_prompt_append.as_deref(), Some(want.as_str()));
    // 就在会话的设置里，用户看得到 agent 拿到了什么
    assert_eq!(
        session_get(&td, &mut c, &r.session.id).await.settings.system_prompt_append.as_deref(),
        Some(want.as_str())
    );
    assert_eq!(r.session.settings.system_prompt_append.as_deref(), Some(want.as_str()));
    // 同一个函数：`rule.preview`
    assert_eq!(preview(&td, &mut c, Some("web")).await.text, want);

    // 「经过」里记着用了哪几条，按文字里的顺序
    let (ids, skipped) = dispatched_rules(&history(&td, &mut c, &todo.id).await, &r.session.id);
    assert_eq!((ids, skipped), (vec![n1.id.clone(), n2.id.clone(), w.id.clone()], None));
    // 被注入的规则记下了时间，没被注入的没有；`updated_ms` 不动
    let l = list(&td, &mut c, json!({})).await;
    for rule in &l.rules {
        let used = [&n1.id, &n2.id, &w.id].contains(&&rule.id);
        assert_eq!(rule.last_injected_ms.is_some(), used, "{}", rule.id);
    }
    assert_eq!(l.rules.iter().find(|x| x.id == w.id).unwrap().updated_ms, w.updated_ms);
    assert_eq!(l.rules.iter().find(|x| x.id == a.id).unwrap().last_injected_ms, None);
    assert_eq!(l.rules.iter().find(|x| x.id == pending.id).unwrap().last_injected_ms, None);

    // 另一个项目拿到它自己的
    let todo2 = new_todo(&td, &mut c, "api work", Some("api")).await;
    let r2 = dispatch(&td, &mut c, &todo2.id, json!({})).await;
    let got = spawned_settings(&td, &r2.session.id).await;
    assert_eq!(got.system_prompt_append, Some(text_of(&["node first", "node second", "api only"])));
    td.shutdown().await;
}

#[tokio::test]
async fn the_rules_come_before_the_callers_own_append_after_a_blank_line() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    add_ok(&td, &mut c, "be careful").await;
    let todo = new_todo(&td, &mut c, "with an append", Some("web")).await;
    let r = dispatch(&td, &mut c, &todo.id, json!({ "settings": { "system_prompt_append": "Be terse." } })).await;
    let got = spawned_settings(&td, &r.session.id).await;
    assert_eq!(got.system_prompt_append, Some(format!("{}\n\nBe terse.", text_of(&["be careful"]))));

    // 没有规则时，调用方的原样交给 agent
    let mut cli = td.local_cli_client();
    let rid = list(&td, &mut cli, json!({})).await.rules[0].id.clone();
    update(&td, &mut cli, json!({ "id": rid, "status": "disabled" })).await.unwrap();
    let todo = new_todo(&td, &mut c, "no rules now", Some("web")).await;
    let r = dispatch(&td, &mut c, &todo.id, json!({ "settings": { "system_prompt_append": "Be terse." } })).await;
    assert_eq!(spawned_settings(&td, &r.session.id).await.system_prompt_append.as_deref(), Some("Be terse."));
    let nothing = new_todo(&td, &mut c, "nothing at all", Some("web")).await;
    let r = dispatch(&td, &mut c, &nothing.id, json!({})).await;
    assert_eq!(spawned_settings(&td, &r.session.id).await.system_prompt_append, None);
    td.shutdown().await;
}

#[tokio::test]
async fn the_combined_text_is_still_limited_to_16_kib() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let rule = add_ok(&td, &mut c, "stay within bounds").await;
    let rules_len = text_of(&["stay within bounds"]).len();
    let room = 16 * 1024 - rules_len - 2;

    // 刚好 16 KiB：可以
    let todo = new_todo(&td, &mut c, "fits exactly", Some("web")).await;
    let ok = dispatch(&td, &mut c, &todo.id, json!({ "settings": { "system_prompt_append": "a".repeat(room) } })).await;
    assert_eq!(spawned_settings(&td, &ok.session.id).await.system_prompt_append.unwrap().len(), 16 * 1024);

    // 多一个字节：`invalid_settings`，什么都不留下
    let before_injected = rule_of(&td, &mut c, &rule.id).await.last_injected_ms;
    let sessions_before = session_count(&td, &mut c).await;
    let todo = new_todo(&td, &mut c, "one byte too many", Some("web")).await;
    let e =
        dispatch_raw(&td, &mut c, &todo.id, json!({ "settings": { "system_prompt_append": "a".repeat(room + 1) } }))
            .await
            .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
    assert_eq!(e.data.as_ref().unwrap().field.as_deref(), Some("system_prompt_append"));
    assert_eq!(session_count(&td, &mut c).await, sessions_before);
    assert!(history(&td, &mut c, &todo.id).await.is_empty());
    let after: Todo = c.call(&td, methods::TODO_GET, json!({ "id": todo.id })).await.unwrap();
    assert!(after.sessions.is_empty());
    assert_eq!(rule_of(&td, &mut c, &rule.id).await.last_injected_ms, before_injected);

    // 调用方自己的就已经超了：没有规则也是同一个错
    let e = dispatch_raw(
        &td,
        &mut c,
        &todo.id,
        json!({ "settings": { "system_prompt_append": "a".repeat(16 * 1024 + 1) } }),
    )
    .await
    .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
    td.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_cannot_take_a_system_prompt_gets_no_rules_and_the_dispatch_still_works() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let rule = add_ok(&td, &mut c, "invisible to this agent").await;
    td.fake.system_prompt.store(false, Ordering::SeqCst);
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();

    let todo = new_todo(&td, &mut c, "for an agent without prompts", Some("web")).await;
    let r = dispatch(&td, &mut c, &todo.id, json!({ "settings": { "system_prompt_append": "mine" } })).await;
    // 规则没有注入，调用方自己的照旧
    assert_eq!(spawned_settings(&td, &r.session.id).await.system_prompt_append.as_deref(), Some("mine"));
    let (ids, skipped) = dispatched_rules(&history(&td, &mut c, &todo.id).await, &r.session.id);
    assert_eq!((ids, skipped), (vec![], Some(RulesSkipped::Unsupported)));
    assert_eq!(rule_of(&td, &mut c, &rule.id).await.last_injected_ms, None, "nothing was injected");

    // 没有生效的规则时没有什么「被跳过」的
    update(&td, &mut c, json!({ "id": rule.id, "status": "disabled" })).await.unwrap();
    let todo = new_todo(&td, &mut c, "no rules, no agent support", Some("web")).await;
    let r = dispatch(&td, &mut c, &todo.id, json!({})).await;
    assert_eq!(dispatched_rules(&history(&td, &mut c, &todo.id).await, &r.session.id), (vec![], None));
    td.shutdown().await;
}

#[tokio::test]
async fn manual_sessions_get_rules_only_when_the_node_is_set_to_inject_them() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let rule = add_ok(&td, &mut c, "for manual sessions too").await;
    let create = |mid: &str, project: Option<&str>, append: Option<&str>| {
        let mut p = json!({ "agent": "fake", "prompt": "hello", "message_id": mid });
        if let Some(project) = project {
            p["project"] = json!(project);
        }
        if let Some(append) = append {
            p["settings"] = json!({ "system_prompt_append": append });
        }
        p
    };

    // 缺省关：手动开的会话不带
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, create("m1", Some("web"), None)).await.unwrap();
    assert_eq!(spawned_settings(&td, &s.id).await.system_prompt_append, None);
    assert_eq!(rule_of(&td, &mut c, &rule.id).await.last_injected_ms, None);

    // 热加载打开
    anycon_daemon::config::set_value(&td.paths, "rules.inject_manual_sessions", "true").unwrap();
    td.ctx.config.reload().unwrap();
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, create("m2", Some("web"), None)).await.unwrap();
    assert_eq!(spawned_settings(&td, &s.id).await.system_prompt_append, Some(text_of(&["for manual sessions too"])));
    assert!(rule_of(&td, &mut c, &rule.id).await.last_injected_ms.is_some());
    // 没有项目的会话：只有全节点规则；调用方自己的接在后面
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, create("m3", None, Some("mine"))).await.unwrap();
    assert_eq!(
        spawned_settings(&td, &s.id).await.system_prompt_append,
        Some(format!("{}\n\nmine", text_of(&["for manual sessions too"])))
    );
    // 按 message_id 重试：返回同一个会话，不再注入一次
    let injected = rule_of(&td, &mut c, &rule.id).await.last_injected_ms;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let again: SessionSummary = c.call(&td, methods::SESSION_CREATE, create("m3", None, Some("mine"))).await.unwrap();
    assert_eq!(again.id, s.id);
    assert_eq!(rule_of(&td, &mut c, &rule.id).await.last_injected_ms, injected);

    // 合起来超过 16 KiB：`invalid_settings`
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            create("m4", Some("web"), Some(&"a".repeat(16 * 1024 - 10))),
        )
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));

    // 再关上
    anycon_daemon::config::set_value(&td.paths, "rules.inject_manual_sessions", "false").unwrap();
    td.ctx.config.reload().unwrap();
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, create("m5", Some("web"), None)).await.unwrap();
    assert_eq!(spawned_settings(&td, &s.id).await.system_prompt_append, None);
    td.shutdown().await;
}

#[tokio::test]
async fn manual_sessions_on_an_agent_without_system_prompts_are_left_alone() {
    let td = TestDaemon::start_with(&[("rules.inject_manual_sessions", "true")], false).await;
    let mut c = td.local_cli_client();
    add_ok(&td, &mut c, "not for this agent").await;
    td.fake.system_prompt.store(false, Ordering::SeqCst);
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hello", "message_id": "m1" }))
        .await
        .unwrap();
    assert_eq!(spawned_settings(&td, &s.id).await.system_prompt_append, None);
    td.shutdown().await;
}

#[tokio::test]
async fn a_running_session_keeps_the_rules_it_was_created_with() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let r1 = add_ok(&td, &mut c, "original wording").await;
    let r2 = add_ok(&td, &mut c, "second rule").await;
    let todo = new_todo(&td, &mut c, "first run", Some("web")).await;
    let first = dispatch(&td, &mut c, &todo.id, json!({})).await;
    let snapshot = text_of(&["original wording", "second rule"]);
    assert_eq!(spawned_settings(&td, &first.session.id).await.system_prompt_append.as_deref(), Some(snapshot.as_str()));

    // 之后改文字、停用、删除：已经在跑的会话不变
    update(&td, &mut c, json!({ "id": r1.id, "text": "new wording" })).await.unwrap();
    update(&td, &mut c, json!({ "id": r2.id, "status": "disabled" })).await.unwrap();
    let s = session_get(&td, &mut c, &first.session.id).await;
    assert_eq!(s.settings.system_prompt_append.as_deref(), Some(snapshot.as_str()));
    // 进程拉起时也是那份快照：重启会话
    let _: Empty = c.call(&td, methods::SESSION_RESTART, json!({ "session_id": first.session.id })).await.unwrap();
    // 懒重启：下一条消息才拉起进程
    let _: SessionSendResult = c
        .call(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": first.session.id, "message_id": id(), "content": "continue" }),
        )
        .await
        .unwrap();
    let t = Instant::now();
    loop {
        let n = td.fake.spawns.lock().unwrap().iter().filter(|r| r.session_id == first.session.id).count();
        if n >= 2 {
            break;
        }
        assert!(t.elapsed() < T, "the session was not respawned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let last = td
        .fake
        .spawns
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.session_id == first.session.id)
        .unwrap()
        .settings
        .clone();
    assert_eq!(last.system_prompt_append.as_deref(), Some(snapshot.as_str()));

    // 下一次派发用新的
    let todo2 = new_todo(&td, &mut c, "second run", Some("web")).await;
    let second = dispatch(&td, &mut c, &todo2.id, json!({})).await;
    assert_eq!(spawned_settings(&td, &second.session.id).await.system_prompt_append, Some(text_of(&["new wording"])));
    delete(&td, &mut c, json!({ "id": r1.id })).await.unwrap();
    assert_eq!(
        session_get(&td, &mut c, &first.session.id).await.settings.system_prompt_append.as_deref(),
        Some(snapshot.as_str())
    );
    td.shutdown().await;
}

#[tokio::test]
async fn last_injected_ms_changes_neither_updated_ms_nor_notifies() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut watcher = td.hub_client("dev-w", "W");
    project_dir(&td, "web");
    let r = add_ok(&td, &mut c, "watch me").await;
    let _ = changes(&mut watcher);
    let todo = new_todo(&td, &mut c, "inject", Some("web")).await;
    dispatch(&td, &mut c, &todo.id, json!({})).await;
    let now = rule_of(&td, &mut c, &r.id).await;
    assert!(now.last_injected_ms.is_some());
    assert_eq!((now.updated_ms, now.created_ms), (r.updated_ms, r.created_ms));
    assert!(changes(&mut watcher).is_empty(), "an injection is not a change of the rule");
    // 之后的修改带着 last_injected_ms 一起走，没有被冲掉
    let u = update(&td, &mut c, json!({ "id": r.id, "text": "edited" })).await.unwrap();
    assert_eq!(u.last_injected_ms, now.last_injected_ms);
    // 一次失败的派发（没有项目）不算注入
    let t2 = new_todo(&td, &mut c, "no project", None).await;
    let before = rule_of(&td, &mut c, &r.id).await.last_injected_ms;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(dispatch_raw(&td, &mut c, &t2.id, json!({})).await.is_err());
    assert_eq!(rule_of(&td, &mut c, &r.id).await.last_injected_ms, before);
    td.shutdown().await;
}

#[tokio::test]
async fn a_dispatch_retried_with_the_same_message_id_injects_and_records_once() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let r = add_ok(&td, &mut c, "once only").await;
    let todo = new_todo(&td, &mut c, "retry me", Some("web")).await;
    let mid = id();
    let first = dispatch(&td, &mut c, &todo.id, json!({ "message_id": mid })).await;
    let injected = rule_of(&td, &mut c, &r.id).await.last_injected_ms;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let again = dispatch(&td, &mut c, &todo.id, json!({ "message_id": mid })).await;
    assert_eq!(again.session.id, first.session.id);
    assert_eq!(rule_of(&td, &mut c, &r.id).await.last_injected_ms, injected);
    let dispatched = history(&td, &mut c, &todo.id)
        .await
        .into_iter()
        .filter(|e| matches!(e.data, TodoEventData::Dispatched { .. }))
        .count();
    assert_eq!(dispatched, 1);
    td.shutdown().await;
}

#[tokio::test]
async fn the_dispatch_preview_shows_the_rules_text_the_dispatch_will_inject() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    let with_project = new_todo(&td, &mut c, "has a project", Some("web")).await;
    let without = new_todo(&td, &mut c, "has none", None).await;
    async fn pv(td: &TestDaemon, todo: &str) -> TodoDispatchPreviewResult {
        let mut c = td.local_cli_client();
        c.call(td, methods::TODO_DISPATCH_PREVIEW, json!({ "id": todo, "no_push": false })).await.unwrap()
    }
    // 没有规则：空串，第一条消息照旧
    let r = pv(&td, &with_project.id).await;
    assert_eq!(r.rules_text, "");
    assert!(r.first_message.starts_with("has a project"));

    add_ok(&td, &mut c, "node rule").await;
    add_in(&td, &mut c, "web rule", "web").await;
    assert_eq!(pv(&td, &with_project.id).await.rules_text, text_of(&["node rule", "web rule"]));
    // 没有项目的项：只有全节点规则
    assert_eq!(pv(&td, &without.id).await.rules_text, text_of(&["node rule"]));
    // 和 `rule.preview` 是同一段文字，也是派发真的注入的那一段
    assert_eq!(pv(&td, &with_project.id).await.rules_text, preview(&td, &mut c, Some("web")).await.text);
    let r = dispatch(&td, &mut c, &with_project.id, json!({})).await;
    assert_eq!(
        spawned_settings(&td, &r.session.id).await.system_prompt_append,
        Some(pv(&td, &with_project.id).await.rules_text)
    );
    // `project` 给了就用它的规则：派发会选另一个项目时，预览和真正的注入一致
    project_dir(&td, "api");
    add_in(&td, &mut c, "api rule", "api").await;
    let mut c2 = td.local_cli_client();
    let other: TodoDispatchPreviewResult = c2
        .call(
            &td,
            methods::TODO_DISPATCH_PREVIEW,
            json!({ "id": with_project.id, "project": " api ", "no_push": false }),
        )
        .await
        .unwrap();
    assert_eq!(other.rules_text, text_of(&["node rule", "api rule"]));
    let r = dispatch(&td, &mut c, &with_project.id, json!({ "project": "api" })).await;
    assert_eq!(spawned_settings(&td, &r.session.id).await.system_prompt_append, Some(other.rules_text));
    let e = c2
        .call::<Value>(&td, methods::TODO_DISPATCH_PREVIEW, json!({ "id": without.id, "project": "a\nb" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), invalid("project", None));
    // 预览不是注入：它不改 last_injected_ms（上面的派发之前没有任何规则被注入过）
    td.shutdown().await;
}

#[tokio::test]
async fn previews_and_rule_calls_never_touch_last_injected_ms() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    project_dir(&td, "web");
    add_in(&td, &mut c, "web rule", "web").await;
    let todo = new_todo(&td, &mut c, "preview only", Some("web")).await;
    let _: TodoDispatchPreviewResult =
        c.call(&td, methods::TODO_DISPATCH_PREVIEW, json!({ "id": todo.id })).await.unwrap();
    preview(&td, &mut c, Some("web")).await;
    assert!(list(&td, &mut c, json!({})).await.rules.iter().all(|r| r.last_injected_ms.is_none()));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 真的 mock adapter

#[tokio::test]
async fn the_mock_agent_echoes_the_injected_rules() {
    let td = TestDaemon::start_mock(&[("rules.inject_manual_sessions", "true")], false).await;
    let mut c = td.local_cli_client();
    add_ok(&td, &mut c, "echo me").await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "mock", "prompt": "hello", "message_id": "m1" }))
        .await
        .unwrap();
    assert_eq!(s.settings.system_prompt_append, Some(text_of(&["echo me"])));
    let t = Instant::now();
    loop {
        let st = session_get(&td, &mut c, &s.id).await;
        if st.usage.turns >= 1 && st.status == SessionStatus::Idle {
            break;
        }
        assert!(t.elapsed() < T, "stuck in {:?}", st.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let h: SessionHistoryResult =
        c.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "limit": 1000 })).await.unwrap();
    let text: String = h
        .events
        .iter()
        .filter_map(|e| if let Event::TextDelta { text } = &e.event { Some(text.as_str()) } else { None })
        .collect();
    // mock 回显 `system_prompt_append` 的前 32 个字符
    assert!(text.contains("prompt_append=Standing rules from the user. Th You said: hello"), "{text}");
    td.shutdown().await;
}
