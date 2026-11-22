//! 判断调用及提炼记录的存取。每次状态变化是一条 SQL；预算检查由 Judge 的节点级互斥保护。

use anycon_protocol::{ExtractDropped, ExtractRun, ExtractRunStatus, JudgeError, JudgeUsage};
use sqlx::Row;

use super::{Result, Store};

pub struct JudgeCallStart<'a> {
    pub position: &'a str,
    pub backend: &'a str,
    pub kind: &'a str,
    pub model: Option<&'a str>,
    pub started_ms: u64,
    pub input_bytes: u64,
}

pub struct JudgeCallFinish<'a> {
    pub finished_ms: u64,
    pub status: &'a str,
    pub output_bytes: u64,
    pub usage: JudgeUsage,
}

/// 提炼结束时一次写入的元数据；usage 从对应的 judge_calls 读取。
pub struct ExtractRunFinish {
    pub finished_ms: u64,
    pub status: ExtractRunStatus,
    pub added: Vec<String>,
    pub dropped: ExtractDropped,
    pub error: Option<JudgeError>,
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtractAttempts {
    pub failures_since_done: u32,
    pub last_attempt_ms: Option<u64>,
}

fn enum_str<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v).expect("枚举可序列化").as_str().expect("枚举使用字符串表示").into()
}

fn extract_run(r: sqlx::sqlite::SqliteRow) -> Result<ExtractRun> {
    let code: Option<String> = r.try_get("error_code")?;
    let usage = JudgeUsage {
        input_tokens: r.try_get::<Option<i64>, _>("input_tokens")?.map(|v| v as u64),
        output_tokens: r.try_get::<Option<i64>, _>("output_tokens")?.map(|v| v as u64),
        cost_usd: r.try_get("cost_usd")?,
    };
    Ok(ExtractRun {
        id: r.try_get("id")?,
        session_id: r.try_get("session_id")?,
        trigger: serde_json::from_value(r.try_get::<String, _>("trigger")?.into())?,
        device_id: r.try_get("device_id")?,
        status: serde_json::from_value(r.try_get::<String, _>("status")?.into())?,
        started_ms: r.try_get::<i64, _>("started_ms")? as u64,
        finished_ms: r.try_get::<Option<i64>, _>("finished_ms")?.map(|v| v as u64),
        points: r.try_get::<i64, _>("points")? as u32,
        from_seq: r.try_get::<i64, _>("from_seq")? as u64,
        to_seq: r.try_get::<i64, _>("to_seq")? as u64,
        added: serde_json::from_str(&r.try_get::<String, _>("added")?)?,
        dropped: serde_json::from_str(&r.try_get::<String, _>("dropped")?)?,
        backend: r.try_get("backend")?,
        error: code
            .map(|code| -> Result<JudgeError> {
                Ok(JudgeError {
                    code: serde_json::from_value(code.into())?,
                    status: r.try_get::<Option<i64>, _>("error_status")?.map(|v| v as u16),
                })
            })
            .transpose()?,
        usage: (usage != JudgeUsage::default()).then_some(usage),
    })
}

