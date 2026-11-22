//! 决策档案的规则（`rule.*`，docs/spec/daemon.md § 23）：每个节点一份，存在 daemon 的 SQLite 里；生效的规则在建会话时
//! 写进会话的 `system_prompt_append`（§ 23.5）。规则只影响 agent 怎么判断，不影响任何权限。
//!
//! 所有写操作（新建、修改、删除）在 [`Store::rule_lock`](crate::store::Store::rule_lock) 里串行：「id 已存在就原样返回」「总数 ≤ 300」
//! 「生效时的字数检查」都是读了再写。新建 / 修改 / 删除之后向所有在线客户端发 `rule.changed`，并写审计（§ 23.7）。
//! 注入时的读（[`injection_for`]）不取这把锁：一条 `SELECT` 看到的就是一份一致的快照；`last_injected_ms` 只由
//! [`Store::rules_mark_injected`] 写，`rule_save` 不碰那一列，所以两边互不覆盖。
//!
//! 规则的 `text` 是用户内容：不进日志，不进审计（审计只记长度和字段名）。

use std::collections::HashSet;

use anycon_protocol::{
    methods, ClearableRuleField, ErrorCode, RpcError, Rule, RuleAddParams, RuleAddSourceKind, RuleAddStatus,
    RuleBudget, RuleChanged, RuleDeleteParams, RuleListParams, RuleListResult, RulePreviewParams, RulePreviewResult,
    RuleSource, RuleSourceKind, RuleStatus, RuleSync, RuleUpdateParams,
};
use serde_json::json;
use sqlx::Row;

use crate::clients::Caller;
use crate::daemon::Ctx;
use crate::store::{Store, StoreError};
use crate::util::now_ms;
use crate::workspace::Workspace;

/// 规则文字的码点数上限。
pub const MAX_TEXT_CHARS: usize = 600;
/// 规则总数上限（不含 `rejected` 的）：`rule.list` 一次给全，不翻页。
pub const MAX_RULES: i64 = 300;
/// `rejected` 的规则另有一个上限（留着是为了之后提炼时不再提同样的）：满了再否决一条，就删掉最早否决的那一条。
/// 这是 daemon 唯一会自动删除的规则；它只删 `rejected` 的，从不动待确认、生效、停用的。
pub const MAX_REJECTED: i64 = 300;
/// agent 提的（`agent`、`extracted` 来源）、还在待确认的规则最多这么多：agent 占不满 300 个名额，把人挡在外面。
pub const MAX_AGENT_PENDING: i64 = 50;
/// 一个会话拿到的规则文字（含模板）的字节上限（§ 23.4）。
pub const BUDGET_CAP: usize = 4 * 1024;
/// 来源里 `session_id` / `request_id` 的字节上限。
const MAX_SOURCE_ID_BYTES: usize = 128;

/// 注入文字的开头（§ 23.5，给 agent 看，英文）。
pub const TEMPLATE_HEADER: &str =
    "Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:";

type RpcResult<T> = Result<T, RpcError>;

fn internal(e: StoreError) -> RpcError {
    // 只带数据库错误本身，不带任何规则内容
    tracing::warn!(error = %e, "rule storage error");
    RpcError::new(ErrorCode::Internal, "rule storage error")
}

fn invalid(field: &str, reason: Option<&str>, message: String) -> RpcError {
    let mut e = RpcError::new(ErrorCode::Invalid, message);
    if let Some(d) = e.data.as_mut() {
        d.field = Some(field.to_string());
        d.reason = reason.map(str::to_string);
    }
    e
}

fn not_found(id: &str) -> RpcError {
    RpcError::new(ErrorCode::NotFound, format!("rule {id} not found"))
}

fn forbidden() -> RpcError {
    RpcError::new(ErrorCode::Forbidden, "an agent connection cannot change or delete a rule")
}

/// 超出注入的字数上限（`data.bytes` 是这次改动之后一个会话拿到的规则文字的字节数，`data.cap` 是上限）。
fn over_budget(field: &str, bytes: usize) -> RpcError {
    let mut e = invalid(
        field,
        Some("over_budget"),
        format!("the active rules would take {bytes} bytes of the {BUDGET_CAP} allowed in a session; merge or disable some first"),
    );
    if let Some(d) = e.data.as_mut() {
        d.bytes = Some(bytes as u64);
        d.cap = Some(BUDGET_CAP as u64);
    }
    e
}

// ====================================================================== 校验

