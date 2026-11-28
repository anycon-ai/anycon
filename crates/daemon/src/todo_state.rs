//! 周期项的跨次状态（`todo.state_*`、`todo.repeat_preview`，docs/spec/daemon.md § 26.11）：开着 `repeat.keep_state` 的模板有一张
//! 「键 → 值」小表，由它复制出来的各次会话里的 agent 读写，用来记「上次已经处理过哪些」。
//!
//! - 状态属于**模板**：参数里的 `id` 是模板或它复制出来的某一项，经 `source.todo_id` 找到模板，读写的都是模板那一份。
//! - 这是对「agent 没有往清单项里写的工具」（ADR-54）只开一格的放开：读、写、删单个键、列出、筛未见过的，agent 标记的调用方
//!   也能调；清空、开关本身、清单项的其余字段仍只由人改。带了 `origin_session` 并且这个会话存在时，核对它是从这个模板的某次复制
//!   派出的。这是护栏，不是边界：调用方可以不带 `origin_session`，有 shell 的 agent 也能清掉环境变量再调用。
//! - 读写状态不动模板和复制项的 `updated_ms`、不发 `todo.changed`：理由同 § 22.3，会话运行期间带 `if_updated_ms` 的删除和
//!   「移到另一个节点」不能因此一直失败。变化发 `todo.state_changed`，只带模板 id 和条数。
//! - 键和值是用户内容：不进日志、不进审计（审计只记长度和条数），本模块的 `tracing` 只带 id 和数字。
//! - 写入在 [`Store::state_lock`] 里串行（条数上限和淘汰是读了再写）；不和清单的锁嵌套。

use anycon_protocol::{
    methods, ErrorCode, RepeatStateChanged, RepeatStateClearParams, RepeatStateClearResult, RepeatStateDeleteParams,
    RepeatStateDeleteResult, RepeatStateEntry, RepeatStateGetParams, RepeatStateGetResult, RepeatStateListParams,
    RepeatStateListResult, RepeatStateSetParams, RepeatStateSetResult, RepeatStateUnseenParams,
    RepeatStateUnseenResult, RpcError, Todo, TodoRepeatPreviewParams, TodoRepeatPreviewResult, TodoSourceKind,
};
use serde_json::json;
use sqlx::Row;

use crate::clients::Caller;
use crate::daemon::Ctx;
use crate::store::{Store, StoreError};
use crate::todo_dispatch::{compose_segments, join_segments, state_usage_text, StateHint};
use crate::util::now_ms;

/// 键的字节上限。
pub const MAX_KEY_BYTES: usize = 200;
/// 值的字节上限。
pub const MAX_VALUE_BYTES: usize = 1024;
/// 每个模板最多这么多条；满了淘汰最久没被写过的。
pub const MAX_ENTRIES: u32 = 2000;
/// `todo.state_unseen` 一次最多给这么多个键。
pub const MAX_UNSEEN_KEYS: usize = 2000;
/// `todo.state_list` 的 `limit` 缺省值和上限。
pub const LIST_DEFAULT_LIMIT: u32 = 200;
pub const LIST_MAX_LIMIT: u32 = 1000;

type RpcResult<T> = Result<T, RpcError>;

fn internal(e: StoreError) -> RpcError {
    // 只带数据库错误本身，不带键和值
    tracing::warn!(error = %e, "todo state storage error");
    RpcError::new(ErrorCode::Internal, "todo state storage error")
}

fn with_data(code: ErrorCode, message: impl Into<String>, field: Option<&str>, reason: Option<&str>) -> RpcError {
    let mut e = RpcError::new(code, message);
    if let Some(d) = e.data.as_mut() {
        d.field = field.map(str::to_string);
        d.reason = reason.map(str::to_string);
    }
    e
}

fn invalid(field: &str, reason: &str, message: impl Into<String>) -> RpcError {
    with_data(ErrorCode::Invalid, message, Some(field), Some(reason))
}

fn forbidden(reason: &str, message: impl Into<String>) -> RpcError {
    with_data(ErrorCode::Forbidden, message, None, Some(reason))
}

