//! Bot 的范围、运行和问题存储；只保存解析后的决定，不保存判断提示或原始回应。

use anycon_protocol::*;
use sqlx::Row;

use crate::gitops::{pr_state_parse, PrChecks, PrSnapshot};

use super::{Result, Store};

/// `done` 判断交回时 `reason` 的固定前缀。旧版本写的 `HandedBack` 事件没有 `done` 标记，统计只能靠这个前缀认出它们，
/// 所以写入方和统计查询共用这一个常量。
pub(crate) const DONE_HAND_BACK_PREFIX: &str = "The twin believes this is done: ";

/// 本机 Bot 的派发记录 `(todo_id, created_ms)`：`twin_sessions` 里请求方为本机的会话，加上 `todo_remotes` 里派到别的节点的
/// 记录（和每日派发次数 `twin_dispatches_since` 同一个来源）。人手动派发的、Bot 设备派来的都不在内。
const LOCAL_DISPATCHES_CTE: &str = "WITH local_dispatches(todo_id, created_ms) AS ( \
     SELECT todo_id, created_ms FROM twin_sessions WHERE requester='local' \
     UNION ALL SELECT todo_id, created_ms FROM todo_remotes)";

const CALL_USAGE_SQL: &str = "SELECT id, input_tokens, output_tokens, cost_usd FROM judge_calls";

impl Store {
    pub async fn twin_scope(&self, requester: &str) -> Result<TwinScope> {
        let row = sqlx::query("SELECT * FROM twin_scopes WHERE requester = ?")
            .bind(requester)
            .fetch_optional(self.pool())
            .await?;
        Ok(match row {
            Some(r) => TwinScope {
                paused: r.get("paused"),
                projects: serde_json::from_str(r.get("projects"))?,
                agent: r.get("agent"),
                model: r.get("model"),
                mode: r.get("mode"),
                max_sessions: r.get::<i64, _>("max_sessions") as u32,
                daily_dispatches: r.get::<i64, _>("daily_dispatches") as u32,
            },
            None => TwinScope {
                paused: false,
                projects: vec![],
                agent: None,
                model: None,
                mode: None,
                max_sessions: 2,
                daily_dispatches: 10,
            },
        })
    }

