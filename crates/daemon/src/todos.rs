//! 清单（`todo.*`，docs/spec/daemon.md § 21）：每个节点一份个人待办，存在 daemon 的 SQLite 里。
//!
//! 所有写操作（新建、修改、删除、保留期清理）在 [`Store::todo_lock`](crate::store::Store::todo_lock) 里串行：
//! 「id 已存在就原样返回」「未关闭的项 ≤ 1000」「未收下的项 ≤ 100」都是读了再写，并发的两次不能各自通过检查。
//! 新建 / 修改 / 删除之后向所有在线客户端发 `todo.changed`，并写审计（§ 21.5）。
//!
//! `title` / `body` 是用户内容：不进日志，不进审计（审计只记长度和字段名）。

use std::sync::Arc;

use anycon_protocol::{
    methods, ClearableTodoField, ErrorCode, RpcError, Todo, TodoAddParams, TodoChanged, TodoDeleteParams, TodoIdParams,
    TodoListParams, TodoListResult, TodoSource, TodoSourceKind, TodoState, TodoUpdateParams,
};
use futures::StreamExt;
use serde_json::json;
use sqlx::Row;

use crate::clients::{Caller, Clients};
use crate::daemon::Ctx;
use crate::store::{Store, StoreError};
use crate::util::now_ms;

/// 标题的字符数上限（去掉首尾空白之后）。
pub const MAX_TITLE_CHARS: usize = 200;
/// 正文的字节上限。
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// 项目名的字符数上限（不校验项目存在，只防止塞进超长的东西）。
pub const MAX_PROJECT_CHARS: usize = 200;
/// 没关闭的项最多这么多。
pub const MAX_OPEN: i64 = 1000;
/// 没被收下（`unreviewed`）的项最多这么多：agent 占不满清单。
pub const MAX_UNREVIEWED: i64 = 100;
/// `todo.list` 的 `limit` 缺省值和上限。
pub const DEFAULT_LIMIT: u32 = 200;
pub const MAX_LIMIT: u32 = 1000;
/// 一页各项序列化后的字节预算：经 hub 的一条响应有上限（重组后的密文 ≤ 16 MiB），1000 项各带 16 KiB 正文会超过它。
pub const PAGE_BYTE_BUDGET: usize = 1024 * 1024;

type RpcResult<T> = Result<T, RpcError>;

fn internal(e: StoreError) -> RpcError {
    // 只带数据库错误本身，不带任何清单内容
    tracing::warn!(error = %e, "todo storage error");
    RpcError::new(ErrorCode::Internal, "todo storage error")
}

/// `invalid`，带 `data.field`（出问题的参数）和可选的 `data.reason`。
fn invalid(field: &str, reason: Option<&str>, message: String) -> RpcError {
    let mut e = RpcError::new(ErrorCode::Invalid, message);
    if let Some(d) = e.data.as_mut() {
        d.field = Some(field.to_string());
        d.reason = reason.map(str::to_string);
    }
    e
}

fn not_found(id: &str) -> RpcError {
    RpcError::new(ErrorCode::NotFound, format!("todo {id} not found"))
}

// ---------------- 校验 ----------------

/// `id` 必须是小写带连字符的 UUID（客户端生成，`todo.add` 按它幂等）。不做规范化：客户端自己留着一份 id，
/// 悄悄改写会让它对不上；`todo.add` / `todo.update` / `todo.delete` 对不规范的 id 一律 `invalid`（`data.field = "id"`）。
pub(crate) fn check_id(id: &str) -> RpcResult<()> {
    let canonical = uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id);
    if canonical {
        Ok(())
    } else {
        Err(invalid("id", None, "id must be a lowercase hyphenated UUID".into()))
    }
}

/// 标题：去掉首尾空白之后 1–200 个 Unicode 码点，单行（不含控制字符，也不含 U+2028 / U+2029 这两个行分隔符）。返回去掉空白后的标题。
fn check_title(title: &str) -> RpcResult<String> {
    let t = title.trim();
    if t.is_empty() {
        return Err(invalid("title", None, "title must not be empty".into()));
    }
    if t.chars().count() > MAX_TITLE_CHARS {
        return Err(invalid("title", None, format!("title must be at most {MAX_TITLE_CHARS} characters")));
    }
    if t.chars().any(crate::config::is_line_break_or_control) {
        return Err(invalid("title", None, "title must be a single line".into()));
    }
    Ok(t.to_string())
}