/// 规则文字：去掉首尾空白之后 1–600 个 Unicode 码点；可以多行，但除换行外不含控制字符，也不含 U+2028 / U+2029。返回去掉空白后的文字。
pub(crate) fn check_text(text: &str) -> RpcResult<String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(invalid("text", None, "text must not be empty".into()));
    }
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(invalid("text", None, format!("text must be at most {MAX_TEXT_CHARS} characters")));
    }
    if t.chars().any(|c| c != '\n' && crate::config::is_line_break_or_control(c)) {
        return Err(invalid("text", None, "text must not contain control characters other than line breaks".into()));
    }
    // 规则原样注入、要由人看过才生效：看不见的字符（双向控制符、零宽字符）没法在界面上审阅，一律拒绝
    if t.chars().any(is_invisible_format) {
        return Err(invalid(
            "text",
            Some("invisible"),
            "text must not contain bidirectional controls or zero-width characters".into(),
        ));
    }
    Ok(t.to_string())
}

/// 双向文字控制符（U+202A–U+202E、U+2066–U+2069、U+200E / U+200F、U+061C）和零宽字符（U+200B–U+200D、U+2060、U+FEFF）。
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{061C}'
            | '\u{200B}'..='\u{200D}'
            | '\u{2060}'
            | '\u{FEFF}'
    )
}

/// 来源里的 `session_id` / `request_id`：非空、≤ 128 字节、不含控制字符。
fn check_source_id(field: &str, v: &str) -> RpcResult<String> {
    if v.is_empty() || v.len() > MAX_SOURCE_ID_BYTES || v.chars().any(char::is_control) {
        return Err(invalid(
            field,
            None,
            format!("{field} must be 1-{MAX_SOURCE_ID_BYTES} bytes without control characters"),
        ));
    }
    Ok(v.to_string())
}

// ====================================================================== 模板和字数（纯函数）

/// 一条规则在注入文字里的样子：`- ` 开头，多行的后续行缩进两格（含空行）。
fn item_line(text: &str) -> String {
    let mut s = String::with_capacity(text.len() + 4);
    s.push_str("- ");
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            s.push_str("\n  ");
        }
        s.push_str(line);
    }
    s
}

/// 一条规则占注入文字的字节数：前面的换行加上它自己的行。
fn item_bytes(text: &str) -> usize {
    1 + item_line(text).len()
}

/// 注入文字（§ 23.5）：模板的开头，然后每条规则一行，按给定的顺序。没有规则时是空串。末尾没有换行。
pub fn render_rules_text(texts: &[&str]) -> String {
    if texts.is_empty() {
        return String::new();
    }
    let mut s = String::from(TEMPLATE_HEADER);
    for t in texts {
        s.push('\n');
        s.push_str(&item_line(t));
    }
    s
}

/// 一个会话拿到的规则：全节点规则在前，这个项目的在后，各按 `created_ms`（同一毫秒的按写入先后）。`active` 必须已经是按这个顺序排好的生效规则。
pub fn select_for_session<'a>(
    active: &[&'a Rule],
    project: Option<&str>,
    same: &dyn Fn(&str, &str) -> bool,
) -> Vec<&'a Rule> {
    let mut out: Vec<&Rule> = active.iter().copied().filter(|r| r.project.is_none()).collect();
    if let Some(p) = project {
        out.extend(active.iter().copied().filter(|r| r.project.as_deref().is_some_and(|rp| same(rp, p))));
    }
    out
}

/// 字数的计算结果（§ 23.4）：模板的开头和全节点规则的字节数，加上每个项目规则的字节数。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Budget {
    /// 有任何一条生效的规则时 = 模板开头 + 全节点规则；一条都没有时是 0（那时不注入任何东西）。
    pub node_bytes: usize,
    /// 项目名（同一个项目的不同写法归在第一次出现的那个名字下）和它的生效规则的字节数，按第一次出现的顺序。
    pub projects: Vec<(String, usize)>,
}

impl Budget {
    fn project_bytes(&self, project: &str, same: &dyn Fn(&str, &str) -> bool) -> usize {
        self.projects.iter().find(|(k, _)| same(k, project)).map(|(_, b)| *b).unwrap_or(0)
    }

    /// 一个这个项目的会话拿到的文字的字节数。
    pub fn session_bytes(&self, project: Option<&str>, same: &dyn Fn(&str, &str) -> bool) -> usize {
        self.node_bytes + project.map(|p| self.project_bytes(p, same)).unwrap_or(0)
    }

    pub fn to_wire(&self) -> RuleBudget {
        RuleBudget {
            cap: BUDGET_CAP as u64,
            node_bytes: self.node_bytes as u64,
            projects: self.projects.iter().map(|(k, b)| (k.clone(), *b as u64)).collect(),
        }
    }
}

/// 按生效的规则（任意顺序都行，只是项目名的归并按出现的顺序）算字数。
pub fn budget_of(active: &[&Rule], same: &dyn Fn(&str, &str) -> bool) -> Budget {
    if active.is_empty() {
        return Budget::default();
    }
    let mut b = Budget { node_bytes: TEMPLATE_HEADER.len(), projects: Vec::new() };
    for r in active {
        let bytes = item_bytes(&r.text);
        match r.project.as_deref() {
            None => b.node_bytes += bytes,
            Some(p) => match b.projects.iter_mut().find(|(k, _)| same(k, p)) {
                Some((_, total)) => *total += bytes,
                None => b.projects.push((p.to_string(), bytes)),
            },
        }
    }
    b
}