/// 键：非空，≤ 200 字节，不含控制字符和 U+2028 / U+2029（一行一个键的 `unseen` 靠它）。
fn check_key(field: &str, key: &str) -> RpcResult<()> {
    if key.is_empty() {
        return Err(invalid(field, "empty", "a state key must not be empty"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(invalid(field, "length", format!("a state key is at most {MAX_KEY_BYTES} bytes")));
    }
    if key.chars().any(crate::config::is_line_break_or_control) {
        return Err(invalid(field, "control", "a state key must not contain control characters or line breaks"));
    }
    Ok(())
}

fn check_value(value: &str) -> RpcResult<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(invalid("value", "length", format!("a state value is at most {MAX_VALUE_BYTES} bytes")));
    }
    Ok(())
}

/// 一次调用落到的模板，和用量算在哪一项上（周期项复制出来的项；算不出来时没有）。
struct Target {
    template: Todo,
    attribute: Option<String>,
}

/// 找到状态所属的模板并检查调用方（§ 26.11 的「谁能调」）。`need_on`：模板必须开着 `keep_state`（清空不要求）。
async fn resolve(
    c: &Ctx,
    caller: &Caller,
    id: &str,
    agent: bool,
    origin_session: Option<&str>,
    need_on: bool,
) -> RpcResult<Target> {
    crate::todos::check_id(id)?;
    // Twin 设备在分发处已经被方法白名单挡住（§ 27.3）；本机 Twin 自己不读写状态
    if caller.is_twin() || caller.is_twin_device() {
        return Err(forbidden("twin", "the twin cannot use a repeating to-do's state"));
    }
    let todo = c
        .store
        .todo_get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("todo {id} not found")))?;
    let template = if todo.repeat.is_some() {
        todo.clone()
    } else {
        match (todo.source.kind, todo.source.todo_id.as_deref()) {
            (TodoSourceKind::Repeat, Some(tid)) => c.store.todo_get(tid).await.map_err(internal)?.ok_or_else(|| {
                invalid("id", "template_gone", "the repeating to-do this item was copied from is gone")
            })?,
            _ => {
                return Err(invalid(
                    "id",
                    "not_repeating",
                    "state belongs to a repeating to-do; this item is neither one nor a copy of one",
                ))
            }
        }
    };
    if need_on && !template.repeat.as_ref().is_some_and(|r| r.keep_state) {
        return Err(invalid("id", "state_off", "this repeating to-do does not keep state across its runs"));
    }
    // 来源会话存在时核对它是这个模板的某次复制派出的；不存在（或没带）就不核对。护栏，不是边界
    let mut attribute = None;
    if let Some(sid) = origin_session.filter(|s| !s.is_empty()) {
        if let Ok(session) = c.sessions.get_summary(sid).await {
            let owner = match session.todo_id.as_deref() {
                Some(tid) => c.store.todo_get(tid).await.map_err(internal)?,
                None => None,
            };
            let belongs = owner.as_ref().is_some_and(|o| {
                o.source.kind == TodoSourceKind::Repeat && o.source.todo_id.as_deref() == Some(template.id.as_str())
            });
            if !belongs {
                return Err(forbidden(
                    "origin_session",
                    "this session was not started from a copy of that repeating to-do",
                ));
            }
            attribute = owner.map(|o| o.id);
        }
    }
    // 用量只算 agent 的调用（自报 `mcp`、`agent: true` 或带了来源会话）：人在界面上看、改不算这一轮用过
    let agentish = agent || caller.is_mcp() || origin_session.is_some_and(|s| !s.is_empty());
    if attribute.is_none() && agentish && todo.id != template.id {
        attribute = Some(todo.id.clone());
    }
    if !agentish {
        attribute = None;
    }
    Ok(Target { template, attribute })
}

fn count_use(c: &Ctx, t: &Target, write: bool) {
    if let Some(id) = &t.attribute {
        c.sessions.todo_signal().count_state_use(id, write);
    }
}

fn notify(c: &Ctx, template_id: &str, count: u32) {
    c.clients.broadcast(methods::TODO_STATE_CHANGED, RepeatStateChanged { id: template_id.to_string(), count });
}

// ====================================================================== 存储