impl Store {
    pub async fn start_judge_call(&self, c: &JudgeCallStart<'_>) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO judge_calls (id, position, backend, kind, model, started_ms, input_bytes) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(c.position)
        .bind(c.backend)
        .bind(c.kind)
        .bind(c.model)
        .bind(c.started_ms as i64)
        .bind(c.input_bytes as i64)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn finish_judge_call(&self, id: &str, c: &JudgeCallFinish<'_>) -> Result<()> {
        sqlx::query(
            "UPDATE judge_calls SET finished_ms = ?, status = ?, output_bytes = ?, input_tokens = ?, \
             output_tokens = ?, cost_usd = ? WHERE id = ? AND finished_ms IS NULL",
        )
        .bind(c.finished_ms as i64)
        .bind(c.status)
        .bind(c.output_bytes as i64)
        .bind(c.usage.input_tokens.map(|v| v as i64))
        .bind(c.usage.output_tokens.map(|v| v as i64))
        .bind(c.usage.cost_usd)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_judge_calls_since(&self, since_ms: u64) -> Result<u32> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_calls WHERE started_ms >= ?")
            .bind(since_ms as i64)
            .fetch_one(&self.pool)
            .await?;
        Ok(n.min(u32::MAX as i64) as u32)
    }

    pub async fn prune_judge_calls(&self, before_ms: u64) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM judge_calls WHERE started_ms < ? AND finished_ms IS NOT NULL")
            .bind(before_ms as i64)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    pub async fn cancel_unfinished_judge_calls(&self, now_ms: u64) -> Result<u64> {
        Ok(sqlx::query("UPDATE judge_calls SET finished_ms = ?, status = 'cancelled' WHERE finished_ms IS NULL")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// 插入时强制使用 running 和空结果；调用方提供范围、来源和唯一 id。
    pub async fn insert_extract_run(&self, r: &ExtractRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO rule_extract_runs (id, session_id, trigger, device_id, status, started_ms, \
             points, from_seq, to_seq, backend) VALUES (?, ?, ?, ?, 'running', ?, ?, ?, ?, ?)",
        )
        .bind(&r.id)
        .bind(&r.session_id)
        .bind(enum_str(r.trigger))
        .bind(&r.device_id)
        .bind(r.started_ms as i64)
        .bind(r.points as i64)
        .bind(r.from_seq as i64)
        .bind(r.to_seq as i64)
        .bind(&r.backend)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn finish_extract_run(&self, id: &str, r: &ExtractRunFinish) -> Result<()> {
        sqlx::query(
            "UPDATE rule_extract_runs SET status = ?, finished_ms = ?, added = ?, dropped = ?, \
             error_code = ?, error_status = ?, call_id = ? WHERE id = ? AND status = 'running'",
        )
        .bind(enum_str(r.status))
        .bind(r.finished_ms as i64)
        .bind(serde_json::to_string(&r.added)?)
        .bind(serde_json::to_string(&r.dropped)?)
        .bind(r.error.as_ref().map(|e| enum_str(e.code)))
        .bind(r.error.as_ref().and_then(|e| e.status).map(i64::from))
        .bind(&r.call_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_extract_run(&self, id: &str) -> Result<Option<ExtractRun>> {
        sqlx::query(
            "SELECT r.*, c.input_tokens, c.output_tokens, c.cost_usd FROM rule_extract_runs r LEFT \
             JOIN judge_calls c ON r.call_id = c.id WHERE r.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(extract_run)
        .transpose()
    }

    pub async fn list_extract_runs(&self, session_id: Option<&str>, limit: u32) -> Result<Vec<ExtractRun>> {
        sqlx::query(
            "SELECT r.*, c.input_tokens, c.output_tokens, c.cost_usd FROM rule_extract_runs r LEFT \
             JOIN judge_calls c ON r.call_id = c.id WHERE (? IS NULL OR r.session_id = ?) ORDER BY \
             r.started_ms DESC, r.rowid DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(session_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(extract_run)
        .collect()
    }

    pub async fn extract_watermark(&self, session_id: &str) -> Result<Option<u64>> {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT to_seq FROM rule_extract_runs WHERE session_id = ? AND status = 'done' ORDER BY \
             started_ms DESC, rowid DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(n.map(|n| n as u64))
    }

    /// 同毫秒的尝试按插入顺序比较，成功会清零之前的失败次数。
    pub async fn extract_attempts(&self, session_id: &str) -> Result<ExtractAttempts> {
        let r = sqlx::query(
            "SELECT MAX(started_ms) AS last_attempt, SUM(CASE WHEN status = 'failed' AND rowid > \
             COALESCE((SELECT rowid FROM rule_extract_runs WHERE session_id = ? AND status = 'done' \
             ORDER BY started_ms DESC, rowid DESC LIMIT 1), 0) THEN 1 ELSE 0 END) AS failures FROM \
             rule_extract_runs WHERE session_id = ?",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(ExtractAttempts {
            failures_since_done: r.try_get::<Option<i64>, _>("failures")?.unwrap_or(0) as u32,
            last_attempt_ms: r.try_get::<Option<i64>, _>("last_attempt")?.map(|v| v as u64),
        })
    }

    pub async fn cancel_running_extract_runs(&self, now_ms: u64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE rule_extract_runs SET status = 'failed', finished_ms = ?, error_code = \
             'cancelled', error_status = NULL WHERE status = 'running'",
        )
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn prune_extract_runs(&self, before_ms: u64) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM rule_extract_runs WHERE started_ms < ? AND status != 'running' AND rowid NOT \
             IN (SELECT rowid FROM rule_extract_runs AS latest WHERE latest.session_id = \
             rule_extract_runs.session_id AND latest.status = 'done' ORDER BY latest.started_ms DESC, \
             latest.rowid DESC LIMIT 1)",
        )
        .bind(before_ms as i64)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::{ExtractTrigger, JudgeErrorCode};

    fn run(id: &str, session: &str, time: u64) -> ExtractRun {
        ExtractRun {
            id: id.into(),
            session_id: session.into(),
            trigger: ExtractTrigger::Manual,
            device_id: Some("device".into()),
            status: ExtractRunStatus::Running,
            started_ms: time,
            finished_ms: None,
            points: 2,
            from_seq: 3,
            to_seq: 10,
            added: vec![],
            dropped: ExtractDropped::default(),
            backend: "api".into(),
            error: None,
            usage: None,
        }
    }

    fn finish(status: ExtractRunStatus) -> ExtractRunFinish {
        ExtractRunFinish {
            finished_ms: 500,
            status,
            added: vec!["rule".into()],
            dropped: ExtractDropped { invalid: 1, duplicate: 2, full: 3 },
            error: (status == ExtractRunStatus::Failed)
                .then_some(JudgeError { code: JudgeErrorCode::Http, status: Some(503) }),
            call_id: None,
        }
    }

    #[tokio::test]
    async fn extraction_watermark_failure_streak_listing_and_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).await.unwrap();
        assert_eq!(store.extract_watermark("s").await.unwrap(), None);
        assert_eq!(store.extract_attempts("s").await.unwrap(), ExtractAttempts::default());
        store.insert_extract_run(&run("a", "s", 100)).await.unwrap();
        assert_eq!(store.get_extract_run("a").await.unwrap().unwrap(), run("a", "s", 100));
        store.finish_extract_run("a", &finish(ExtractRunStatus::Failed)).await.unwrap();
        assert_eq!(store.extract_watermark("s").await.unwrap(), None);
        assert_eq!(store.extract_attempts("s").await.unwrap().failures_since_done, 1);
        store.insert_extract_run(&run("b", "s", 100)).await.unwrap();
        let id = store
            .start_judge_call(&JudgeCallStart {
                position: "extract",
                backend: "api",
                kind: "api",
                model: Some("m"),
                started_ms: 100,
                input_bytes: 30,
            })
            .await
            .unwrap();
        let usage = JudgeUsage { input_tokens: Some(4), output_tokens: Some(2), cost_usd: Some(0.001) };
        store
            .finish_judge_call(
                &id,
                &JudgeCallFinish { finished_ms: 120, status: "ok", output_bytes: 15, usage: usage.clone() },
            )
            .await
            .unwrap();
        let mut done = finish(ExtractRunStatus::Done);
        done.call_id = Some(id);
        store.finish_extract_run("b", &done).await.unwrap();
        assert_eq!(store.extract_watermark("s").await.unwrap(), Some(10));
        assert_eq!(store.extract_attempts("s").await.unwrap().failures_since_done, 0);
        let b = store.get_extract_run("b").await.unwrap().unwrap();
        assert_eq!(b.usage, Some(usage));
        assert_eq!(b.added, vec!["rule"]);
        assert_eq!(b.dropped.full, 3);
        for id in ["c", "d"] {
            store.insert_extract_run(&run(id, "s", 100)).await.unwrap();
            store.finish_extract_run(id, &finish(ExtractRunStatus::Failed)).await.unwrap();
        }
        store.insert_extract_run(&run("other", "other", 200)).await.unwrap();
        assert_eq!(
            store.extract_attempts("s").await.unwrap(),
            ExtractAttempts { failures_since_done: 2, last_attempt_ms: Some(100) }
        );
        assert_eq!(
            store.list_extract_runs(Some("s"), 2).await.unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["d", "c"]
        );
        assert_eq!(store.list_extract_runs(None, 1).await.unwrap()[0].id, "other");
        assert_eq!(store.list_extract_runs(None, 0).await.unwrap().len(), 0);
        assert!(store.get_extract_run("absent").await.unwrap().is_none());
        let failed = store.get_extract_run("d").await.unwrap().unwrap();
        assert_eq!(failed.error.unwrap(), JudgeError { code: JudgeErrorCode::Http, status: Some(503) });
        store.close().await;
    }

    #[tokio::test]
    async fn retention_preserves_each_sessions_latest_done_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let store = Store::open(&db).await.unwrap();
        for (id, session, time, to_seq, status) in [
            ("older", "a", 10, 10, ExtractRunStatus::Done),
            ("same-time-older", "a", 20, 20, ExtractRunStatus::Done),
            ("latest", "a", 20, 30, ExtractRunStatus::Done),
            ("failed", "a", 30, 40, ExtractRunStatus::Failed),
            ("running", "a", 40, 50, ExtractRunStatus::Running),
            ("other-latest", "b", 5, 60, ExtractRunStatus::Done),
            ("no-success", "c", 10, 70, ExtractRunStatus::Failed),
            ("boundary", "c", 100, 80, ExtractRunStatus::Failed),
        ] {
            let mut run = run(id, session, time);
            run.to_seq = to_seq;
            store.insert_extract_run(&run).await.unwrap();
            if status != ExtractRunStatus::Running {
                store.finish_extract_run(id, &finish(status)).await.unwrap();
            }
        }
        assert_eq!(store.extract_watermark("a").await.unwrap(), Some(30));
        assert_eq!(store.extract_watermark("b").await.unwrap(), Some(60));
        assert_eq!(store.prune_extract_runs(100).await.unwrap(), 4);
        for id in ["older", "same-time-older", "failed", "no-success"] {
            assert!(store.get_extract_run(id).await.unwrap().is_none());
        }
        for id in ["latest", "running", "other-latest", "boundary"] {
            assert!(store.get_extract_run(id).await.unwrap().is_some());
        }
        assert_eq!(store.prune_extract_runs(100).await.unwrap(), 0);
        store.close().await;
        let store = Store::open(&db).await.unwrap();
        assert_eq!(store.extract_watermark("a").await.unwrap(), Some(30));
        assert_eq!(store.extract_watermark("b").await.unwrap(), Some(60));
        assert_eq!(store.extract_watermark("c").await.unwrap(), None);
        store.finish_extract_run("running", &finish(ExtractRunStatus::Done)).await.unwrap();
        assert_eq!(store.prune_extract_runs(100).await.unwrap(), 1);
        assert!(store.get_extract_run("latest").await.unwrap().is_none());
        assert_eq!(store.extract_watermark("a").await.unwrap(), Some(50));
        assert_eq!(store.extract_watermark("b").await.unwrap(), Some(60));
        store.close().await;
    }

    #[tokio::test]
    async fn unfinished_rows_are_cancelled_and_retention_respects_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).await.unwrap();
        for (id, time) in [("old", 100), ("boundary", 200)] {
            store.insert_extract_run(&run(id, "s", time)).await.unwrap();
            store
                .start_judge_call(&JudgeCallStart {
                    position: "extract",
                    backend: "api",
                    kind: "api",
                    model: None,
                    started_ms: time,
                    input_bytes: 10,
                })
                .await
                .unwrap();
        }
        assert_eq!(store.count_judge_calls_since(0).await.unwrap(), 2);
        assert_eq!(store.count_judge_calls_since(200).await.unwrap(), 1);
        assert_eq!(store.prune_extract_runs(200).await.unwrap(), 0);
        assert_eq!(store.prune_judge_calls(200).await.unwrap(), 0);
        assert_eq!(store.cancel_unfinished_judge_calls(300).await.unwrap(), 2);
        assert_eq!(store.cancel_running_extract_runs(300).await.unwrap(), 2);
        assert_eq!(store.cancel_unfinished_judge_calls(400).await.unwrap(), 0);
        assert_eq!(store.cancel_running_extract_runs(400).await.unwrap(), 0);
        let r = store.get_extract_run("old").await.unwrap().unwrap();
        assert_eq!(r.status, ExtractRunStatus::Failed);
        assert_eq!(r.error.unwrap().code, JudgeErrorCode::Cancelled);
        assert_eq!(r.finished_ms, Some(300));
        assert_eq!(store.extract_watermark("s").await.unwrap(), None);
        assert_eq!(store.prune_extract_runs(200).await.unwrap(), 1);
        assert_eq!(store.prune_judge_calls(200).await.unwrap(), 1);
        assert_eq!(store.count_judge_calls_since(0).await.unwrap(), 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM judge_calls").fetch_one(store.pool()).await.unwrap();
        assert_eq!(status, "cancelled");
        store.close().await;
    }
}