/// 一条规则变成生效（或者生效的规则改了文字 / 项目）之后的检查（§ 23.4）。`after` 是改动**之后**的生效规则的字数，
/// `project` 是这条规则的项目。超了返回超出的那个字节数（全节点规则：全节点的总量加最大的项目的总量；项目规则：全节点的总量加这个项目的总量）。
pub fn check_budget(after: &Budget, project: Option<&str>, same: &dyn Fn(&str, &str) -> bool) -> Result<(), usize> {
    let bytes = match project {
        None => after.node_bytes + after.projects.iter().map(|(_, b)| *b).max().unwrap_or(0),
        Some(p) => after.node_bytes + after.project_bytes(p, same),
    };
    if bytes > BUDGET_CAP {
        Err(bytes)
    } else {
        Ok(())
    }
}

/// 把规则文字放在调用方自己给的 `system_prompt_append` 前面，中间空一行（§ 23.5）。
pub fn prepend_to_append(rules_text: &str, caller_append: Option<&str>) -> String {
    match caller_append.filter(|a| !a.is_empty()) {
        Some(a) => format!("{rules_text}\n\n{a}"),
        None => rules_text.to_string(),
    }
}

// ====================================================================== 存储

fn row_to_rule(r: &sqlx::sqlite::SqliteRow) -> Rule {
    // 库里不认识的字符串（降级到旧版本之后读到更新版本写的值）按 `Unknown` 读：不生效，也不会被写回成别的值
    let status = match r.get::<String, _>("status").as_str() {
        "pending" => RuleStatus::Pending,
        "active" => RuleStatus::Active,
        "disabled" => RuleStatus::Disabled,
        "rejected" => RuleStatus::Rejected,
        _ => RuleStatus::Unknown,
    };
    let sync = match r.get::<String, _>("sync").as_str() {
        "node" => RuleSync::Node,
        "group" => RuleSync::Group,
        _ => RuleSync::Unknown,
    };
    let kind = match r.get::<String, _>("source_kind").as_str() {
        "manual" => RuleSourceKind::Manual,
        "answer" => RuleSourceKind::Answer,
        "extracted" => RuleSourceKind::Extracted,
        "agent" => RuleSourceKind::Agent,
        _ => RuleSourceKind::Unknown,
    };
    Rule {
        id: r.get("id"),
        text: r.get("text"),
        project: r.get("project"),
        status,
        sync,
        source: RuleSource {
            kind,
            session_id: r.get("source_session"),
            seq: r.get::<Option<i64>, _>("source_seq").map(|v| v as u64),
            request_id: r.get("source_request"),
            device_id: r.get("source_device"),
        },
        created_ms: r.get::<i64, _>("created_ms") as u64,
        updated_ms: r.get::<i64, _>("updated_ms") as u64,
        last_injected_ms: r.get::<Option<i64>, _>("last_injected_ms").map(|v| v as u64),
    }
}

fn check_writable(r: &Rule) -> Result<(), StoreError> {
    // `Unknown` 只存在于客户端的解析里：daemon 不能把它写进库（读回去会变成别的东西）
    if r.status == RuleStatus::Unknown || r.source.kind == RuleSourceKind::Unknown || r.sync == RuleSync::Unknown {
        return Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a rule cannot have an unknown status, source or sync",
        )));
    }
    Ok(())
}

impl Store {
    /// 全部规则（≤ 300 条），按 `created_ms` 从早到晚（同一毫秒的按写入先后）。
    pub async fn rules_all(&self) -> Result<Vec<Rule>, StoreError> {
        let rows = sqlx::query("SELECT * FROM rules ORDER BY created_ms, rowid").fetch_all(self.pool()).await?;
        Ok(rows.iter().map(row_to_rule).collect())
    }

    pub async fn rule_get(&self, id: &str) -> Result<Option<Rule>, StoreError> {
        let r = sqlx::query("SELECT * FROM rules WHERE id = ?").bind(id).fetch_optional(self.pool()).await?;
        Ok(r.as_ref().map(row_to_rule))
    }