fn entry_of(r: &sqlx::sqlite::SqliteRow) -> RepeatStateEntry {
    RepeatStateEntry { key: r.get("key"), value: r.get("value"), written_ms: r.get::<i64, _>("written_ms") as u64 }
}

impl Store {
    /// 一个模板的条数。
    pub async fn todo_state_count(&self, template_id: &str) -> Result<u32, StoreError> {
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM todo_state WHERE template_id = ?")
            .bind(template_id)
            .fetch_one(self.pool())
            .await?
            .get(0);
        Ok(n as u32)
    }

    pub async fn todo_state_get(&self, template_id: &str, key: &str) -> Result<Option<RepeatStateEntry>, StoreError> {
        let r = sqlx::query("SELECT key, value, written_ms FROM todo_state WHERE template_id = ? AND key = ?")
            .bind(template_id)
            .bind(key)
            .fetch_optional(self.pool())
            .await?;
        Ok(r.as_ref().map(entry_of))
    }

    /// 写一个键（调用方持 [`Store::state_lock`]）；模板这时已经不在了就什么都不写，返回 `None`。返回 `(写完之后的条数, 淘汰了几条)`。
    pub async fn todo_state_set(
        &self,
        template_id: &str,
        key: &str,
        value: &str,
        now: u64,
    ) -> Result<Option<(u32, u32)>, StoreError> {
        let mut tx = self.pool().begin().await?;
        // 一条语句里同时确认模板还在：和删除模板之间不会留下没有主人的行
        let written = sqlx::query(
            "INSERT INTO todo_state (template_id, key, value, written_ms, seq) \
             SELECT ?, ?, ?, ?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM todo_state WHERE template_id = ?) \
             WHERE EXISTS (SELECT 1 FROM todos WHERE id = ?) \
             ON CONFLICT (template_id, key) DO UPDATE SET value = excluded.value, written_ms = excluded.written_ms, \
             seq = excluded.seq",
        )
        .bind(template_id)
        .bind(key)
        .bind(value)
        .bind(now as i64)
        .bind(template_id)
        .bind(template_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if written == 0 {
            return Ok(None);
        }
        let count: i64 = sqlx::query("SELECT COUNT(*) FROM todo_state WHERE template_id = ?")
            .bind(template_id)
            .fetch_one(&mut *tx)
            .await?
            .get(0);
        let over = (count - i64::from(MAX_ENTRIES)).max(0);
        if over > 0 {
            // 淘汰最久没被写过的（`seq` 最小的）；刚写的这一条 `seq` 最大，不会被淘汰
            sqlx::query(
                "DELETE FROM todo_state WHERE template_id = ? AND key IN \
                 (SELECT key FROM todo_state WHERE template_id = ? ORDER BY seq LIMIT ?)",
            )
            .bind(template_id)
            .bind(template_id)
            .bind(over)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Some(((count - over) as u32, over as u32)))
    }

    /// 删一个键（调用方持 [`Store::state_lock`]）；返回原来有没有它。
    pub async fn todo_state_delete(&self, template_id: &str, key: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM todo_state WHERE template_id = ? AND key = ?")
            .bind(template_id)
            .bind(key)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }

    /// 清空一个模板的状态；返回删了几条。
    pub async fn todo_state_clear(&self, template_id: &str) -> Result<u32, StoreError> {
        Ok(sqlx::query("DELETE FROM todo_state WHERE template_id = ?")
            .bind(template_id)
            .execute(self.pool())
            .await?
            .rows_affected() as u32)
    }

    /// 一页：按键（按字节）从小到大；`(这一页, 匹配前缀的条数, 后面还有)`。
    pub async fn todo_state_page(
        &self,
        template_id: &str,
        prefix: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<RepeatStateEntry>, u32, bool), StoreError> {
        // 固定的 SQL，可选条件用 `? IS NULL OR …`；前缀按字符比较（`substr` / `length` 对 TEXT 都按字符算）
        let rows = sqlx::query(
            "SELECT key, value, written_ms FROM todo_state WHERE template_id = ? \
               AND (? IS NULL OR substr(key, 1, length(?)) = ?) AND (? IS NULL OR key > ?) \
             ORDER BY key LIMIT ?",
        )
        .bind(template_id)
        .bind(prefix)
        .bind(prefix)
        .bind(prefix)
        .bind(after)
        .bind(after)
        .bind(limit as i64 + 1)
        .fetch_all(self.pool())
        .await?;
        let matched: i64 = sqlx::query(
            "SELECT COUNT(*) FROM todo_state WHERE template_id = ? AND (? IS NULL OR substr(key, 1, length(?)) = ?)",
        )
        .bind(template_id)
        .bind(prefix)
        .bind(prefix)
        .bind(prefix)
        .fetch_one(self.pool())
        .await?
        .get(0);
        let has_more = rows.len() > limit;
        Ok((rows.iter().take(limit).map(entry_of).collect(), matched as u32, has_more))
    }

    /// 这些键里存着的那些。
    pub async fn todo_state_present(
        &self,
        template_id: &str,
        keys: &[String],
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        if keys.is_empty() {
            return Ok(Default::default());
        }
        let rows =
            sqlx::query("SELECT key FROM todo_state WHERE template_id = ? AND key IN (SELECT value FROM json_each(?))")
                .bind(template_id)
                .bind(serde_json::to_string(keys)?)
                .fetch_all(self.pool())
                .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }
}

/// 派发这一项时第一条消息里要不要带状态的用法说明（§ 26.11、§ 22.4）：它是周期项复制出来的、模板还在、模板开着
/// `keep_state` 并且没有 `state_hint_off`。要带时给出 `(模板 id, 此刻的条数)`。
pub(crate) async fn hint_for(store: &Store, todo: &Todo) -> Result<Option<(String, u32)>, StoreError> {
    let (TodoSourceKind::Repeat, Some(tid)) = (todo.source.kind, todo.source.todo_id.as_deref()) else {
        return Ok(None);
    };
    let Some(template) = store.todo_get(tid).await? else { return Ok(None) };
    if !template.repeat.as_ref().is_some_and(|r| r.keep_state && !r.state_hint_off) {
        return Ok(None);
    }
    Ok(Some((template.id.clone(), store.todo_state_count(&template.id).await?)))
}

// ====================================================================== RPC

/// `todo.state_get`。
pub async fn get(c: &Ctx, caller: &Caller, p: RepeatStateGetParams) -> RpcResult<RepeatStateGetResult> {
    let t = resolve(c, caller, &p.id, p.agent.unwrap_or(false), p.origin_session.as_deref(), true).await?;
    check_key("key", &p.key)?;
    let entry = c.store.todo_state_get(&t.template.id, &p.key).await.map_err(internal)?;
    count_use(c, &t, false);
    Ok(RepeatStateGetResult { template_id: t.template.id, entry })
}

/// `todo.state_set`。
pub async fn set(c: &Ctx, caller: &Caller, p: RepeatStateSetParams) -> RpcResult<RepeatStateSetResult> {
    let t = resolve(c, caller, &p.id, p.agent.unwrap_or(false), p.origin_session.as_deref(), true).await?;
    check_key("key", &p.key)?;
    check_value(&p.value)?;
    let (count, evicted) = {
        let _w = c.store.state_lock().lock().await;
        c.store
            .todo_state_set(&t.template.id, &p.key, &p.value, now_ms())
            .await
            .map_err(internal)?
            .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("todo {} not found", t.template.id)))?
    };
    count_use(c, &t, true);
    // 只记长度和条数，不记键和值
    c.audit
        .record(
            caller,
            "todo.state_set",
            Some(&t.template.id),
            json!({ "key_len": p.key.len(), "value_len": p.value.len(), "count": count, "evicted": evicted }),
        )
        .await;
    notify(c, &t.template.id, count);
    Ok(RepeatStateSetResult { template_id: t.template.id, count, evicted })
}