fn check_body(body: &str) -> RpcResult<()> {
    if body.len() > MAX_BODY_BYTES {
        return Err(invalid("body", None, format!("body must be at most {} KiB", MAX_BODY_BYTES / 1024)));
    }
    Ok(())
}

pub(crate) fn check_project(project: &str) -> RpcResult<String> {
    let p = project.trim();
    if p.is_empty() || p.chars().count() > MAX_PROJECT_CHARS || p.chars().any(crate::config::is_line_break_or_control) {
        return Err(invalid(
            "project",
            None,
            format!("project must be 1-{MAX_PROJECT_CHARS} characters without control characters"),
        ));
    }
    Ok(p.to_string())
}

fn check_due(due_ms: u64) -> RpcResult<u64> {
    if due_ms > i64::MAX as u64 {
        return Err(invalid("due_ms", None, "due_ms is out of range".into()));
    }
    Ok(due_ms)
}

// ---------------- 存储 ----------------

fn row_to_todo(r: &sqlx::sqlite::SqliteRow) -> Todo {
    // 库里不认识的 `source_kind` 字符串（比如降级到旧版本之后读到更新版本写的种类）按 `local` 读：这是一个已知的降级行为，
    // 旧版本不知道那个种类，更新的版本会把它读回原样。这里不改它。
    let kind = match r.get::<String, _>("source_kind").as_str() {
        "device" => TodoSourceKind::Device,
        "agent" => TodoSourceKind::Agent,
        _ => TodoSourceKind::Local,
    };
    let state = match r.get::<String, _>("state").as_str() {
        "working" => TodoState::Working,
        "waiting" => TodoState::Waiting,
        "review" => TodoState::Review,
        "done" => TodoState::Done,
        "dropped" => TodoState::Dropped,
        _ => TodoState::Open,
    };
    Todo {
        id: r.get("id"),
        title: r.get("title"),
        body: r.get("body"),
        project: r.get("project"),
        state,
        due_ms: r.get::<Option<i64>, _>("due_ms").map(|v| v as u64),
        source: TodoSource { kind, device_id: r.get("source_device"), session_id: r.get("source_session") },
        unreviewed: r.get::<i64, _>("unreviewed") != 0,
        created_ms: r.get::<i64, _>("created_ms") as u64,
        updated_ms: r.get::<i64, _>("updated_ms") as u64,
        closed_ms: r.get::<Option<i64>, _>("closed_ms").map(|v| v as u64),
        // 关联的会话不在 `todos` 表里，读出来之后由 `todo_get` / `todo_page` 补上；`activity` 是算出来的，不存
        sessions: Vec::new(),
        activity: None,
    }
}

fn source_kind_str(k: TodoSourceKind) -> &'static str {
    match k {
        TodoSourceKind::Device => "device",
        TodoSourceKind::Local => "local",
        TodoSourceKind::Agent => "agent",
        // daemon 自己不产生 `Unknown`（它只在客户端解析更新版本的数据时出现），`todo_insert` 也拒绝写它；
        // 这里只是让匹配保持完整，并且绝不能落到人的来源上
        TodoSourceKind::Unknown => "unknown",
    }
}

/// [`Store::todo_save`]，可以在事务里用（派发把这一项的修改、关联和「经过」放在同一个事务里）。
pub(crate) async fn save_todo_on<'e, E>(db: E, t: &Todo) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "UPDATE todos SET title = ?, body = ?, project = ?, state = ?, due_ms = ?, unreviewed = ?, \
         updated_ms = ?, closed_ms = ? WHERE id = ?",
    )
    .bind(&t.title)
    .bind(&t.body)
    .bind(&t.project)
    .bind(t.state.as_str())
    .bind(t.due_ms.map(|v| v as i64))
    .bind(t.unreviewed as i64)
    .bind(t.updated_ms as i64)
    .bind(t.closed_ms.map(|v| v as i64))
    .bind(&t.id)
    .execute(db)
    .await?;
    Ok(())
}

