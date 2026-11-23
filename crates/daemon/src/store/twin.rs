//! 分身的范围、运行和问题存储；只保存解析后的决定，不保存判断提示或原始回应。

use anycon_protocol::*;
use sqlx::Row;

use crate::gitops::{pr_state_parse, PrChecks, PrSnapshot};

use super::{Result, Store};

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
        rows.iter()
            .map(|r| {
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
                })
            })
            .collect()
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

    /// 这个请求方从 `since` 起派发的次数。本机分身（`local`）派到别的节点的也算（§ 27.4）。
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

    /// 分身设备派出的会话：`(设备 id, 会话 id)`。启动时读进推送过滤的归属索引。
    pub async fn twin_device_sessions(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT requester, session_id FROM twin_sessions WHERE requester != 'local'")
            .fetch_all(self.pool())
            .await?)
    }

    /// 分身设备建的项：`(设备 id, 项 id)`。
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

    /// 来自分身设备的项不进本机分身的范围：来源是分身的项，B 自己的分身不碰（docs/spec/daemon.md § 27.3）。
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

    /// 要轮询 PR 的会话：分身派出的、所属的项还交给分身处理且没关闭。返回 `(session_id, todo_id)`。
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

fn snapshot_of(r: &sqlx::sqlite::SqliteRow) -> PrSnapshot {
    PrSnapshot {
        number: r.get::<i64, _>("number") as u64,
        state: pr_state_parse(r.get("state")),
        checks: PrChecks::parse(r.get("checks")),
        reviews: r.get::<i64, _>("reviews") as u32,
        last_review: r.get("last_review"),
    }
}