    /// 占 [`MAX_RULES`] 名额的规则数（不含 `rejected`）。
    pub async fn rule_count(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query("SELECT COUNT(*) FROM rules WHERE status != 'rejected'").fetch_one(self.pool()).await?.get(0))
    }

    pub async fn rule_count_rejected(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query("SELECT COUNT(*) FROM rules WHERE status = 'rejected'").fetch_one(self.pool()).await?.get(0))
    }

    /// 待确认的、不是人写的（`agent` / `extracted`）规则数，[`MAX_AGENT_PENDING`] 的依据。
    pub async fn rule_count_agent_pending(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query("SELECT COUNT(*) FROM rules WHERE status = 'pending' AND source_kind IN ('agent', 'extracted')")
            .fetch_one(self.pool())
            .await?
            .get(0))
    }

    /// 最早否决的那条 `rejected` 规则（`updated_ms` 最小；`except` 是刚刚否决的那一条，不算）。
    pub async fn rule_oldest_rejected(&self, except: &str) -> Result<Option<String>, StoreError> {
        let r =
            sqlx::query("SELECT id FROM rules WHERE status = 'rejected' AND id != ? ORDER BY updated_ms, id LIMIT 1")
                .bind(except)
                .fetch_optional(self.pool())
                .await?;
        Ok(r.map(|r| r.get(0)))
    }

    pub async fn rule_insert(&self, r: &Rule) -> Result<(), StoreError> {
        check_writable(r)?;
        sqlx::query(
            "INSERT INTO rules (id, text, project, status, sync, source_kind, source_session, source_seq, \
             source_request, source_device, created_ms, updated_ms, last_injected_ms) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&r.id)
        .bind(&r.text)
        .bind(&r.project)
        .bind(r.status.as_str())
        .bind(match r.sync {
            RuleSync::Group => "group",
            _ => "node",
        })
        .bind(r.source.kind.as_str())
        .bind(&r.source.session_id)
        .bind(r.source.seq.map(|v| v as i64))
        .bind(&r.source.request_id)
        .bind(&r.source.device_id)
        .bind(r.created_ms as i64)
        .bind(r.updated_ms as i64)
        .bind(r.last_injected_ms.map(|v| v as i64))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// 写回可以变的字段（来源和创建时间不变；`last_injected_ms` 只由 [`Store::rules_mark_injected`] 写）。
    pub async fn rule_save(&self, r: &Rule) -> Result<(), StoreError> {
        check_writable(r)?;
        sqlx::query("UPDATE rules SET text = ?, project = ?, status = ?, updated_ms = ? WHERE id = ?")
            .bind(&r.text)
            .bind(&r.project)
            .bind(r.status.as_str())
            .bind(r.updated_ms as i64)
            .bind(&r.id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// 物理删除。返回有没有这一条。
    pub async fn rule_delete(&self, id: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM rules WHERE id = ?").bind(id).execute(self.pool()).await?.rows_affected() > 0)
    }

    /// 这些规则刚被注入了一个会话：记下时间。不动 `updated_ms`（它是版本号），也不通知。
    pub async fn rules_mark_injected(&self, ids: &[String], at_ms: u64) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await?;
        for id in ids {
            sqlx::query("UPDATE rules SET last_injected_ms = ? WHERE id = ?")
                .bind(at_ms as i64)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn active_of(all: &[Rule]) -> Vec<&Rule> {
    all.iter().filter(|r| r.status == RuleStatus::Active).collect()
}

// ====================================================================== 注入

/// 建会话时要注入的规则（§ 23.5）：文字和写进文字里的规则 id（按它们在文字里的顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub text: String,
    pub rule_ids: Vec<String>,
}

/// 现在开一个这个项目的会话会注入的规则；没有生效的规则时是 `None`。`rule.preview`、`todo.dispatch_preview` 的 `rules_text`
/// 和真正的注入都走这个函数。
pub async fn injection_for(
    store: &Store,
    workspace: &Workspace,
    project: Option<&str>,
) -> Result<Option<Injection>, StoreError> {
    let all = store.rules_all().await?;
    let active = active_of(&all);
    let same = |a: &str, b: &str| workspace.same_project_name(a, b);
    let chosen = select_for_session(&active, project, &same);
    if chosen.is_empty() {
        return Ok(None);
    }
    let texts: Vec<&str> = chosen.iter().map(|r| r.text.as_str()).collect();
    Ok(Some(Injection { text: render_rules_text(&texts), rule_ids: chosen.iter().map(|r| r.id.clone()).collect() }))
}

// ====================================================================== RPC

fn notify_changed(c: &Ctx, change: RuleChanged) {
    c.clients.broadcast(methods::RULE_CHANGED, change);
}

fn status_changes_to_active(old: Option<&Rule>, new: &Rule) -> bool {
    new.status == RuleStatus::Active
        && old.is_none_or(|o| o.status != RuleStatus::Active || o.text != new.text || o.project != new.project)
}

/// 改动之后的生效规则会不会超出字数上限（`new` 是改动之后的这一条，`all` 里是改动之前的全部规则）。
fn check_after(c: &Ctx, all: &[Rule], new: &Rule, field: &str) -> RpcResult<()> {
    let same = |a: &str, b: &str| c.workspace.same_project_name(a, b);
    let mut active: Vec<&Rule> = all.iter().filter(|r| r.status == RuleStatus::Active && r.id != new.id).collect();
    active.push(new);
    let after = budget_of(&active, &same);
    check_budget(&after, new.project.as_deref(), &same).map_err(|bytes| over_budget(field, bytes))
}

/// `rule.list`。
pub async fn list(c: &Ctx, p: RuleListParams) -> RpcResult<RuleListResult> {
    let all = c.store.rules_all().await.map_err(internal)?;
    let same = |a: &str, b: &str| c.workspace.same_project_name(a, b);
    let budget = budget_of(&active_of(&all), &same).to_wire();
    let project = p.project.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rules = all
        .into_iter()
        .filter(|r| p.status.is_none_or(|s| r.status == s))
        .filter(|r| project.is_none_or(|p| r.project.as_deref().is_some_and(|rp| same(rp, p))))
        .collect();
    Ok(RuleListResult { rules, budget })
}

/// `rule.add`：按 `id` 幂等，`id` 已经存在就原样返回已有的那一条（不改它、不审计、不通知）。
pub async fn add(c: &Ctx, caller: &Caller, p: RuleAddParams) -> RpcResult<Rule> {
    crate::todos::check_id(&p.id)?;
    let _w = c.store.rule_lock().lock().await;
    if let Some(existing) = c.store.rule_get(&p.id).await.map_err(internal)? {
        return Ok(existing);
    }
    let text = check_text(&p.text)?;
    let project = match p.project.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(crate::todos::check_project(s)?),
        _ => None,
    };
    // 调用方自报是 agent、或者来自自报了 `mcp` 的本机连接：只能写待确认的，来源记成 agent（§ 23.2）
    let by_agent = p.agent.unwrap_or(false) || caller.is_mcp();
    let device_id = (!caller.local).then(|| caller.device_id.clone());
    let (status, source) = if by_agent {
        // `origin_session` 只在这条规则记成 agent 写的、并且这个会话存在时才记，否则丢掉这个字段（同 `todo.add`，§ 21.2）
        let session_id = match p.origin_session.as_deref() {
            Some(s) if c.store.session_exists(s).await.map_err(internal)? => Some(s.to_string()),
            _ => None,
        };
        (
            RuleStatus::Pending,
            RuleSource { kind: RuleSourceKind::Agent, session_id, seq: None, request_id: None, device_id },
        )
    } else {
        let status = match p.status.unwrap_or(RuleAddStatus::Active) {
            RuleAddStatus::Active => RuleStatus::Active,
            RuleAddStatus::Pending => RuleStatus::Pending,
        };
        let src = p.source.as_ref();
        let kind = match src.map(|s| s.kind).unwrap_or(RuleAddSourceKind::Manual) {
            RuleAddSourceKind::Manual => RuleSourceKind::Manual,
            RuleAddSourceKind::Answer => RuleSourceKind::Answer,
        };
        // 只有 `answer` 带出处（哪个会话的哪个请求）；`manual` 给了也不记
        let (session_id, request_id) = match (kind, src) {
            (RuleSourceKind::Answer, Some(s)) => (
                s.session_id.as_deref().map(|v| check_source_id("source.session_id", v)).transpose()?,
                s.request_id.as_deref().map(|v| check_source_id("source.request_id", v)).transpose()?,
            ),
            _ => (None, None),
        };
        (status, RuleSource { kind, session_id, seq: None, request_id, device_id })
    };

    check_capacity(&c.store, by_agent).await?;
    let now = now_ms();
    let rule = Rule {
        id: p.id,
        text,
        project,
        status,
        sync: RuleSync::Node,
        source,
        created_ms: now,
        updated_ms: now,
        last_injected_ms: None,
    };
    if rule.status == RuleStatus::Active {
        let all = c.store.rules_all().await.map_err(internal)?;
        check_after(c, &all, &rule, "text")?;
    }
    insert(c, caller, &rule).await?;
    Ok(rule)
}

/// 调用者持有 rule_lock；新建规则与提炼共用名额检查。
pub(crate) async fn check_capacity(store: &Store, by_agent: bool) -> RpcResult<()> {
    if store.rule_count().await.map_err(internal)? >= MAX_RULES {
        return Err(invalid("rules", Some("too_many"), format!("at most {MAX_RULES} rules")));
    }
    if by_agent && store.rule_count_agent_pending().await.map_err(internal)? >= MAX_AGENT_PENDING {
        return Err(invalid(
            "rules",
            Some("too_many_pending"),
            format!(
                "at most {MAX_AGENT_PENDING} agent-proposed rules can wait for review; confirm or reject some first"
            ),
        ));
    }
    Ok(())
}

/// 调用者持有 rule_lock；所有新规则共用存储、审计和通知路径。
pub(crate) async fn insert(c: &Ctx, caller: &Caller, rule: &Rule) -> RpcResult<()> {
    c.store.rule_insert(rule).await.map_err(internal)?;
    // 只记长度（按字符）、项目名、状态和来源种类，不记文字
    let mut detail = json!({
        "text_len": rule.text.chars().count(),
        "source_kind": rule.source.kind.as_str(),
        "status": rule.status.as_str(),
    });
    if let Some(project) = &rule.project {
        detail["project"] = json!(project);
    }
    c.audit.record(caller, "rule.add", Some(&rule.id), detail).await;
    notify_changed(c, RuleChanged { id: rule.id.clone(), rule: Some(rule.clone()), deleted: false });
    Ok(())
}

/// `if_updated_ms` 给了而和现在的不同 → `invalid`（`reason = "changed"`，`data.updated_ms` 是现在的值）。
fn check_unchanged(cur: &Rule, if_updated_ms: Option<u64>) -> RpcResult<()> {
    match if_updated_ms {
        Some(expected) if cur.updated_ms != expected => {
            let mut e = invalid("if_updated_ms", Some("changed"), "the rule changed since it was read".into());
            if let Some(d) = e.data.as_mut() {
                d.updated_ms = Some(cur.updated_ms);
            }
            Err(e)
        }
        _ => Ok(()),
    }
}

/// `rule.update`：缺省的字段保持不变，`clear` 在合并之后生效；没有任何字段真的变了就是空操作（不改 `updated_ms`、不通知、不审计）。
pub async fn update(c: &Ctx, caller: &Caller, p: RuleUpdateParams) -> RpcResult<Rule> {
    crate::todos::check_id(&p.id)?;
    // 自报了 `mcp` 的本机连接不能改规则：和 agent 不能修改清单项是同一条界线，护栏（§ 23.2）
    if caller.is_mcp() {
        return Err(forbidden());
    }
    let _w = c.store.rule_lock().lock().await;
    let old = c.store.rule_get(&p.id).await.map_err(internal)?.ok_or_else(|| not_found(&p.id))?;
    check_unchanged(&old, p.if_updated_ms)?;
    let mut new = old.clone();
    if let Some(t) = &p.text {
        new.text = check_text(t)?;
    }
    if let Some(pr) = &p.project {
        new.project = Some(crate::todos::check_project(pr)?);
    }
    if let Some(s) = p.status {
        if s == RuleStatus::Unknown {
            return Err(invalid("status", None, "status must be pending, active, disabled or rejected".into()));
        }
        new.status = s;
    }
    for f in &p.clear {
        match f {
            ClearableRuleField::Project => new.project = None,
        }
    }

    let mut fields: Vec<&str> = Vec::new();
    if new.text != old.text {
        fields.push("text");
    }
    if new.project != old.project {
        fields.push("project");
    }
    if new.status != old.status {
        fields.push("status");
    }
    if fields.is_empty() {
        return Ok(old);
    }
    // 把否决的规则改回别的状态，和新建一样占一个名额
    if old.status == RuleStatus::Rejected
        && new.status != RuleStatus::Rejected
        && c.store.rule_count().await.map_err(internal)? >= MAX_RULES
    {
        return Err(invalid("rules", Some("too_many"), format!("at most {MAX_RULES} rules")));
    }
    // 变成生效、或者生效的规则改了文字 / 项目：检查字数上限（§ 23.4）
    if status_changes_to_active(Some(&old), &new) {
        let all = c.store.rules_all().await.map_err(internal)?;
        let field = if fields.contains(&"status") { "status" } else { fields[0] };
        check_after(c, &all, &new, field)?;
    }
    new.updated_ms = crate::todos::next_updated_ms(now_ms(), old.updated_ms);
    c.store.rule_save(&new).await.map_err(internal)?;
    let mut detail = json!({ "fields": fields });
    if fields.contains(&"text") {
        detail["text_len"] = json!(new.text.chars().count());
    }
    if new.status != old.status {
        detail["status"] = json!({ "from": old.status, "to": new.status });
    }
    c.audit.record(caller, "rule.update", Some(&new.id), detail).await;
    notify_changed(c, RuleChanged { id: new.id.clone(), rule: Some(new.clone()), deleted: false });
    // `rejected` 的规则有自己的上限：满了就删掉最早否决的那条（§ 23.7）
    if new.status == RuleStatus::Rejected && old.status != RuleStatus::Rejected {
        evict_oldest_rejected(c, caller, &new.id).await;
    }
    Ok(new)
}

/// `rejected` 的规则超过 [`MAX_REJECTED`] 时删掉最早否决的（`except` 是刚否决的那条）。调用方持规则锁。
async fn evict_oldest_rejected(c: &Ctx, caller: &Caller, except: &str) {
    while c.store.rule_count_rejected().await.is_ok_and(|n| n > MAX_REJECTED) {
        let Ok(Some(oldest)) = c.store.rule_oldest_rejected(except).await else { return };
        match c.store.rule_delete(&oldest).await {
            Ok(true) => {
                c.audit.record(caller, "rule.delete", Some(&oldest), json!({ "auto": "rejected_cap" })).await;
                notify_changed(c, RuleChanged { id: oldest, rule: None, deleted: true });
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, "cannot delete an old rejected rule");
                return;
            }
        }
    }
}

/// `rule.delete`：物理删除。`if_updated_ms` 对不上什么都不删。检查和删除在同一把锁里。
pub async fn delete(c: &Ctx, caller: &Caller, p: RuleDeleteParams) -> RpcResult<()> {
    crate::todos::check_id(&p.id)?;
    if caller.is_mcp() {
        return Err(forbidden());
    }
    let _w = c.store.rule_lock().lock().await;
    let cur = c.store.rule_get(&p.id).await.map_err(internal)?.ok_or_else(|| not_found(&p.id))?;
    check_unchanged(&cur, p.if_updated_ms)?;
    if !c.store.rule_delete(&p.id).await.map_err(internal)? {
        return Err(not_found(&p.id));
    }
    c.audit.record(caller, "rule.delete", Some(&p.id), json!({})).await;
    notify_changed(c, RuleChanged { id: p.id, rule: None, deleted: true });
    Ok(())
}

/// `rule.preview`：现在开一个这个项目的会话，会注入的那段文字。只读。
pub async fn preview(c: &Ctx, p: RulePreviewParams) -> RpcResult<RulePreviewResult> {
    let project = match p.project.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(crate::todos::check_project(s)?),
        _ => None,
    };
    let inj = injection_for(&c.store, &c.workspace, project.as_deref()).await.map_err(internal)?;
    Ok(match inj {
        Some(i) => RulePreviewResult { bytes: i.text.len() as u64, text: i.text, rule_ids: i.rule_ids },
        None => RulePreviewResult { text: String::new(), rule_ids: Vec::new(), bytes: 0 },
    })
}

/// 这些规则刚被注入了一个会话（建会话时调用）。失败只记日志：它只是个「最近用到」的标记。
pub async fn mark_injected(store: &Store, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let unique: HashSet<&String> = ids.iter().collect();
    let ids: Vec<String> = unique.into_iter().cloned().collect();
    if let Err(e) = store.rules_mark_injected(&ids, now_ms()).await {
        tracing::warn!(error = %e, "cannot record that rules were injected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, project: Option<&str>, text: &str, created_ms: u64) -> Rule {
        Rule {
            id: id.into(),
            text: text.into(),
            project: project.map(str::to_string),
            status: RuleStatus::Active,
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

    fn same(a: &str, b: &str) -> bool {
        a == b
    }

    #[test]
    fn the_template_is_exact_and_indents_continuation_lines() {
        assert_eq!(render_rules_text(&[]), "");
        assert_eq!(
            render_rules_text(&[
                "Write the failing test first.",
                "Ask before adding a dependency.\nExplain why.\n\nThen wait."
            ]),
            "Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:\n\
             - Write the failing test first.\n\
             - Ask before adding a dependency.\n  Explain why.\n  \n  Then wait."
        );
    }

    #[test]
    fn a_rule_cannot_fake_a_new_item_or_the_header_at_the_start_of_a_line() {
        let r = render_rules_text(&["first\n- second"]);
        assert!(r.ends_with("- first\n  - second"), "{r}");
        for line in r.lines().skip(1) {
            assert!(line.starts_with("- ") || line.starts_with("  "), "{line:?}");
        }
    }

    #[test]
    fn rules_come_node_wide_first_then_the_project_each_in_the_given_order() {
        let rs = [
            rule("a", Some("web"), "web one", 1),
            rule("b", None, "node one", 2),
            rule("c", Some("api"), "api one", 3),
            rule("d", None, "node two", 4),
            rule("e", Some("web"), "web two", 5),
        ];
        let active: Vec<&Rule> = rs.iter().collect();
        let ids = |p| select_for_session(&active, p, &same).iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(Some("web")), ["b", "d", "a", "e"]);
        assert_eq!(ids(Some("api")), ["b", "d", "c"]);
        assert_eq!(ids(None), ["b", "d"]);
        assert_eq!(ids(Some("nothing")), ["b", "d"]);
    }

    #[test]
    fn the_budget_counts_the_template_the_node_rules_and_each_project_separately() {
        let header = TEMPLATE_HEADER.len();
        assert_eq!(budget_of(&[], &same), Budget::default());
        let rs = [rule("a", None, "abc", 1), rule("b", Some("web"), "de\nf", 2), rule("c", Some("web"), "g", 3)];
        let active: Vec<&Rule> = rs.iter().collect();
        let b = budget_of(&active, &same);
        // "\n- abc" = 6 bytes
        assert_eq!(b.node_bytes, header + 6);
        // "\n- de\n  f" = 9, "\n- g" = 4
        assert_eq!(b.projects, vec![("web".to_string(), 13)]);
        // the rendered text for a web session is exactly node_bytes + web
        let text = render_rules_text(&["abc", "de\nf", "g"]);
        assert_eq!(text.len(), b.session_bytes(Some("web"), &same));
        assert_eq!(render_rules_text(&["abc"]).len(), b.session_bytes(None, &same));
        // only projects: the header is still counted once
        let only = [rule("p", Some("api"), "xy", 1)];
        let b = budget_of(&only.iter().collect::<Vec<_>>(), &same);
        assert_eq!((b.node_bytes, b.projects.clone()), (header, vec![("api".to_string(), 5)]));
        assert_eq!(render_rules_text(&["xy"]).len(), b.session_bytes(Some("api"), &same));
    }

    #[test]
    fn chinese_text_is_measured_in_bytes_not_characters() {
        let t = "中".repeat(100);
        assert_eq!(item_bytes(&t), 1 + 2 + 300);
    }

    #[test]
    fn a_node_wide_rule_is_checked_against_the_largest_project_and_a_project_rule_against_its_own() {
        let fill = |n: usize| "x".repeat(n);
        let header = TEMPLATE_HEADER.len();
        // node = header + 1 + 2 + 600 ; two projects: small and large
        let rs = [
            rule("n", None, &fill(600), 1),
            rule("s", Some("small"), &fill(100), 2),
            rule("l", Some("large"), &fill(600), 3),
        ];
        let b = budget_of(&rs.iter().collect::<Vec<_>>(), &same);
        let node = header + 603;
        assert_eq!(b.node_bytes, node);
        // a node-wide rule: node + the largest project (603)
        assert_eq!(check_budget(&b, None, &same), Ok(()));
        // grow the largest project until node + largest is just over the cap
        let room = BUDGET_CAP - node - 603;
        let mut big = rs.clone();
        big[2].text = fill(600 + room + 1);
        let b = budget_of(&big.iter().collect::<Vec<_>>(), &same);
        assert_eq!(check_budget(&b, None, &same), Err(BUDGET_CAP + 1));
        // the small project is still fine: only its own total counts
        assert_eq!(check_budget(&b, Some("small"), &same), Ok(()));
        assert_eq!(check_budget(&b, Some("large"), &same), Err(BUDGET_CAP + 1));
    }

    #[test]
    fn exactly_the_cap_is_allowed() {
        let header = TEMPLATE_HEADER.len();
        let n = BUDGET_CAP - header - 3;
        let r = [rule("a", None, &"x".repeat(n), 1)];
        let b = budget_of(&r.iter().collect::<Vec<_>>(), &same);
        assert_eq!(b.node_bytes, BUDGET_CAP);
        assert_eq!(check_budget(&b, None, &same), Ok(()));
        let r = [rule("a", None, &"x".repeat(n + 1), 1)];
        let b = budget_of(&r.iter().collect::<Vec<_>>(), &same);
        assert_eq!(check_budget(&b, None, &same), Err(BUDGET_CAP + 1));
    }

    #[test]
    fn the_caller_s_own_append_follows_after_a_blank_line() {
        assert_eq!(prepend_to_append("R", None), "R");
        assert_eq!(prepend_to_append("R", Some("")), "R");
        assert_eq!(prepend_to_append("R", Some("mine")), "R\n\nmine");
    }

    #[test]
    fn text_validation_follows_the_spec() {
        assert_eq!(check_text("  a\nb  ").unwrap(), "a\nb");
        assert!(check_text("   ").is_err());
        assert!(check_text(&"a".repeat(600)).is_ok());
        assert!(check_text(&"a".repeat(601)).is_err());
        assert!(check_text(&"中".repeat(600)).is_ok());
        assert!(check_text(&"中".repeat(601)).is_err());
        for bad in ["a\tb", "a\rb", "a\u{0}b", "a\u{2028}b", "a\u{2029}b", "a\u{85}b", "a\u{1b}[0m"] {
            assert!(check_text(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn it_only_becomes_a_budget_question_when_a_rule_turns_active_or_an_active_rule_changes() {
        let old = rule("a", None, "t", 1);
        let mut new = old.clone();
        assert!(!status_changes_to_active(Some(&old), &new));
        new.text = "u".into();
        assert!(status_changes_to_active(Some(&old), &new));
        let mut disabled = old.clone();
        disabled.status = RuleStatus::Disabled;
        assert!(status_changes_to_active(Some(&disabled), &old));
        // a disabled rule edited but still disabled: no check
        let mut edited = disabled.clone();
        edited.text = "u".into();
        assert!(!status_changes_to_active(Some(&disabled), &edited));
    }
}