/// 每次真正的修改把 `updated_ms` 设成 `max(现在, 旧值 + 1)`：同一毫秒里的两次写、或者时钟往回拨，都不会让内容变了而 `updated_ms` 不变。
/// 客户端靠 `updated_ms` 判断「这一项自从我读到之后有没有被改过」（`todo.delete` 的 `if_updated_ms`），所以它对同一项必须严格递增。
pub(crate) fn next_updated_ms(now: u64, old: u64) -> u64 {
    now.max(old.saturating_add(1))
}

impl Store {
    pub async fn todo_get(&self, id: &str) -> Result<Option<Todo>, StoreError> {
        let r = sqlx::query("SELECT * FROM todos WHERE id = ?").bind(id).fetch_optional(self.pool()).await?;
        let Some(mut t) = r.as_ref().map(row_to_todo) else { return Ok(None) };
        t.sessions = self.todo_links(id).await?;
        Ok(Some(t))
    }

    /// 没关闭的项数。
    pub async fn todo_count_open(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query("SELECT COUNT(*) FROM todos WHERE state NOT IN ('done','dropped')")
            .fetch_one(self.pool())
            .await?
            .get(0))
    }

    /// 没被收下的项数。
    pub async fn todo_count_unreviewed(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query("SELECT COUNT(*) FROM todos WHERE unreviewed = 1").fetch_one(self.pool()).await?.get(0))
    }

    pub async fn todo_insert(&self, t: &Todo) -> Result<(), StoreError> {
        // `Unknown` 只存在于客户端的解析里：daemon 不能把它写进库（读回去会变成别的种类）
        if t.source.kind == TodoSourceKind::Unknown {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a todo cannot have an unknown source kind",
            )));
        }
        sqlx::query(
            "INSERT INTO todos (id, title, body, project, state, due_ms, source_kind, source_device, source_session, \
             unreviewed, created_ms, updated_ms, closed_ms) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&t.id)
        .bind(&t.title)
        .bind(&t.body)
        .bind(&t.project)
        .bind(t.state.as_str())
        .bind(t.due_ms.map(|v| v as i64))
        .bind(source_kind_str(t.source.kind))
        .bind(&t.source.device_id)
        .bind(&t.source.session_id)
        .bind(t.unreviewed as i64)
        .bind(t.created_ms as i64)
        .bind(t.updated_ms as i64)
        .bind(t.closed_ms.map(|v| v as i64))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// 写回可以变的字段（来源和创建时间不变）。
    pub async fn todo_save(&self, t: &Todo) -> Result<(), StoreError> {
        save_todo_on(self.pool(), t).await
    }

    /// 物理删除。返回有没有这一项。
    pub async fn todo_delete(&self, id: &str) -> Result<bool, StoreError> {
        // 项和它的关联、「经过」在同一个事务里删（§ 22.3）
        let mut tx = self.pool().begin().await?;
        let deleted =
            sqlx::query("DELETE FROM todos WHERE id = ?").bind(id).execute(&mut *tx).await?.rows_affected() > 0;
        if deleted {
            sqlx::query("DELETE FROM todo_sessions WHERE todo_id = ?").bind(id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM todo_events WHERE todo_id = ?").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// 关闭时间早于 `before_ms` 的项 id（`history_retention`）。
    pub async fn todos_closed_before(&self, before_ms: u64) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT id FROM todos WHERE state IN ('done','dropped') AND closed_ms IS NOT NULL AND closed_ms < ?",
        )
        .bind(before_ms as i64)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    /// 一页：`(updated_ms, id)` 从新到旧。按字节预算和条数截断，`has_more` 表示后面还有没给出的项。
    pub async fn todo_page(&self, p: &TodoListParams, limit: usize) -> Result<TodoListResult, StoreError> {
        // 固定的 SQL，可选的过滤条件用 `? IS NULL OR …` 表达（不拼接字符串）
        let (before_ms, before_id) = match &p.before {
            Some(b) => (Some(b.updated_ms.min(i64::MAX as u64) as i64), Some(b.id.clone())),
            None => (None, None),
        };
        let q = sqlx::query(
            "SELECT * FROM todos \
             WHERE (? = 1 OR state NOT IN ('done','dropped')) \
               AND (? IS NULL OR project = ?) \
               AND (? IS NULL OR updated_ms < ? OR (updated_ms = ? AND id < ?)) \
             ORDER BY updated_ms DESC, id DESC",
        )
        .bind(p.include_closed.unwrap_or(false) as i64)
        .bind(&p.project)
        .bind(&p.project)
        .bind(before_ms)
        .bind(before_ms)
        .bind(before_ms)
        .bind(before_id);
        let mut rows = q.fetch(self.pool());
        let (mut todos, mut bytes, mut has_more) = (Vec::new(), 0usize, false);
        while let Some(row) = rows.next().await {
            let t = row_to_todo(&row?);
            let size = serde_json::to_vec(&t).map(|v| v.len()).unwrap_or(0);
            // 至少给一项；之后再放一项会超过预算就停
            if todos.len() >= limit || (!todos.is_empty() && bytes + size > PAGE_BYTE_BUDGET) {
                has_more = true;
                break;
            }
            bytes += size;
            todos.push(t);
        }
        drop(rows);
        let links = self.todo_links_of(&todos.iter().map(|t| t.id.clone()).collect::<Vec<_>>()).await?;
        for t in &mut todos {
            t.sessions = links.get(&t.id).cloned().unwrap_or_default();
        }
        Ok(TodoListResult { todos, has_more })
    }
}

// ---------------- RPC ----------------

fn notify_changed(clients: &Clients, change: TodoChanged) {
    clients.broadcast(methods::TODO_CHANGED, change);
}

/// `todo.list`。
pub async fn list(c: &Ctx, mut p: TodoListParams) -> RpcResult<TodoListResult> {
    // 项目名存的是去掉首尾空白之后的，过滤条件同样处理
    p.project = p.project.map(|s| s.trim().to_string());
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let mut page = c.store.todo_page(&p, limit).await.map_err(internal)?;
    crate::todo_dispatch::fill_activity(&c.sessions, &mut page.todos).await;
    Ok(page)
}

/// 这一项是谁记的（§ 21.2）。`agent` 是调用方自报的；自报了 `mcp` 的本机连接总是算 agent。
async fn source_of(c: &Ctx, caller: &Caller, agent: bool, origin_session: Option<&str>) -> RpcResult<TodoSource> {
    if agent || caller.is_mcp() {
        // `origin_session` 只在这一项记成 agent 加的、并且这个会话存在时才记，否则丢掉这个字段
        let session_id = match origin_session {
            Some(s) if c.store.session_exists(s).await.map_err(internal)? => Some(s.to_string()),
            _ => None,
        };
        let device_id = (!caller.local).then(|| caller.device_id.clone());
        return Ok(TodoSource { kind: TodoSourceKind::Agent, device_id, session_id });
    }
    if caller.local {
        Ok(TodoSource { kind: TodoSourceKind::Local, device_id: None, session_id: None })
    } else {
        Ok(TodoSource { kind: TodoSourceKind::Device, device_id: Some(caller.device_id.clone()), session_id: None })
    }
}

/// `todo.add`：按 `id` 幂等，`id` 已经存在就原样返回已有的那一项（不改它、不审计、不通知）。
pub async fn add(c: &Ctx, caller: &Caller, p: TodoAddParams) -> RpcResult<Todo> {
    check_id(&p.id)?;
    let _w = c.store.todo_lock().lock().await;
    if let Some(mut existing) = c.store.todo_get(&p.id).await.map_err(internal)? {
        existing.activity = crate::todo_dispatch::compute_activity(&c.sessions, &existing).await;
        return Ok(existing);
    }
    let title = check_title(&p.title)?;
    let body = p.body.unwrap_or_default();
    check_body(&body)?;
    // 空白的项目名按没给处理（`--project ""`）
    let project = match p.project.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(check_project(s)?),
        _ => None,
    };
    let due_ms = p.due_ms.map(check_due).transpose()?;
    let source = source_of(c, caller, p.agent.unwrap_or(false), p.origin_session.as_deref()).await?;
    let unreviewed = source.kind == TodoSourceKind::Agent;

    if c.store.todo_count_open().await.map_err(internal)? >= MAX_OPEN {
        return Err(invalid("todos", Some("too_many"), format!("at most {MAX_OPEN} open todos")));
    }
    if unreviewed && c.store.todo_count_unreviewed().await.map_err(internal)? >= MAX_UNREVIEWED {
        return Err(invalid(
            "todos",
            Some("too_many_unreviewed"),
            format!("at most {MAX_UNREVIEWED} agent-added todos can wait for review; review or delete some first"),
        ));
    }

    let now = now_ms();
    let todo = Todo {
        id: p.id,
        title,
        body,
        project,
        state: TodoState::Open,
        due_ms,
        source,
        unreviewed,
        created_ms: now,
        updated_ms: now,
        closed_ms: None,
        sessions: Vec::new(),
        activity: None,
    };
    c.store.todo_insert(&todo).await.map_err(internal)?;
    // 只记长度（标题按字符、正文按字节）和项目名，不记内容
    let mut detail = json!({
        "title_len": todo.title.chars().count(),
        "body_len": todo.body.len(),
        "source_kind": source_kind_str(todo.source.kind),
    });
    if let Some(project) = &todo.project {
        detail["project"] = json!(project);
    }
    c.audit.record(caller, "todo.add", Some(&todo.id), detail).await;
    notify_changed(&c.clients, TodoChanged { id: todo.id.clone(), todo: Some(todo.clone()), deleted: false });
    Ok(todo)
}

/// `todo.update`：缺省的字段保持不变，`clear` 在合并之后生效；没有任何字段真的变了就是空操作。
pub async fn update(c: &Ctx, caller: &Caller, p: TodoUpdateParams) -> RpcResult<Todo> {
    check_id(&p.id)?;
    let _w = c.store.todo_lock().lock().await;
    let old = c.store.todo_get(&p.id).await.map_err(internal)?.ok_or_else(|| not_found(&p.id))?;
    let mut new = old.clone();
    if let Some(t) = &p.title {
        new.title = check_title(t)?;
    }
    if let Some(b) = &p.body {
        check_body(b)?;
        new.body = b.clone();
    }
    if let Some(pr) = &p.project {
        new.project = Some(check_project(pr)?);
    }
    if let Some(d) = p.due_ms {
        new.due_ms = Some(check_due(d)?);
    }
    if let Some(s) = p.state {
        new.state = s;
    }
    for f in &p.clear {
        match f {
            ClearableTodoField::Project => new.project = None,
            ClearableTodoField::DueMs => new.due_ms = None,
        }
    }

    let mut fields: Vec<&str> = Vec::new();
    if new.title != old.title {
        fields.push("title");
    }
    if new.body != old.body {
        fields.push("body");
    }
    if new.project != old.project {
        fields.push("project");
    }
    if new.due_ms != old.due_ms {
        fields.push("due_ms");
    }
    if new.state != old.state {
        fields.push("state");
    }
    // `unreviewed` 从 true 变 false：人收下它（`reviewed: true`），或者对它做了任何别的修改。
    // 自报了 `mcp` 的本机连接两条都不算：`reviewed` 被忽略，它做的修改也不算「人改过」。
    if old.unreviewed && !caller.is_mcp() && (p.reviewed == Some(true) || !fields.is_empty()) {
        new.unreviewed = false;
        fields.push("unreviewed");
    }
    if fields.is_empty() {
        let mut old = old;
        old.activity = crate::todo_dispatch::compute_activity(&c.sessions, &old).await;
        return Ok(old);
    }
    // 把关闭的项重新打开，和新建一样占一个「没关闭」的名额
    if old.state.is_closed() && !new.state.is_closed() && c.store.todo_count_open().await.map_err(internal)? >= MAX_OPEN
    {
        return Err(invalid("todos", Some("too_many"), format!("at most {MAX_OPEN} open todos")));
    }
    // 进入 done / dropped 记 closed_ms；重新打开清掉；在两个关闭状态之间换（done → dropped）保持原来的关闭时间
    let now = now_ms();
    match (old.state.is_closed(), new.state.is_closed()) {
        (false, true) => new.closed_ms = Some(now),
        (true, false) => new.closed_ms = None,
        _ => {}
    }
    new.updated_ms = next_updated_ms(now, old.updated_ms);
    c.store.todo_save(&new).await.map_err(internal)?;
    let mut detail = json!({ "fields": fields });
    if new.state != old.state {
        detail["state"] = json!({ "from": old.state, "to": new.state });
    }
    c.audit.record(caller, "todo.update", Some(&new.id), detail).await;
    // 派发过的项：状态是人改的，「经过」里记一条（§ 22.4）
    crate::todo_dispatch::record_manual_state(c, caller, &new, old.state).await;
    new.activity = crate::todo_dispatch::compute_activity(&c.sessions, &new).await;
    notify_changed(&c.clients, TodoChanged { id: new.id.clone(), todo: Some(new.clone()), deleted: false });
    Ok(new)
}

/// `todo.get`：读一项。只读：不审计、不通知。
pub async fn get(c: &Ctx, p: TodoIdParams) -> RpcResult<Todo> {
    check_id(&p.id)?;
    let mut t = c.store.todo_get(&p.id).await.map_err(internal)?.ok_or_else(|| not_found(&p.id))?;
    t.activity = crate::todo_dispatch::compute_activity(&c.sessions, &t).await;
    Ok(t)
}

/// `todo.delete`：物理删除。给了 `if_updated_ms`，而这一项现在的 `updated_ms` 不是它：什么都不删，`invalid`（`reason = "changed"`，
/// `data.updated_ms` 是现在的值），不审计、不通知。检查和删除在同一把锁里，中间没有别的写入。
pub async fn delete(c: &Ctx, caller: &Caller, p: TodoDeleteParams) -> RpcResult<()> {
    let id = p.id.as_str();
    check_id(id)?;
    let _w = c.store.todo_lock().lock().await;
    if let Some(expected) = p.if_updated_ms {
        let cur = c.store.todo_get(id).await.map_err(internal)?.ok_or_else(|| not_found(id))?;
        if cur.updated_ms != expected {
            let mut e = invalid("if_updated_ms", Some("changed"), "the todo changed since it was read".into());
            if let Some(d) = e.data.as_mut() {
                d.updated_ms = Some(cur.updated_ms);
            }
            return Err(e);
        }
    }
    if !c.store.todo_delete(id).await.map_err(internal)? {
        return Err(not_found(id));
    }
    c.audit.record(caller, "todo.delete", Some(id), json!({})).await;
    notify_changed(&c.clients, TodoChanged { id: id.to_string(), todo: None, deleted: true });
    Ok(())
}

/// 保留期清理（和会话的 `history_retention` 在同一轮里）：关闭时间早于 `cutoff_ms` 的项删掉并通知客户端；没关闭的不动。
pub async fn purge_closed(store: &Store, clients: &Arc<Clients>, cutoff_ms: u64) {
    let _w = store.todo_lock().lock().await;
    let ids = match store.todos_closed_before(cutoff_ms).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list todos past the retention period");
            return;
        }
    };
    for id in ids {
        match store.todo_delete(&id).await {
            Ok(true) => notify_changed(clients, TodoChanged { id, todo: None, deleted: true }),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "cannot delete an expired todo"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::next_updated_ms;

    #[test]
    fn updated_ms_is_the_clock_unless_that_would_not_move_it_forward() {
        assert_eq!(next_updated_ms(200, 100), 200, "the clock is ahead: use it");
        assert_eq!(next_updated_ms(100, 100), 101, "the same millisecond: one more");
        assert_eq!(next_updated_ms(50, 100), 101, "the clock stepped back: one more than the stored value");
        assert_eq!(next_updated_ms(0, u64::MAX), u64::MAX, "no overflow");
    }
}