/// `todo.state_delete`。没存着这个键不是错误（`deleted = false`，不审计、不通知）。
pub async fn delete(c: &Ctx, caller: &Caller, p: RepeatStateDeleteParams) -> RpcResult<RepeatStateDeleteResult> {
    let t = resolve(c, caller, &p.id, p.agent.unwrap_or(false), p.origin_session.as_deref(), true).await?;
    check_key("key", &p.key)?;
    let (deleted, count) = {
        let _w = c.store.state_lock().lock().await;
        let deleted = c.store.todo_state_delete(&t.template.id, &p.key).await.map_err(internal)?;
        (deleted, c.store.todo_state_count(&t.template.id).await.map_err(internal)?)
    };
    count_use(c, &t, true);
    if deleted {
        c.audit
            .record(
                caller,
                "todo.state_delete",
                Some(&t.template.id),
                json!({ "key_len": p.key.len(), "count": count }),
            )
            .await;
        notify(c, &t.template.id, count);
    }
    Ok(RepeatStateDeleteResult { template_id: t.template.id, deleted, count })
}

/// `todo.state_list`。
pub async fn list(c: &Ctx, caller: &Caller, p: RepeatStateListParams) -> RpcResult<RepeatStateListResult> {
    let t = resolve(c, caller, &p.id, p.agent.unwrap_or(false), p.origin_session.as_deref(), true).await?;
    let prefix = p.prefix.as_deref().filter(|s| !s.is_empty());
    let after = p.after.as_deref().filter(|s| !s.is_empty());
    let limit = p.limit.unwrap_or(LIST_DEFAULT_LIMIT).clamp(1, LIST_MAX_LIMIT) as usize;
    let (entries, matched, has_more) =
        c.store.todo_state_page(&t.template.id, prefix, after, limit).await.map_err(internal)?;
    let total =
        if prefix.is_some() { c.store.todo_state_count(&t.template.id).await.map_err(internal)? } else { matched };
    count_use(c, &t, false);
    Ok(RepeatStateListResult { template_id: t.template.id, entries, total, matched, has_more })
}