    pub async fn save_twin_scope(&self, requester: &str, s: &TwinScope) -> Result<()> {
        sqlx::query(
            "INSERT INTO twin_scopes VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(requester) DO UPDATE SET paused=excluded.paused, projects=excluded.projects, \
             agent=excluded.agent, model=excluded.model, mode=excluded.mode, \
             max_sessions=excluded.max_sessions, daily_dispatches=excluded.daily_dispatches",
        )
        .bind(requester)
        .bind(s.paused)
        .bind(serde_json::to_string(&s.projects)?)
        .bind(&s.agent)
        .bind(&s.model)
        .bind(&s.mode)
        .bind(s.max_sessions)
        .bind(s.daily_dispatches)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn save_twin_run(&self, r: &TwinRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO twin_runs VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET finished_ms=excluded.finished_ms, result=excluded.result, \
             items=excluded.items, judge_calls=excluded.judge_calls, \
             decisions=excluded.decisions, error=excluded.error",
        )
        .bind(&r.id)
        .bind(&r.requester)
        .bind(r.started_ms as i64)
        .bind(r.finished_ms.map(|v| v as i64))
        .bind(serde_json::to_string(&r.triggers)?)
        .bind(r.dry_run)
        .bind(serde_json::to_string(&r.result)?)
        .bind(serde_json::to_string(&r.items)?)
        .bind(serde_json::to_string(&r.judge_calls)?)
        .bind(serde_json::to_string(&r.decisions)?)
        .bind(r.error.as_ref().map(serde_json::to_string).transpose()?)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn twin_runs(&self, requester: &str, p: TwinRunsParams) -> Result<Vec<TwinRun>> {
        let rows = sqlx::query(
            "SELECT * FROM twin_runs WHERE requester=? AND (? IS NULL OR started_ms < ?) \
             ORDER BY started_ms DESC, id DESC LIMIT ?",
        )
        .bind(requester)
        .bind(p.before_ms.map(|v| v as i64))
        .bind(p.before_ms.map(|v| v as i64))
        .bind(p.limit.unwrap_or(20).clamp(1, 100))
        .fetch_all(self.pool())
        .await?;
        let mut runs = rows.iter().map(run_of).collect::<Result<Vec<_>>>()?;
        // 一次读出这一页所有运行的调用，在内存里按运行汇总
        let ids: Vec<&String> = runs.iter().flat_map(|r| r.judge_calls.iter()).collect();
        let by_id = if ids.is_empty() {
            Default::default()
        } else {
            let calls = sqlx::query(sqlx::AssertSqlSafe(format!(
                "{CALL_USAGE_SQL} WHERE id IN (SELECT value FROM json_each(?))"
            )))
            .bind(serde_json::to_string(&ids)?)
            .fetch_all(self.pool())
            .await?;
            call_usage_map(&calls)
        };
        for run in &mut runs {
            run.usage = usage_of(&by_id, &run.judge_calls);
        }
        Ok(runs)
    }

    /// 本请求方最近一次非试跑运行（按 `started_ms`）的摘要；可能还在运行。只读摘要要的列，不碰 `items` / `decisions`。
    pub async fn twin_last_run(&self, requester: &str) -> Result<Option<TwinRunBrief>> {
        let row = sqlx::query(
            "SELECT id, started_ms, finished_ms, triggers, result, error FROM twin_runs \
             WHERE requester=? AND dry_run=0 ORDER BY started_ms DESC, id DESC LIMIT 1",
        )
        .bind(requester)
        .fetch_optional(self.pool())
        .await?;
        row.map(|r| {
            Ok(TwinRunBrief {
                id: r.get("id"),
                started_ms: r.get::<i64, _>("started_ms") as u64,
                finished_ms: r.get::<Option<i64>, _>("finished_ms").map(|v| v as u64),
                triggers: serde_json::from_str(r.get("triggers"))?,
                result: serde_json::from_str(r.get("result"))?,
                error: r.get::<Option<&str>, _>("error").map(serde_json::from_str).transpose()?,
            })
        })
        .transpose()
    }

    /// 一组判断调用的用量之和：`judge_calls` 按调用存了每次的用量，运行只记调用的 id，所以读的时候现算（和调用一起按同一个
    /// 截止时间清理）。某个成员没有任何调用报告过则缺省；三个都缺省时整体为 `None`。重复的 id 只算一次。
    pub async fn judge_usage(&self, call_ids: &[String]) -> Result<Option<JudgeUsage>> {
        if call_ids.is_empty() {
            return Ok(None);
        }
        let calls =
            sqlx::query(sqlx::AssertSqlSafe(format!("{CALL_USAGE_SQL} WHERE id IN (SELECT value FROM json_each(?))")))
                .bind(serde_json::to_string(call_ids)?)
                .fetch_all(self.pool())
                .await?;
        Ok(usage_of(&call_usage_map(&calls), call_ids))
    }

    /// `twin.stats` 用的原始数据：`since` 起开始的运行（含试跑，带 `usage`）。
    pub async fn twin_runs_since(&self, requester: &str, since: u64) -> Result<Vec<TwinRun>> {
        let rows = sqlx::query("SELECT * FROM twin_runs WHERE requester=? AND started_ms>=? ORDER BY started_ms, id")
            .bind(requester)
            .bind(since as i64)
            .fetch_all(self.pool())
            .await?;
        let mut runs = rows.iter().map(run_of).collect::<Result<Vec<_>>>()?;
        // 一次读出所有可能相关的调用（调用不早于它所属的运行），在内存里按运行汇总
        let calls = sqlx::query(sqlx::AssertSqlSafe(format!("{CALL_USAGE_SQL} WHERE started_ms>=?")))
            .bind(since as i64)
            .fetch_all(self.pool())
            .await?;
        let by_id = call_usage_map(&calls);
        for run in &mut runs {
            run.usage = usage_of(&by_id, &run.judge_calls);
        }
        Ok(runs)
    }

    /// `since` 起「经过」里的 `handed_back` / `taken_over` 事件：`(时间, kind)`。
    /// - `handed_back` 只算没做完就交回的：`done` 判断写的那一条不算（`done` 标记，或旧版本写的、没有标记但 `reason` 带
    ///   [`DONE_HAND_BACK_PREFIX`] 的）。交回只发生在 `handler = twin` 的项上，那些项不会是 Bot 设备记的，所以不另查派发。
    /// - `taken_over` 只算本机 Bot 在事件时间或之前派发过的项（Bot 设备派来的会话里的接手也写这个事件，不是本机 Bot 的）。
    pub async fn todo_event_times_since(&self, since: u64) -> Result<Vec<(u64, String)>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{LOCAL_DISPATCHES_CTE} SELECT e.ts_ms, e.kind FROM todo_events e WHERE e.ts_ms>=? AND ( \
               (e.kind='handed_back' AND COALESCE(json_extract(e.data,'$.done'),0) = 0 \
                  AND substr(COALESCE(json_extract(e.data,'$.reason'),''),1,length(?)) != ?) \
               OR (e.kind='taken_over' AND EXISTS ( \
                  SELECT 1 FROM local_dispatches d WHERE d.todo_id=e.todo_id AND d.created_ms<=e.ts_ms)))"
        )))
        .bind(since as i64)
        .bind(DONE_HAND_BACK_PREFIX)
        .bind(DONE_HAND_BACK_PREFIX)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(t, k)| (t as u64, k)).collect())
    }

    /// `since` 起把本机 Bot 派发过的项关闭的 `state` 事件：`(事件时间, 是否完成, 最近一次派发时间)`。
    /// 只算从未关闭变为 `done` / `dropped` 的事件（`from` 已经关闭的不算），且本机 Bot 在事件时间或之前派发过；
    /// 派发时间取不晚于事件的最近一次，重新打开再关闭时从最近那次派发算起。
    /// 「本机 Bot 派发过」= `twin_sessions` 里有本机请求方的会话，或 `todo_remotes` 里有派到别的节点的记录。
    pub async fn twin_closed_since(&self, since: u64) -> Result<Vec<(u64, bool, u64)>> {
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{LOCAL_DISPATCHES_CTE} SELECT ts_ms, to_state, last_ms FROM ( \
               SELECT e.ts_ms AS ts_ms, json_extract(e.data,'$.to') AS to_state, \
                      (SELECT MAX(d.created_ms) FROM local_dispatches d \
                        WHERE d.todo_id=e.todo_id AND d.created_ms<=e.ts_ms) AS last_ms \
               FROM todo_events e \
               WHERE e.kind='state' AND e.ts_ms>=? AND json_extract(e.data,'$.to') IN ('done','dropped') \
                 AND COALESCE(json_extract(e.data,'$.from'),'') NOT IN ('done','dropped')) \
             WHERE last_ms IS NOT NULL"
        )))
        .bind(since as i64)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(t, to, last)| (t as u64, to == "done", last as u64)).collect())
    }

    pub async fn twin_questions(&self, requester: &str, open_only: bool) -> Result<Vec<TwinQuestion>> {
        let rows = sqlx::query(
            "SELECT * FROM twin_questions WHERE requester=? AND (?=0 OR status='open') ORDER BY created_ms, id",
        )
        .bind(requester)
        .bind(open_only)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(TwinQuestion {
                    id: r.get("id"),
                    todo_id: r.get("todo_id"),
                    question: r.get("question"),
                    options: serde_json::from_str(r.get("options"))?,
                    created_ms: r.get::<i64, _>("created_ms") as u64,
                    answered_ms: r.get::<Option<i64>, _>("answered_ms").map(|v| v as u64),
                    answer: r.get("answer"),
                    answered_by: r.get::<Option<&str>, _>("answered_by").map(serde_json::from_str).transpose()?,
                    status: serde_json::from_value(serde_json::Value::String(r.get("status")))?,
                })
            })
            .collect()
    }

    pub async fn save_twin_question(&self, requester: &str, q: &TwinQuestion) -> Result<()> {
        let status = serde_json::to_value(q.status)?.as_str().unwrap().to_string();
        sqlx::query(
            "INSERT INTO twin_questions VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, answered_ms=excluded.answered_ms, \
             answer=excluded.answer, answered_by=excluded.answered_by",
        )
        .bind(&q.id)
        .bind(&q.todo_id)
        .bind(requester)
        .bind(&q.question)
        .bind(serde_json::to_string(&q.options)?)
        .bind(q.created_ms as i64)
        .bind(q.answered_ms.map(|v| v as i64))
        .bind(&q.answer)
        .bind(q.answered_by.as_ref().map(serde_json::to_string).transpose()?)
        .bind(status)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// 这个请求方从 `since` 起派发的次数。本机 Bot（`local`）派到别的节点的也算（§ 27.4）。
    pub async fn twin_dispatches_since(&self, requester: &str, since: u64) -> Result<u32> {
        let local =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM twin_sessions WHERE requester=? AND created_ms>=?")
                .bind(requester)
                .bind(since as i64)
                .fetch_one(self.pool())
                .await? as u32;
        Ok(if requester == "local" { local.saturating_add(self.todo_remotes_since(since).await?) } else { local })
    }

    /// 这个请求方派出的会话：`(会话 id, 项 id)`。
    pub async fn twin_sessions_of(&self, requester: &str) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT session_id, todo_id FROM twin_sessions WHERE requester=? ORDER BY created_ms")
            .bind(requester)
            .fetch_all(self.pool())
            .await?)
    }

    /// Bot 设备派出的会话：`(设备 id, 会话 id)`。启动时读进推送过滤的归属索引。
    pub async fn twin_device_sessions(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT requester, session_id FROM twin_sessions WHERE requester != 'local'")
            .fetch_all(self.pool())
            .await?)
    }

    /// Bot 设备建的项：`(设备 id, 项 id)`。
    pub async fn twin_device_todos(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT source_device, id FROM todos WHERE source_kind = 'twin' AND source_device IS NOT NULL",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// 一项的来源：`(source_kind, source_device)`；没有这一项为 `None`。
    pub async fn todo_source(&self, id: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(sqlx::query_as("SELECT source_kind, source_device FROM todos WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await?)
    }

    pub async fn twin_session_owner(&self, sid: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT requester FROM twin_sessions WHERE session_id=?")
            .bind(sid)
            .fetch_optional(self.pool())
            .await?)
    }

    /// 来自 Bot 设备的项不进本机 Bot 的范围：来源是 Bot 的项，B 自己的 Bot 不碰（docs/spec/daemon.md § 27.3）。
    pub async fn twin_todos(&self) -> Result<Vec<Todo>> {
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM todos WHERE handler='twin' AND unreviewed=0 AND repeat_kind IS NULL \
             AND source_kind != 'twin' AND state NOT IN ('done','dropped') ORDER BY updated_ms, id",
        )
        .fetch_all(self.pool())
        .await?;
        let mut out = vec![];
        for id in ids {
            if let Some(t) = self.todo_get(&id).await? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// 要轮询 PR 的会话：Bot 派出的、所属的项还交给 Bot 处理且没关闭。返回 `(session_id, todo_id)`。
    pub async fn twin_pr_targets(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT ts.session_id, ts.todo_id FROM twin_sessions ts JOIN todos t ON t.id = ts.todo_id \
             WHERE t.handler = 'twin' AND t.unreviewed = 0 AND t.source_kind != 'twin' \
             AND t.state NOT IN ('done','dropped') \
             ORDER BY ts.created_ms, ts.session_id",
        )
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn twin_pr_get(&self, session_id: &str) -> Result<Option<PrSnapshot>> {
        let row = sqlx::query("SELECT * FROM twin_pr WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| snapshot_of(&r)))
    }

    /// 一项关联的会话里已有基线的 PR 状态，新的在前，最多 3 个。
    pub async fn twin_pr_of_todo(&self, todo_id: &str) -> Result<Vec<PrSnapshot>> {
        let rows = sqlx::query(
            "SELECT p.* FROM twin_pr p JOIN twin_sessions ts ON ts.session_id = p.session_id \
             WHERE ts.todo_id = ? ORDER BY ts.created_ms DESC, ts.session_id LIMIT 3",
        )
        .bind(todo_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(snapshot_of).collect())
    }

    pub async fn twin_pr_save(&self, session_id: &str, s: &PrSnapshot) -> Result<()> {
        sqlx::query(
            "INSERT INTO twin_pr VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET number=excluded.number, state=excluded.state, \
             checks=excluded.checks, reviews=excluded.reviews, last_review=excluded.last_review",
        )
        .bind(session_id)
        .bind(s.number as i64)
        .bind(crate::gitops::pr_state_str(s.state))
        .bind(s.checks.as_str())
        .bind(s.reviews as i64)
        .bind(&s.last_review)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn prune_twin(&self, before: u64) -> Result<()> {
        sqlx::query("DELETE FROM twin_runs WHERE finished_ms IS NOT NULL AND started_ms < ?")
            .bind(before as i64)
            .execute(self.pool())
            .await?;
        sqlx::query("DELETE FROM twin_questions WHERE status != 'open' AND COALESCE(answered_ms,created_ms) < ?")
            .bind(before as i64)
            .execute(self.pool())
            .await?;
        sqlx::query("DELETE FROM twin_sessions WHERE created_ms < ? AND session_id NOT IN (SELECT id FROM sessions)")
            .bind(before as i64)
            .execute(self.pool())
            .await?;
        sqlx::query("DELETE FROM twin_pr WHERE session_id NOT IN (SELECT id FROM sessions)")
            .execute(self.pool())
            .await?;
        sqlx::query("DELETE FROM twin_items WHERE todo_id NOT IN (SELECT id FROM todos)").execute(self.pool()).await?;
        Ok(())
    }

    pub async fn cancel_twin_runs(&self, now: u64) -> Result<()> {
        sqlx::query(
            "UPDATE twin_runs SET finished_ms=?, result='\"cancelled\"', error='{\"code\":\"cancelled\"}' \
             WHERE finished_ms IS NULL",
        )
        .bind(now as i64)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

fn run_of(r: &sqlx::sqlite::SqliteRow) -> Result<TwinRun> {
    Ok(TwinRun {
        id: r.get("id"),
        requester: r.get("requester"),
        started_ms: r.get::<i64, _>("started_ms") as u64,
        finished_ms: r.get::<Option<i64>, _>("finished_ms").map(|v| v as u64),
        triggers: serde_json::from_str(r.get("triggers"))?,
        dry_run: r.get("dry_run"),
        result: serde_json::from_str(r.get("result"))?,
        items: serde_json::from_str(r.get("items"))?,
        judge_calls: serde_json::from_str(r.get("judge_calls"))?,
        decisions: serde_json::from_str(r.get("decisions"))?,
        error: r.get::<Option<&str>, _>("error").map(serde_json::from_str).transpose()?,
        usage: None,
    })
}

/// 调用行 → `id` 到该次用量。
fn call_usage_map(rows: &[sqlx::sqlite::SqliteRow]) -> std::collections::HashMap<String, JudgeUsage> {
    rows.iter()
        .map(|r| {
            (
                r.get::<String, _>("id"),
                JudgeUsage {
                    input_tokens: r.get::<Option<i64>, _>("input_tokens").map(|v| v as u64),
                    output_tokens: r.get::<Option<i64>, _>("output_tokens").map(|v| v as u64),
                    cost_usd: r.get::<Option<f64>, _>("cost_usd"),
                },
            )
        })
        .collect()
}

/// 一次运行的调用 id 对应的用量之和；重复的 id 只算一次，库里没有的 id（已清理）跳过。
fn usage_of(by_id: &std::collections::HashMap<String, JudgeUsage>, ids: &[String]) -> Option<JudgeUsage> {
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    sum_usage(unique.into_iter().filter_map(|id| by_id.get(id)))
}

/// 把若干次调用的用量相加；某个成员没有任何调用报告过就缺省，全部缺省则为 `None`。
pub(crate) fn sum_usage<'a>(calls: impl Iterator<Item = &'a JudgeUsage>) -> Option<JudgeUsage> {
    let mut sum = JudgeUsage::default();
    for u in calls {
        sum.input_tokens = add_opt(sum.input_tokens, u.input_tokens);
        sum.output_tokens = add_opt(sum.output_tokens, u.output_tokens);
        sum.cost_usd = match (sum.cost_usd, u.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
    }
    (sum != JudgeUsage::default()).then_some(sum)
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

fn snapshot_of(r: &sqlx::sqlite::SqliteRow) -> PrSnapshot {
    PrSnapshot {
        number: r.get::<i64, _>("number") as u64,
        state: pr_state_parse(r.get("state")),
        checks: PrChecks::parse(r.get("checks")),
        reviews: r.get::<i64, _>("reviews") as u32,
        last_review: r.get("last_review"),
    }
}