/// `todo.state_unseen`：按请求里的顺序、去重，返回没存着的键。
pub async fn unseen(c: &Ctx, caller: &Caller, p: RepeatStateUnseenParams) -> RpcResult<RepeatStateUnseenResult> {
    let t = resolve(c, caller, &p.id, p.agent.unwrap_or(false), p.origin_session.as_deref(), true).await?;
    if p.keys.len() > MAX_UNSEEN_KEYS {
        return Err(invalid("keys", "too_many", format!("at most {MAX_UNSEEN_KEYS} keys per call")));
    }
    let mut keys: Vec<String> = Vec::with_capacity(p.keys.len());
    for k in p.keys {
        check_key("keys", &k)?;
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let present = c.store.todo_state_present(&t.template.id, &keys).await.map_err(internal)?;
    count_use(c, &t, false);
    Ok(RepeatStateUnseenResult {
        template_id: t.template.id,
        unseen: keys.into_iter().filter(|k| !present.contains(k)).collect(),
    })
}

/// `todo.state_clear`：只由人调用；开关关着时也能清（关掉开关不删已有的条目，清掉它们靠这里），清掉了 `repeat` 的旧模板
/// 名下还有条目时也能清。
pub async fn clear(c: &Ctx, caller: &Caller, p: RepeatStateClearParams) -> RpcResult<RepeatStateClearResult> {
    crate::todos::check_id(&p.id)?;
    if caller.is_mcp() || p.agent == Some(true) {
        return Err(forbidden("agent", "only a person can clear a repeating to-do's state"));
    }
    // 以前是模板、后来清掉了 `repeat` 的项：它名下还有条目时也能清（否则这些条目要等删掉这一项才会消失）
    let template_id = match resolve(c, caller, &p.id, false, None, false).await {
        Ok(t) => t.template.id,
        Err(e) if e.data.as_ref().and_then(|d| d.reason.as_deref()) == Some("not_repeating") => {
            if c.store.todo_state_count(&p.id).await.map_err(internal)? == 0 {
                return Err(e);
            }
            p.id.clone()
        }
        Err(e) => return Err(e),
    };
    let removed = {
        let _w = c.store.state_lock().lock().await;
        c.store.todo_state_clear(&template_id).await.map_err(internal)?
    };
    c.audit.record(caller, "todo.state_clear", Some(&template_id), json!({ "removed": removed })).await;
    if removed > 0 {
        notify(c, &template_id, 0);
    }
    Ok(RepeatStateClearResult { template_id, removed })
}

/// `todo.repeat_preview`：模板复制出来的下一项交给 Twin 时会发出的第一条消息，分段。和真正派发同一个组装函数
/// （[`compose_segments`]）；新复制出来的项还没有之前的报告，Twin 的说明要到派发那一刻才有，所以这两段不在里面。
/// 给了的 `title` / `body` / `project` / `keep_state` / `state_hint_off` 代替存着的值（表单里还没保存的内容）；`id` 不在库里时，
/// 给了 `title` 就按一个还没保存的模板处理（条数为 0，用法说明用这个 id）。只读：不改任何东西、不审计。
pub async fn repeat_preview(
    c: &Ctx,
    caller: &Caller,
    p: TodoRepeatPreviewParams,
) -> RpcResult<TodoRepeatPreviewResult> {
    crate::todos::check_id(&p.id)?;
    if caller.is_mcp() || caller.is_twin() || caller.is_twin_device() {
        return Err(RpcError::new(ErrorCode::Forbidden, "an agent connection cannot preview a repeating to-do"));
    }
    // 表单里的字段：校验同 `todo.add`；空白的项目表示没有项目
    let title = p.title.as_deref().map(crate::todos::check_title).transpose()?;
    if let Some(b) = p.body.as_deref() {
        crate::todos::check_body(b)?;
    }
    let project = match p.project.as_deref().map(str::trim) {
        Some("") => Some(None),
        Some(s) => Some(Some(crate::todos::check_project(s)?)),
        None => None,
    };
    let stored = c.store.todo_get(&p.id).await.map_err(internal)?;
    // 库里有：存着的值打底（还不是周期项的项，预览的是加上重复规则之后复制出来的样子）。
    // 库里没有：给了标题就是一个还没保存的模板，条数为 0；没给标题无从预览
    let (base_title, base_body, base_project, rule, state_count) = match &stored {
        Some(t) => (
            t.title.clone(),
            t.body.clone(),
            t.project.clone(),
            t.repeat.clone(),
            c.store.todo_state_count(&t.id).await.map_err(internal)?,
        ),
        None if title.is_some() => (String::new(), String::new(), None, None, 0),
        None => return Err(RpcError::new(ErrorCode::NotFound, format!("todo {} not found", p.id))),
    };
    let title = title.unwrap_or(base_title);
    let body = p.body.unwrap_or(base_body);
    let project = project.unwrap_or(base_project);
    let keep = p.keep_state.unwrap_or(rule.as_ref().is_some_and(|r| r.keep_state));
    let hint_off = p.state_hint_off.unwrap_or(rule.as_ref().is_some_and(|r| r.state_hint_off));
    let hint = (keep && !hint_off).then_some(StateHint { template_id: &p.id, count: state_count });
    let segments = compose_segments(&title, &body, &[], hint, p.no_push.unwrap_or(true));
    let rules_text = crate::rules::injection_for(&c.store, &c.workspace, project.as_deref())
        .await
        .map_err(internal)?
        .map(|i| i.text)
        .unwrap_or_default();
    Ok(TodoRepeatPreviewResult {
        first_message: join_segments(&segments),
        segments,
        rules_text,
        state_usage_text: state_usage_text(&p.id, None),
        state_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked() {
        assert!(check_key("key", "https://example.com/mr/1").is_ok());
        assert!(check_key("key", &"k".repeat(MAX_KEY_BYTES)).is_ok());
        let reason = |k: &str| check_key("key", k).unwrap_err().data.unwrap().reason.unwrap();
        assert_eq!(reason(""), "empty");
        assert_eq!(reason(&"k".repeat(MAX_KEY_BYTES + 1)), "length");
        // 字节数，不是字符数
        assert_eq!(reason(&"é".repeat(MAX_KEY_BYTES / 2 + 1)), "length");
        for bad in ["a\nb", "a\tb", "a\u{1b}[31m", "a\u{2028}b"] {
            assert_eq!(reason(bad), "control", "{bad:?}");
        }
        assert!(check_value("").is_ok() && check_value(&"v".repeat(MAX_VALUE_BYTES)).is_ok());
        assert!(check_value("line\nbreaks are fine").is_ok());
        assert!(check_value(&"v".repeat(MAX_VALUE_BYTES + 1)).is_err());
    }
}
