//! 存储：SQLite（WAL）。表见 `migrations/0001_init.sql` 和 docs/spec/daemon.md § 11。
//!
//! 只用运行时查询（不用 `query!` 宏），编译不依赖数据库。所有时间是 Unix 毫秒。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_agent_api::ResumeHandle;
use anycon_protocol::{
    AgentInfo, AuditEntry, DeviceKind, Event, SearchHit, SessionEvent, SessionSettings, SessionStatus, UsageTotals,
    WorktreeState,
};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
}

/// `sessions` 表的一行。
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub project: Option<String>,
    /// 创建时项目指向哪里（`linked:<canonical 目录>` / `folder:<名字>`），旧会话为空（第一次拉起时补上）。
    pub project_loc: Option<String>,
    pub agent: String,
    pub title: String,
    pub title_locked: bool,
    pub status: SessionStatus,
    pub archived: bool,
    pub settings: SessionSettings,
    pub agent_session_id: Option<String>,
    pub launch_args: Value,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub last_seq: u64,
    pub next_turn_id: u64,
    pub usage: UsageTotals,
    pub preview: Option<String>,
    /// 会话在 anycon 建的 worktree 里工作时，`worktrees` 表里那一行的 id（ADR-34）。
    pub worktree_id: Option<String>,
}

impl SessionRow {
    pub fn resume_handle(&self) -> Option<ResumeHandle> {
        self.agent_session_id
            .as_ref()
            .map(|id| ResumeHandle { agent_session_id: id.clone(), launch_args: self.launch_args.clone() })
    }
}

#[derive(Debug, Clone)]
pub struct PeerRow {
    pub device_id: String,
    pub pubkey: [u8; 32],
    pub name: String,
    pub kind: DeviceKind,
    pub paired_at_ms: u64,
    pub last_seen_ms: Option<u64>,
}

/// `enroll_password` 表的那一行（节点口令，ADR-36）。口令本身不存，只有盐和 Argon2id 的输出。
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollPasswordRow {
    pub salt: [u8; 16],
    pub prs: [u8; 32],
    pub set_at_ms: u64,
    /// 设置者：设备 id，本机 CLI 是 `"local"`。
    pub set_by: String,
    /// 连续失败次数（悲观计数，含正在进行的那次）。
    pub failures: u32,
    /// 退避结束的时间（毫秒）；0 = 不在退避中。
    pub next_allowed_at_ms: u64,
    pub locked: bool,
}

impl Drop for EnrollPasswordRow {
    /// 派生值用完就清零。
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.prs.zeroize();
    }
}

impl std::fmt::Debug for EnrollPasswordRow {
    /// 不打印盐和派生值，免得一不小心进了日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollPasswordRow")
            .field("set_at_ms", &self.set_at_ms)
            .field("set_by", &self.set_by)
            .field("failures", &self.failures)
            .field("next_allowed_at_ms", &self.next_allowed_at_ms)
            .field("locked", &self.locked)
            .finish_non_exhaustive()
    }
}

/// `blobs` 表的一行（附件，ADR-35）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRow {
    /// 32 位小写 hex。
    pub id: String,
    /// 第一次被消息引用时绑定的会话；为空 = 上传完成但还没被引用。
    pub session_id: Option<String>,
    /// 展示用文件名（不进路径）。
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 正式文件名（`<id>.<ext>`），相对 `<数据目录>/blobs/`。
    pub path: String,
    pub created_ms: u64,
}

/// `linked_projects` 表的一行（关联的本机已有目录，ADR-33）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedRow {
    pub name: String,
    /// canonicalize 之后的绝对路径。
    pub path: String,
    pub created_ms: u64,
}

/// `worktrees` 表的一行（anycon 建的 git worktree，ADR-34）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub id: String,
    /// 创建时项目指向哪里（写法同 `SessionRow.project_loc`）。
    pub project_loc: Option<String>,
    /// 项目名（展示用）。
    pub project: String,
    /// 相对项目目录的路径（`.anycon/worktrees/<name>`）。
    pub rel_path: String,
    pub branch: String,
    /// 规范化的 base：完整引用名（`refs/heads/main`、`refs/remotes/origin/main`、`refs/tags/v1.0`）或完整提交 SHA。
    pub base: String,
    /// 展示名（短引用名，或提交的前 12 位）：对外的 `SessionWorktree.base` / `WorktreeInfo.base`、合并提交信息都用它。
    pub base_display: String,
    pub base_sha: String,
    /// 所属会话；会话删除之后为空。
    pub session_id: Option<String>,
    pub state: WorktreeState,
    /// 创建完成时目录的 `(dev, ino)`；`creating` / `failed` 时为空。
    pub identity: Option<(u64, u64)>,
    /// 创建 / 重建完成时 worktree 根下 `.git` 指针文件的内容（项目目录的绝对路径换成了 `{project}`）；为空时不核对。
    pub git_pointer: Option<String>,
    pub created_ms: u64,
}

pub fn worktree_state_str(s: WorktreeState) -> &'static str {
    match s {
        WorktreeState::Creating => "creating",
        WorktreeState::Active => "active",
        WorktreeState::Kept => "kept",
        WorktreeState::Removed => "removed",
        WorktreeState::Missing => "missing",
        WorktreeState::Failed => "failed",
    }
}

pub fn parse_worktree_state(s: &str) -> WorktreeState {
    match s {
        "creating" => WorktreeState::Creating,
        "active" => WorktreeState::Active,
        "kept" => WorktreeState::Kept,
        "removed" => WorktreeState::Removed,
        "failed" => WorktreeState::Failed,
        _ => WorktreeState::Missing,
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub kind: String,
    pub target: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
}

pub fn status_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Queued => "queued",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::WaitingPermission => "waiting_permission",
        SessionStatus::Idle => "idle",
        SessionStatus::Ended => "ended",
        SessionStatus::Error => "error",
        SessionStatus::Interrupted => "interrupted",
    }
}

pub fn parse_status(s: &str) -> SessionStatus {
    match s {
        "queued" => SessionStatus::Queued,
        "starting" => SessionStatus::Starting,
        "running" => SessionStatus::Running,
        "waiting_permission" => SessionStatus::WaitingPermission,
        "idle" => SessionStatus::Idle,
        "ended" => SessionStatus::Ended,
        "error" => SessionStatus::Error,
        _ => SessionStatus::Interrupted,
    }
}

fn kind_str(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::Daemon => "daemon",
        DeviceKind::Web => "web",
        DeviceKind::Cli => "cli",
        DeviceKind::Sdk => "sdk",
    }
}

pub fn parse_kind(s: &str) -> DeviceKind {
    match s {
        "daemon" => DeviceKind::Daemon,
        "web" => DeviceKind::Web,
        "sdk" => DeviceKind::Sdk,
        _ => DeviceKind::Cli,
    }
}

/// 事件类型名 + 索引用的 ref_id。
pub fn event_index(ev: &Event) -> (String, Option<String>) {
    let kind = serde_json::to_value(ev)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
        .unwrap_or_else(|| "unknown".into());
    let ref_id = match ev {
        Event::UserMessage { message_id, .. } => Some(message_id.clone()),
        Event::Permission { request_id, .. } | Event::PermissionResolved { request_id, .. } => Some(request_id.clone()),
        _ => None,
    };
    (kind, ref_id)
}

impl Store {
    /// 打开（必要时创建）数据库并跑迁移。
    pub async fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(false)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool, path: path.to_path_buf() })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&self.pool).await;
        self.pool.close().await;
    }

    /// 数据库文件（含 WAL）占用的字节数。
    pub fn db_bytes(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            total += std::fs::metadata(PathBuf::from(p)).map(|m| m.len()).unwrap_or(0);
        }
        total
    }

    // ---------------- kv ----------------

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query("SELECT v FROM kv WHERE k = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.get::<String, _>(0)))
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO kv(k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------------- sessions ----------------

    pub async fn upsert_session(&self, s: &SessionRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, project, agent, title, title_locked, status, archived, settings, agent_session_id, launch_args, \
             created_ms, updated_ms, last_seq, next_turn_id, usage, preview, project_loc, worktree_id) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET project=excluded.project, project_loc=excluded.project_loc, title=excluded.title, title_locked=excluded.title_locked, \
             status=excluded.status, archived=excluded.archived, settings=excluded.settings, agent_session_id=excluded.agent_session_id, \
             launch_args=excluded.launch_args, updated_ms=excluded.updated_ms, last_seq=excluded.last_seq, \
             next_turn_id=excluded.next_turn_id, usage=excluded.usage, preview=excluded.preview, worktree_id=excluded.worktree_id",
        )
        .bind(&s.id)
        .bind(&s.project)
        .bind(&s.agent)
        .bind(&s.title)
        .bind(s.title_locked)
        .bind(status_str(s.status))
        .bind(s.archived)
        .bind(serde_json::to_string(&s.settings)?)
        .bind(&s.agent_session_id)
        .bind(serde_json::to_string(&s.launch_args)?)
        .bind(s.created_ms as i64)
        .bind(s.updated_ms as i64)
        .bind(s.last_seq as i64)
        .bind(s.next_turn_id as i64)
        .bind(serde_json::to_string(&s.usage)?)
        .bind(&s.preview)
        .bind(&s.project_loc)
        .bind(&s.worktree_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn row_to_session(r: &sqlx::sqlite::SqliteRow) -> Result<SessionRow> {
        let settings: String = r.try_get("settings")?;
        let launch: Option<String> = r.try_get("launch_args")?;
        let usage: String = r.try_get("usage")?;
        Ok(SessionRow {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            project_loc: r.try_get("project_loc")?,
            agent: r.try_get("agent")?,
            title: r.try_get("title")?,
            title_locked: r.try_get("title_locked")?,
            status: parse_status(&r.try_get::<String, _>("status")?),
            archived: r.try_get("archived")?,
            settings: serde_json::from_str(&settings).unwrap_or_default(),
            agent_session_id: r.try_get("agent_session_id")?,
            launch_args: launch.and_then(|l| serde_json::from_str(&l).ok()).unwrap_or(Value::Null),
            created_ms: r.try_get::<i64, _>("created_ms")? as u64,
            updated_ms: r.try_get::<i64, _>("updated_ms")? as u64,
            last_seq: r.try_get::<i64, _>("last_seq")? as u64,
            next_turn_id: r.try_get::<i64, _>("next_turn_id")? as u64,
            usage: serde_json::from_str(&usage).unwrap_or_default(),
            preview: r.try_get("preview")?,
            worktree_id: r.try_get("worktree_id")?,
        })
    }

    pub async fn load_sessions(&self) -> Result<Vec<SessionRow>> {
        let rows = sqlx::query("SELECT * FROM sessions ORDER BY created_ms").fetch_all(&self.pool).await?;
        rows.iter().map(Self::row_to_session).collect()
    }

    pub async fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::row_to_session).transpose()
    }

    /// 删除会话的全部数据：事件、turn、FTS、blob 行。返回 blob 文件路径，由调用方删文件。
    pub async fn delete_session(&self, id: &str) -> Result<Vec<String>> {
        let blob_paths: Vec<String> = sqlx::query("SELECT path FROM blobs WHERE session_id = ?")
            .bind(id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| r.get::<String, _>(0))
            .collect();
        let mut tx = self.pool.begin().await?;
        for sql in [
            "DELETE FROM events WHERE session_id = ?",
            "DELETE FROM turns WHERE session_id = ?",
            "DELETE FROM messages_fts WHERE session_id = ?",
            "DELETE FROM blobs WHERE session_id = ?",
            "DELETE FROM sessions WHERE id = ?",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(blob_paths)
    }

    /// 早于 `before_ms` 且不在跑的会话 id（`history_retention`）。
    pub async fn sessions_older_than(&self, before_ms: u64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM sessions WHERE updated_ms < ? AND status IN ('idle','ended','error','interrupted')",
        )
        .bind(before_ms as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    // ---------------- events ----------------

    pub async fn append_event(&self, ev: &SessionEvent) -> Result<()> {
        let (kind, ref_id) = event_index(&ev.event);
        sqlx::query("INSERT INTO events (session_id, seq, ts_ms, turn_id, kind, ref_id, event) VALUES (?,?,?,?,?,?,?)")
            .bind(&ev.session_id)
            .bind(ev.seq as i64)
            .bind(ev.ts_ms as i64)
            .bind(ev.turn_id.map(|t| t as i64))
            .bind(kind)
            .bind(ref_id)
            .bind(serde_json::to_string(&ev.event)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn row_to_event(r: &sqlx::sqlite::SqliteRow) -> Result<SessionEvent> {
        let ev: String = r.try_get("event")?;
        Ok(SessionEvent {
            session_id: r.try_get("session_id")?,
            seq: r.try_get::<i64, _>("seq")? as u64,
            ts_ms: r.try_get::<i64, _>("ts_ms")? as u64,
            turn_id: r.try_get::<Option<i64>, _>("turn_id")?.map(|t| t as u64),
            event: serde_json::from_str(&ev)?,
        })
    }

    /// seq > after_seq 的事件，升序，最多 `limit` 条。
    pub async fn events_after(&self, session_id: &str, after_seq: u64, limit: u32) -> Result<Vec<SessionEvent>> {
        let rows = sqlx::query("SELECT * FROM events WHERE session_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?")
            .bind(session_id)
            .bind(after_seq as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_event).collect()
    }

    /// seq < before_seq 的最近 `limit` 条，按升序返回。
    pub async fn events_before(&self, session_id: &str, before_seq: u64, limit: u32) -> Result<Vec<SessionEvent>> {
        let rows = sqlx::query("SELECT * FROM events WHERE session_id = ? AND seq < ? ORDER BY seq DESC LIMIT ?")
            .bind(session_id)
            .bind(before_seq as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        let mut evs = rows.iter().map(Self::row_to_event).collect::<Result<Vec<_>>>()?;
        evs.reverse();
        Ok(evs)
    }

    pub async fn max_seq(&self, session_id: &str) -> Result<u64> {
        let r = sqlx::query("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(r.get::<i64, _>(0) as u64)
    }

    pub async fn min_seq(&self, session_id: &str) -> Result<u64> {
        let r = sqlx::query("SELECT COALESCE(MIN(seq), 0) FROM events WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(r.get::<i64, _>(0) as u64)
    }

    /// 幂等：这条 message_id 对应的 UserMessage 事件 seq。
    pub async fn find_user_message(&self, session_id: &str, message_id: &str) -> Result<Option<u64>> {
        let r = sqlx::query("SELECT seq FROM events WHERE session_id = ? AND kind = 'user_message' AND ref_id = ?")
            .bind(session_id)
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.map(|r| r.get::<i64, _>(0) as u64))
    }

    /// 这个 `message_id` 对应的用户消息的原文和附件的 `blob_id`（按顺序）——幂等重发时核对内容和附件是否一致（ADR-35）。
    pub async fn user_message_first(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<(String, Vec<String>)>> {
        let r =
            sqlx::query("SELECT * FROM events WHERE session_id = ? AND kind = 'user_message' AND ref_id = ? LIMIT 1")
                .bind(session_id)
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match r.as_ref().map(Self::row_to_event).transpose()? {
            Some(SessionEvent { event: Event::UserMessage { content, attachments, .. }, .. }) => {
                Some((content, attachments.into_iter().map(|a| a.blob_id).collect()))
            }
            _ => None,
        })
    }

    /// 跨会话按 `message_id` 找第一条用户消息所在的会话（`session.create` 带 prompt 的幂等用）。
    pub async fn find_session_by_message(&self, message_id: &str) -> Result<Option<String>> {
        let r = sqlx::query("SELECT session_id FROM events WHERE ref_id = ? AND kind = 'user_message' LIMIT 1")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.map(|r| r.get::<String, _>(0)))
    }

    /// 这个 request_id 是否已有 `permission_resolved` 事件。
    pub async fn permission_resolved(&self, request_id: &str) -> Result<bool> {
        let r = sqlx::query("SELECT 1 FROM events WHERE ref_id = ? AND kind = 'permission_resolved' LIMIT 1")
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.is_some())
    }

    /// 存在 `permission` 事件、但没有对应 `permission_resolved` 的 request_id（重启恢复用）。
    pub async fn unresolved_permissions(&self, session_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT e.ref_id FROM events e WHERE e.session_id = ? AND e.kind = 'permission' AND NOT EXISTS \
             (SELECT 1 FROM events r WHERE r.session_id = e.session_id AND r.kind = 'permission_resolved' AND r.ref_id = e.ref_id)",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|r| r.try_get::<Option<String>, _>(0).ok().flatten()).collect())
    }

    // ---------------- turns ----------------

    pub async fn turn_start(&self, session_id: &str, turn_id: u64, message_id: &str, started_ms: u64) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO turns (session_id, turn_id, message_id, started_ms) VALUES (?,?,?,?)")
            .bind(session_id)
            .bind(turn_id as i64)
            .bind(message_id)
            .bind(started_ms as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn turn_end(
        &self,
        session_id: &str,
        turn_id: u64,
        ended_ms: u64,
        reason: &str,
        usage: Option<&Value>,
    ) -> Result<()> {
        sqlx::query("UPDATE turns SET ended_ms = ?, reason = ?, usage = ? WHERE session_id = ? AND turn_id = ?")
            .bind(ended_ms as i64)
            .bind(reason)
            .bind(usage.map(|u| u.to_string()))
            .bind(session_id)
            .bind(turn_id as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 未结束的 turn 全部标 error（重启恢复）。
    pub async fn turns_close_open(&self, ended_ms: u64) -> Result<()> {
        sqlx::query("UPDATE turns SET ended_ms = ?, reason = 'error' WHERE ended_ms IS NULL")
            .bind(ended_ms as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 某条消息所在的 turn：优先 `turns.message_id`，否则取 seq 在它之后的第一个 `turn_start`。
    pub async fn turn_of_message(&self, session_id: &str, message_id: &str) -> Result<Option<u64>> {
        if let Some(r) = sqlx::query("SELECT turn_id FROM turns WHERE session_id = ? AND message_id = ?")
            .bind(session_id)
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(Some(r.get::<i64, _>(0) as u64));
        }
        let Some(seq) = self.find_user_message(session_id, message_id).await? else { return Ok(None) };
        let r = sqlx::query(
            "SELECT turn_id FROM events WHERE session_id = ? AND kind = 'turn_start' AND seq > ? ORDER BY seq ASC LIMIT 1",
        )
        .bind(session_id)
        .bind(seq as i64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(r.and_then(|r| r.try_get::<Option<i64>, _>(0).ok().flatten()).map(|t| t as u64))
    }

    // ---------------- FTS ----------------

    pub async fn index_text(&self, session_id: &str, seq: u64, role: &str, ts_ms: u64, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        sqlx::query("INSERT INTO messages_fts (text, session_id, seq, role, ts_ms) VALUES (?,?,?,?,?)")
            .bind(text)
            .bind(session_id)
            .bind(seq as i64)
            .bind(role)
            .bind(ts_ms as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// FTS5 搜索。先用 `MATCH`（词前缀），没有命中再退回 `LIKE`（CJK 等没有空格分词的文本）。
    pub async fn search(&self, query: &str, project: Option<&str>, limit: u32) -> Result<Vec<SearchHit>> {
        let projects: Option<Vec<String>> = project.map(|p| vec![p.to_string()]);
        self.search_in(query, projects.as_deref(), limit).await
    }

    /// 全文搜索，可以限制在若干个项目名里（同一个项目在库里可能有几种写法：NFC / NFD、大小写，调用方把它们都给出来）。
    pub async fn search_in(&self, query: &str, projects: Option<&[String]>, limit: u32) -> Result<Vec<SearchHit>> {
        // 空列表 = 没有这样的项目 = 没有命中
        if projects.is_some_and(|p| p.is_empty()) {
            return Ok(vec![]);
        }
        let in_clause = |n: usize| format!(" AND s.project IN ({})", vec!["?"; n].join(","));
        let terms: Vec<String> = query.split_whitespace().map(|t| format!("\"{}\"*", t.replace('"', "\"\""))).collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let fts_query = terms.join(" ");
        let mut sql = String::from(
            "SELECT messages_fts.session_id AS sid, messages_fts.seq AS seq, messages_fts.ts_ms AS ts, s.title AS title, \
             snippet(messages_fts, 0, char(1), char(2), '…', 14) AS snip \
             FROM messages_fts JOIN sessions s ON s.id = messages_fts.session_id WHERE messages_fts MATCH ?",
        );
        if let Some(p) = projects {
            sql.push_str(&in_clause(p.len()));
        }
        sql.push_str(" ORDER BY rank LIMIT ?");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(fts_query);
        for p in projects.unwrap_or_default() {
            q = q.bind(p.clone());
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await;
        let mut hits: Vec<SearchHit> = match rows {
            Ok(rows) => rows.iter().map(row_to_hit).collect(),
            // 非法 FTS 语法等：当作没有命中，走 LIKE（记一条 warn，别悄悄吞）
            Err(e) => {
                tracing::warn!(error = %e, "full-text search failed; falling back to LIKE");
                vec![]
            }
        };
        if hits.is_empty() {
            let like = format!("%{}%", query.trim().replace('%', "\\%").replace('_', "\\_"));
            let mut sql = String::from(
                "SELECT messages_fts.session_id AS sid, messages_fts.seq AS seq, messages_fts.ts_ms AS ts, s.title AS title, \
                 messages_fts.text AS snip FROM messages_fts JOIN sessions s ON s.id = messages_fts.session_id \
                 WHERE messages_fts.text LIKE ? ESCAPE '\\'",
            );
            if let Some(p) = projects {
                sql.push_str(&in_clause(p.len()));
            }
            sql.push_str(" LIMIT ?");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(like);
            for p in projects.unwrap_or_default() {
                q = q.bind(p.clone());
            }
            let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
            let needle = query.trim().to_lowercase();
            hits = rows
                .iter()
                .map(|r| {
                    let text: String = r.get("snip");
                    let mut h = row_to_hit(r);
                    h.snippet = like_snippet(&text, &needle);
                    h
                })
                .collect();
        }
        Ok(hits)
    }

    /// 会话表里出现过的全部项目名（去重）。
    pub async fn distinct_projects(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT project FROM sessions WHERE project IS NOT NULL")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    // ---------------- peers ----------------

    pub async fn list_peers(&self) -> Result<Vec<PeerRow>> {
        let rows = sqlx::query("SELECT * FROM peers ORDER BY paired_at_ms").fetch_all(&self.pool).await?;
        let mut out = Vec::new();
        for r in rows {
            let key_hex: String = r.try_get("pubkey")?;
            let Ok(bytes) = hex::decode(&key_hex) else { continue };
            let Ok(pubkey) = <[u8; 32]>::try_from(bytes.as_slice()) else { continue };
            out.push(PeerRow {
                device_id: r.try_get("device_id")?,
                pubkey,
                name: r.try_get("name")?,
                kind: parse_kind(&r.try_get::<String, _>("kind")?),
                paired_at_ms: r.try_get::<i64, _>("paired_at_ms")? as u64,
                last_seen_ms: r.try_get::<Option<i64>, _>("last_seen_ms")?.map(|v| v as u64),
            });
        }
        Ok(out)
    }

    pub async fn upsert_peer(&self, p: &PeerRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO peers (device_id, pubkey, name, kind, paired_at_ms, last_seen_ms) VALUES (?,?,?,?,?,?) \
             ON CONFLICT(device_id) DO UPDATE SET pubkey=excluded.pubkey, name=excluded.name, kind=excluded.kind, \
             paired_at_ms=excluded.paired_at_ms, last_seen_ms=excluded.last_seen_ms",
        )
        .bind(&p.device_id)
        .bind(hex::encode(p.pubkey))
        .bind(&p.name)
        .bind(kind_str(p.kind))
        .bind(p.paired_at_ms as i64)
        .bind(p.last_seen_ms.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_peer(&self, device_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM peers WHERE device_id = ?").bind(device_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn touch_peer(&self, device_id: &str, ts_ms: u64) -> Result<()> {
        sqlx::query("UPDATE peers SET last_seen_ms = ? WHERE device_id = ?")
            .bind(ts_ms as i64)
            .bind(device_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------------- enroll_password（节点口令，ADR-36）----------------

    pub async fn enroll_password_get(&self) -> Result<Option<EnrollPasswordRow>> {
        let Some(r) = sqlx::query("SELECT * FROM enroll_password WHERE id = 1").fetch_optional(&self.pool).await?
        else {
            return Ok(None);
        };
        // 派生值读出来的 `Vec<u8>` 用完（Drop）清零：它是口令的函数，拿到就能离线比对猜测
        let salt: zeroize::Zeroizing<Vec<u8>> = zeroize::Zeroizing::new(r.try_get("salt")?);
        let prs: zeroize::Zeroizing<Vec<u8>> = zeroize::Zeroizing::new(r.try_get("prs")?);
        let (Ok(salt), Ok(prs)) = (<[u8; 16]>::try_from(salt.as_slice()), <[u8; 32]>::try_from(prs.as_slice())) else {
            // 行损坏（长度不对）：当作没设口令，免得拿一个残缺的值去握手
            tracing::error!("the enroll_password row is malformed; treating the node password as not set");
            return Ok(None);
        };
        Ok(Some(EnrollPasswordRow {
            salt,
            prs,
            set_at_ms: r.try_get::<i64, _>("set_at_ms")? as u64,
            set_by: r.try_get("set_by")?,
            failures: r.try_get::<i64, _>("failures")? as u32,
            next_allowed_at_ms: r.try_get::<i64, _>("next_allowed_at_ms")? as u64,
            locked: r.try_get::<i64, _>("locked")? != 0,
        }))
    }

    /// 设置 / 修改口令（整行替换，失败计数和锁定一并清零）。
    pub async fn enroll_password_set(&self, row: &EnrollPasswordRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO enroll_password (id, salt, prs, set_at_ms, set_by, failures, next_allowed_at_ms, locked) \
             VALUES (1,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET salt=excluded.salt, prs=excluded.prs, set_at_ms=excluded.set_at_ms, \
             set_by=excluded.set_by, failures=excluded.failures, next_allowed_at_ms=excluded.next_allowed_at_ms, \
             locked=excluded.locked",
        )
        .bind(row.salt.to_vec())
        .bind(row.prs.to_vec())
        .bind(row.set_at_ms as i64)
        .bind(&row.set_by)
        .bind(row.failures as i64)
        .bind(row.next_allowed_at_ms as i64)
        .bind(row.locked as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 清除口令（删行，计数一并没了）。返回之前有没有。
    pub async fn enroll_password_clear(&self) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM enroll_password WHERE id = 1").execute(&self.pool).await?.rows_affected() > 0)
    }

    /// 写失败计数（连续失败次数、退避结束时间、是否锁定）。没有口令行时什么都不做，返回 false。
    pub async fn enroll_password_record(&self, failures: u32, next_allowed_at_ms: u64, locked: bool) -> Result<bool> {
        Ok(sqlx::query("UPDATE enroll_password SET failures = ?, next_allowed_at_ms = ?, locked = ? WHERE id = 1")
            .bind(failures as i64)
            .bind(next_allowed_at_ms as i64)
            .bind(locked as i64)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    // ---------------- agent cache ----------------

    pub async fn load_agent_cache(&self) -> Result<Vec<(AgentInfo, Option<String>)>> {
        let rows = sqlx::query("SELECT info, diagnose FROM agent_cache").fetch_all(&self.pool).await?;
        let mut out = Vec::new();
        for r in rows {
            let info: String = r.try_get(0)?;
            if let Ok(info) = serde_json::from_str::<AgentInfo>(&info) {
                out.push((info, r.try_get::<Option<String>, _>(1)?));
            }
        }
        Ok(out)
    }

    pub async fn save_agent_cache(&self, info: &AgentInfo, diagnose: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_cache (agent_id, info, diagnose, probed_at_ms) VALUES (?,?,?,?) \
             ON CONFLICT(agent_id) DO UPDATE SET info=excluded.info, diagnose=excluded.diagnose, probed_at_ms=excluded.probed_at_ms",
        )
        .bind(&info.id)
        .bind(serde_json::to_string(info)?)
        .bind(diagnose)
        .bind(info.probed_at_ms as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---------------- audit ----------------

    pub async fn insert_audit(&self, e: &AuditEntry) -> Result<()> {
        sqlx::query("INSERT INTO audit (ts_ms, device_id, device_name, action, target, detail) VALUES (?,?,?,?,?,?)")
            .bind(e.ts_ms as i64)
            .bind(&e.device_id)
            .bind(&e.device_name)
            .bind(&e.action)
            .bind(&e.target)
            .bind(e.detail.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 新的在前。
    pub async fn query_audit(
        &self,
        since_ms: Option<u64>,
        device_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AuditEntry>> {
        let mut sql =
            String::from("SELECT ts_ms, device_id, device_name, action, target, detail FROM audit WHERE ts_ms >= ?");
        if device_id.is_some() {
            sql.push_str(" AND device_id = ?");
        }
        sql.push_str(" ORDER BY ts_ms DESC, id DESC LIMIT ?");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(since_ms.unwrap_or(0) as i64);
        if let Some(d) = device_id {
            q = q.bind(d.to_string());
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        let mut out = Vec::new();
        for r in rows {
            let detail: String = r.try_get("detail")?;
            out.push(AuditEntry {
                ts_ms: r.try_get::<i64, _>("ts_ms")? as u64,
                device_id: r.try_get("device_id")?,
                device_name: r.try_get("device_name")?,
                action: r.try_get("action")?,
                target: r.try_get("target")?,
                detail: serde_json::from_str(&detail).unwrap_or(Value::Null),
            });
        }
        Ok(out)
    }

    pub async fn prune_audit(&self, before_ms: u64) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM audit WHERE ts_ms < ?")
            .bind(before_ms as i64)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    // ---------------- blobs ----------------

    fn row_to_blob(r: &sqlx::sqlite::SqliteRow) -> Result<BlobRow> {
        Ok(BlobRow {
            id: r.try_get("id")?,
            session_id: r.try_get("session_id")?,
            name: r.try_get("name")?,
            mime: r.try_get("mime")?,
            size: r.try_get::<i64, _>("size")? as u64,
            sha256: r.try_get("sha256")?,
            width: r.try_get::<Option<i64>, _>("width")?.map(|v| v as u32),
            height: r.try_get::<Option<i64>, _>("height")?.map(|v| v as u32),
            path: r.try_get("path")?,
            created_ms: r.try_get::<i64, _>("created_ms")? as u64,
        })
    }

    pub async fn blob_insert(&self, b: &BlobRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO blobs (id, session_id, name, mime, size, sha256, width, height, path, created_ms) \
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&b.id)
        .bind(&b.session_id)
        .bind(&b.name)
        .bind(&b.mime)
        .bind(b.size as i64)
        .bind(&b.sha256)
        .bind(b.width.map(i64::from))
        .bind(b.height.map(i64::from))
        .bind(&b.path)
        .bind(b.created_ms as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn blob_get(&self, id: &str) -> Result<Option<BlobRow>> {
        let row = sqlx::query("SELECT * FROM blobs WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::row_to_blob).transpose()
    }

    /// 把一批 blob 绑定到会话（一个事务）：每个必须还没绑定、或已经绑在这个会话上。
    /// 某一个绑不上（被别的会话抢先、或已经被删）时整个事务回滚，返回 `Ok(Err(那个 blob 的 id))`。
    pub async fn blobs_bind(&self, session_id: &str, ids: &[String]) -> Result<std::result::Result<(), String>> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            let n =
                sqlx::query("UPDATE blobs SET session_id = ? WHERE id = ? AND (session_id IS NULL OR session_id = ?)")
                    .bind(session_id)
                    .bind(id)
                    .bind(session_id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            if n == 0 {
                tx.rollback().await?;
                return Ok(Err(id.clone()));
            }
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    /// 删一个**还没被引用**的 blob 行；返回是否删掉了（已被引用或不存在 = false）。
    pub async fn blob_delete_unreferenced(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM blobs WHERE id = ? AND session_id IS NULL")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// `before_ms` 之前完成、至今没被引用的 blob（id, 文件名）。
    pub async fn blobs_unreferenced_before(&self, before_ms: u64) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT id, path FROM blobs WHERE session_id IS NULL AND created_ms < ?")
            .bind(before_ms as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| (r.get::<String, _>(0), r.get::<String, _>(1))).collect())
    }

    /// 所有完成的 blob 的个数和总字节数（`anycon doctor` 的 blobs 一行）。
    pub async fn blobs_totals(&self) -> Result<(u64, u64)> {
        let r = sqlx::query("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM blobs").fetch_one(&self.pool).await?;
        Ok((r.get::<i64, _>(0) as u64, r.get::<i64, _>(1) as u64))
    }

    /// 完成但还没被消息引用的 blob 的总字节数（`put_begin` 的全节点配额）。
    pub async fn blobs_unreferenced_bytes(&self) -> Result<u64> {
        let r = sqlx::query("SELECT COALESCE(SUM(size), 0) FROM blobs WHERE session_id IS NULL")
            .fetch_one(&self.pool)
            .await?;
        Ok(r.get::<i64, _>(0) as u64)
    }

    // ---------------- linked_projects ----------------

    pub async fn list_linked(&self) -> Result<Vec<LinkedRow>> {
        let rows = sqlx::query("SELECT name, path, created_ms FROM linked_projects ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| LinkedRow {
                name: r.get("name"),
                path: r.get("path"),
                created_ms: r.get::<i64, _>("created_ms") as u64,
            })
            .collect())
    }

    /// 名字或路径已经存在时返回数据库错误（调用方先在内存里查过重名，这里是最后一道约束）。
    pub async fn insert_linked(&self, row: &LinkedRow) -> Result<()> {
        sqlx::query("INSERT INTO linked_projects (name, path, created_ms) VALUES (?, ?, ?)")
            .bind(&row.name)
            .bind(&row.path)
            .bind(row.created_ms as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 返回是否真的删了一行。
    pub async fn delete_linked(&self, name: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM linked_projects WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    pub async fn rename_linked(&self, from: &str, to: &str) -> Result<bool> {
        Ok(sqlx::query("UPDATE linked_projects SET name = ? WHERE name = ?")
            .bind(to)
            .bind(from)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    // ---------------- worktrees ----------------

    fn row_to_worktree(r: &sqlx::sqlite::SqliteRow) -> Result<WorktreeRow> {
        let dev: Option<i64> = r.try_get("dev")?;
        let ino: Option<i64> = r.try_get("ino")?;
        Ok(WorktreeRow {
            id: r.try_get("id")?,
            project_loc: r.try_get("project_loc")?,
            project: r.try_get("project")?,
            rel_path: r.try_get("rel_path")?,
            branch: r.try_get("branch")?,
            base_sha: r.try_get("base_sha")?,
            base_display: {
                let base: String = r.try_get("base")?;
                let display: Option<String> = r.try_get("base_display")?;
                display.filter(|d| !d.is_empty()).unwrap_or_else(|| crate::worktree::display_base(&base))
            },
            base: r.try_get("base")?,
            session_id: r.try_get("session_id")?,
            state: parse_worktree_state(&r.try_get::<String, _>("state")?),
            identity: dev.zip(ino).map(|(d, i)| (d as u64, i as u64)),
            git_pointer: r.try_get("git_pointer")?,
            created_ms: r.try_get::<i64, _>("created_ms")? as u64,
        })
    }

    const WORKTREE_UPSERT: &'static str = "INSERT INTO worktrees (id, project_loc, project, rel_path, branch, base, base_display, base_sha, session_id, state, dev, ino, git_pointer, created_ms) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT(id) DO UPDATE SET project_loc=excluded.project_loc, project=excluded.project, rel_path=excluded.rel_path, \
         branch=excluded.branch, base=excluded.base, base_display=excluded.base_display, base_sha=excluded.base_sha, session_id=excluded.session_id, \
         state=excluded.state, dev=excluded.dev, ino=excluded.ino, git_pointer=excluded.git_pointer";

    fn bind_worktree<'q>(
        q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
        w: &'q WorktreeRow,
    ) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
        q.bind(&w.id)
            .bind(&w.project_loc)
            .bind(&w.project)
            .bind(&w.rel_path)
            .bind(&w.branch)
            .bind(&w.base)
            .bind(&w.base_display)
            .bind(&w.base_sha)
            .bind(&w.session_id)
            .bind(worktree_state_str(w.state))
            .bind(w.identity.map(|(d, _)| d as i64))
            .bind(w.identity.map(|(_, i)| i as i64))
            .bind(&w.git_pointer)
            .bind(w.created_ms as i64)
    }

    pub async fn upsert_worktree(&self, w: &WorktreeRow) -> Result<()> {
        Self::bind_worktree(sqlx::query(Self::WORKTREE_UPSERT), w).execute(&self.pool).await?;
        Ok(())
    }

    /// 会话行和它的 worktree 行一起写（`session.create { workspace: worktree }`）：先写 worktree 行，再写会话行。
    /// 两张表之间没有外键；会话行写失败时 worktree 行也要撤掉，调用方负责（见 `Sessions::create`）。
    pub async fn insert_session_with_worktree(&self, s: &SessionRow, w: &WorktreeRow) -> Result<()> {
        self.upsert_worktree(w).await?;
        if let Err(e) = self.upsert_session(s).await {
            let _ = self.delete_worktree(&w.id).await;
            return Err(e);
        }
        Ok(())
    }

    pub async fn get_worktree(&self, id: &str) -> Result<Option<WorktreeRow>> {
        let row = sqlx::query("SELECT * FROM worktrees WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::row_to_worktree).transpose()
    }

    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeRow>> {
        let rows = sqlx::query("SELECT * FROM worktrees ORDER BY created_ms, id").fetch_all(&self.pool).await?;
        rows.iter().map(Self::row_to_worktree).collect()
    }

    pub async fn delete_worktree(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM worktrees WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() > 0)
    }

    // ---------------- jobs ----------------

    pub async fn upsert_job(&self, j: &JobRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (id, kind, target, status, error, started_ms, ended_ms) VALUES (?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, error=excluded.error, ended_ms=excluded.ended_ms",
        )
        .bind(&j.id)
        .bind(&j.kind)
        .bind(&j.target)
        .bind(&j.status)
        .bind(&j.error)
        .bind(j.started_ms as i64)
        .bind(j.ended_ms.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 重启后：还在 running 的任务标 failed。
    pub async fn jobs_fail_running(&self, now_ms: u64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE jobs SET status = 'failed', error = 'daemon restarted', ended_ms = ? WHERE status = 'running'",
        )
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn get_job(&self, id: &str) -> Result<Option<JobRow>> {
        let r = sqlx::query("SELECT * FROM jobs WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        Ok(r.map(|r| JobRow {
            id: r.get("id"),
            kind: r.get("kind"),
            target: r.get("target"),
            status: r.get("status"),
            error: r.get("error"),
            started_ms: r.get::<i64, _>("started_ms") as u64,
            ended_ms: r.get::<Option<i64>, _>("ended_ms").map(|v| v as u64),
        }))
    }
}

fn row_to_hit(r: &sqlx::sqlite::SqliteRow) -> SearchHit {
    let snip: String = r.get("snip");
    SearchHit {
        session_id: r.get("sid"),
        seq: r.get::<i64, _>("seq") as u64,
        title: r.get("title"),
        snippet: marked_snippet(&snip),
        ts_ms: r.get::<i64, _>("ts") as u64,
    }
}

/// snippet() 用 \x01 \x02 作高亮标记；先转义 HTML，再换成 `<mark>`。
fn marked_snippet(raw: &str) -> String {
    let esc = html_escape(raw);
    esc.replace('\u{1}', "<mark>").replace('\u{2}', "</mark>")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn like_snippet(text: &str, needle_lower: &str) -> String {
    let lower = text.to_lowercase();
    let chars: Vec<char> = text.chars().collect();
    let Some(byte_pos) = lower.find(needle_lower) else {
        return html_escape(&chars.iter().take(80).collect::<String>());
    };
    // lower 与 text 的字节偏移在 ASCII / 无大小写变形时一致；不一致时退化为开头摘要
    if lower.len() != text.len()
        || !text.is_char_boundary(byte_pos)
        || !text.is_char_boundary(byte_pos + needle_lower.len())
    {
        return html_escape(&chars.iter().take(80).collect::<String>());
    }
    let start_char = text[..byte_pos].chars().count();
    let n_chars = text[byte_pos..byte_pos + needle_lower.len()].chars().count();
    let from = start_char.saturating_sub(30);
    let to = (start_char + n_chars + 30).min(chars.len());
    let before: String = chars[from..start_char].iter().collect();
    let mid: String = chars[start_char..start_char + n_chars].iter().collect();
    let after: String = chars[start_char + n_chars..to].iter().collect();
    format!(
        "{}{}<mark>{}</mark>{}{}",
        if from > 0 { "…" } else { "" },
        html_escape(&before),
        html_escape(&mid),
        html_escape(&after),
        if to < chars.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::{PermissionDetail, PermissionKind, PermissionOption, PermissionOptionKind};

    async fn temp_store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("daemon.db")).await.unwrap();
        (d, s)
    }

    fn row(id: &str, project: Option<&str>) -> SessionRow {
        SessionRow {
            id: id.into(),
            project: project.map(Into::into),
            project_loc: project.map(|p| format!("folder:{p}")),
            agent: "fake".into(),
            title: format!("title of {id}"),
            title_locked: false,
            status: SessionStatus::Idle,
            archived: false,
            settings: SessionSettings { model: Some("m".into()), ..Default::default() },
            agent_session_id: Some("agent-1".into()),
            launch_args: serde_json::json!({"a": 1}),
            created_ms: 1,
            updated_ms: 2,
            last_seq: 0,
            next_turn_id: 1,
            usage: UsageTotals::default(),
            preview: None,
            worktree_id: None,
        }
    }

    fn ev(sid: &str, seq: u64, event: Event) -> SessionEvent {
        SessionEvent { session_id: sid.into(), seq, ts_ms: 100 + seq, turn_id: Some(1), event }
    }

    fn blob(id: &str, session: Option<&str>) -> BlobRow {
        BlobRow {
            id: id.into(),
            session_id: session.map(Into::into),
            name: format!("{id}.png"),
            mime: "image/png".into(),
            size: 10,
            sha256: "0".repeat(64),
            width: Some(3),
            height: None,
            path: format!("{id}.png"),
            created_ms: 5,
        }
    }

    #[tokio::test]
    async fn blobs_roundtrip_bind_in_one_transaction_and_collect_the_unreferenced() {
        let (_d, s) = temp_store().await;
        s.blob_insert(&blob("free1", None)).await.unwrap();
        s.blob_insert(&blob("free2", None)).await.unwrap();
        s.blob_insert(&blob("taken", Some("sess_a"))).await.unwrap();
        assert_eq!(s.blob_get("free1").await.unwrap(), Some(blob("free1", None)), "width/height/name survive");
        assert_eq!(s.blob_get("nope").await.unwrap(), None);
        // 绑定是一个事务：第二个绑不上（被别的会话占着），第一个也要回滚
        let r = s.blobs_bind("sess_b", &["free1".into(), "taken".into(), "free2".into()]).await.unwrap();
        assert_eq!(r, Err("taken".to_string()));
        assert_eq!(s.blob_get("free1").await.unwrap().unwrap().session_id, None, "rolled back");
        assert_eq!(s.blob_get("free2").await.unwrap().unwrap().session_id, None);
        // 已经绑在这个会话上的可以再绑；不存在的绑不上
        assert_eq!(s.blobs_bind("sess_a", &["taken".into(), "free1".into()]).await.unwrap(), Ok(()));
        assert_eq!(s.blob_get("free1").await.unwrap().unwrap().session_id.as_deref(), Some("sess_a"));
        assert_eq!(s.blobs_bind("sess_a", &["ghost".into()]).await.unwrap(), Err("ghost".to_string()));
        // 只回收没被引用的，而且只回收够老的
        assert_eq!(s.blobs_unreferenced_before(6).await.unwrap(), vec![("free2".to_string(), "free2.png".to_string())]);
        assert!(s.blobs_unreferenced_before(5).await.unwrap().is_empty(), "created_ms < before_ms is strict");
        assert!(!s.blob_delete_unreferenced("taken").await.unwrap(), "referenced blobs are never deleted this way");
        assert!(s.blob_delete_unreferenced("free2").await.unwrap());
        assert!(!s.blob_delete_unreferenced("free2").await.unwrap());
        // 删会话带走它的 blob 行，返回文件名
        let mut paths = s.delete_session("sess_a").await.unwrap();
        paths.sort();
        assert_eq!(paths, vec!["free1.png".to_string(), "taken.png".to_string()]);
        assert_eq!(s.blob_get("taken").await.unwrap(), None);
    }

    #[tokio::test]
    async fn blob_totals_count_everything_and_the_unreferenced_separately() {
        let (_d, s) = temp_store().await;
        assert_eq!(s.blobs_totals().await.unwrap(), (0, 0));
        assert_eq!(s.blobs_unreferenced_bytes().await.unwrap(), 0);
        s.blob_insert(&blob("free1", None)).await.unwrap();
        s.blob_insert(&blob("free2", None)).await.unwrap();
        s.blob_insert(&blob("taken", Some("sess_a"))).await.unwrap();
        // 每个 blob 10 字节
        assert_eq!(s.blobs_totals().await.unwrap(), (3, 30));
        assert_eq!(s.blobs_unreferenced_bytes().await.unwrap(), 20, "referenced blobs do not count against the quota");
        assert_eq!(s.blobs_bind("sess_b", &["free1".into()]).await.unwrap(), Ok(()));
        assert_eq!(s.blobs_unreferenced_bytes().await.unwrap(), 10);
    }

    #[tokio::test]
    async fn migrations_and_session_roundtrip() {
        let (_d, s) = temp_store().await;
        let mut r = row("s1", Some("proj"));
        s.upsert_session(&r).await.unwrap();
        r.title = "new".into();
        r.status = SessionStatus::Running;
        r.last_seq = 7;
        s.upsert_session(&r).await.unwrap();
        let back = s.get_session("s1").await.unwrap().unwrap();
        assert_eq!(back.title, "new");
        assert_eq!(back.status, SessionStatus::Running);
        assert_eq!(back.last_seq, 7);
        assert_eq!(back.settings.model.as_deref(), Some("m"));
        assert_eq!(back.project_loc.as_deref(), Some("folder:proj"));
        // 位置可以改（项目改名 / 升格 / 重新关联之后），清空也行
        r.project_loc = Some("linked:/x/y".into());
        s.upsert_session(&r).await.unwrap();
        assert_eq!(s.get_session("s1").await.unwrap().unwrap().project_loc.as_deref(), Some("linked:/x/y"));
        assert_eq!(back.resume_handle().unwrap().agent_session_id, "agent-1");
        assert_eq!(s.load_sessions().await.unwrap().len(), 1);
        assert!(s.get_session("nope").await.unwrap().is_none());
        // 重开：迁移幂等
        s.close().await;
        let s2 = Store::open(&_d.path().join("daemon.db")).await.unwrap();
        assert_eq!(s2.load_sessions().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn events_seq_paging_and_idempotency_index() {
        let (_d, s) = temp_store().await;
        s.upsert_session(&row("s1", None)).await.unwrap();
        for i in 1..=10u64 {
            let e = if i == 3 {
                Event::UserMessage {
                    message_id: "m1".into(),
                    content: "hi".into(),
                    by_device: "d".into(),
                    attachments: vec![],
                }
            } else {
                Event::TextDelta { text: format!("t{i}") }
            };
            s.append_event(&ev("s1", i, e)).await.unwrap();
        }
        assert_eq!(s.max_seq("s1").await.unwrap(), 10);
        assert_eq!(s.min_seq("s1").await.unwrap(), 1);
        let after = s.events_after("s1", 4, 100).await.unwrap();
        assert_eq!(after.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![5, 6, 7, 8, 9, 10]);
        let limited = s.events_after("s1", 0, 3).await.unwrap();
        assert_eq!(limited.len(), 3);
        let before = s.events_before("s1", 6, 3).await.unwrap();
        assert_eq!(before.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(s.find_user_message("s1", "m1").await.unwrap(), Some(3));
        assert_eq!(s.find_user_message("s1", "other").await.unwrap(), None);
        // 同一 message_id 的第二条 user_message 违反唯一索引
        let dup = Event::UserMessage {
            message_id: "m1".into(),
            content: "again".into(),
            by_device: "d".into(),
            attachments: vec![],
        };
        assert!(s.append_event(&ev("s1", 11, dup)).await.is_err());
        // (session, seq) 主键
        assert!(s.append_event(&ev("s1", 5, Event::TextDone {})).await.is_err());
    }

    #[tokio::test]
    async fn permission_tracking() {
        let (_d, s) = temp_store().await;
        s.upsert_session(&row("s1", None)).await.unwrap();
        let perm = |rid: &str| Event::Permission {
            request_id: rid.into(),
            kind: PermissionKind::Tool,
            title: "t".into(),
            detail: PermissionDetail::default(),
            options: vec![PermissionOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
                needs_message: false,
            }],
            expires_at_ms: None,
        };
        s.append_event(&ev("s1", 1, perm("r1"))).await.unwrap();
        s.append_event(&ev("s1", 2, perm("r2"))).await.unwrap();
        s.append_event(&ev(
            "s1",
            3,
            Event::PermissionResolved {
                request_id: "r1".into(),
                option_id: Some("allow".into()),
                by_device: "d".into(),
            },
        ))
        .await
        .unwrap();
        assert!(s.permission_resolved("r1").await.unwrap());
        assert!(!s.permission_resolved("r2").await.unwrap());
        assert_eq!(s.unresolved_permissions("s1").await.unwrap(), vec!["r2".to_string()]);
    }

    #[tokio::test]
    async fn fts_search_prefix_cjk_and_delete() {
        let (_d, s) = temp_store().await;
        s.upsert_session(&row("s1", Some("proj"))).await.unwrap();
        s.upsert_session(&row("s2", None)).await.unwrap();
        s.index_text("s1", 1, "user", 10, "please refactor the <b>parser</b> module").await.unwrap();
        s.index_text("s1", 5, "assistant", 20, "Refactoring done, parser is cleaner now").await.unwrap();
        s.index_text("s2", 1, "user", 30, "完全不同的话题：数据库迁移").await.unwrap();
        let hits = s.search("refactor", None, 10).await.unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.session_id == "s1"));
        assert!(hits[0].snippet.contains("<mark>"));
        assert!(!hits[0].snippet.contains("<b>"), "html must be escaped: {}", hits[0].snippet);
        assert_eq!(s.search("parser", Some("proj"), 10).await.unwrap().len(), 2);
        assert_eq!(s.search("parser", Some("other"), 10).await.unwrap().len(), 0);
        let cjk = s.search("数据库", None, 10).await.unwrap();
        assert_eq!(cjk.len(), 1);
        assert_eq!(cjk[0].session_id, "s2");
        assert!(cjk[0].snippet.contains("<mark>"));
        // 词中间的子串：FTS 没有命中，走 LIKE 兜底
        let mid = s.search("据库", None, 10).await.unwrap();
        assert_eq!(mid.len(), 1);
        assert!(mid[0].snippet.contains("<mark>据库</mark>"), "{}", mid[0].snippet);
        assert_eq!(s.search("\"", None, 10).await.unwrap().len(), 0);
        // 删除会话连带清 FTS 和事件
        s.append_event(&ev("s1", 1, Event::TextDone {})).await.unwrap();
        s.delete_session("s1").await.unwrap();
        assert!(s.search("parser", None, 10).await.unwrap().is_empty());
        assert!(s.events_after("s1", 0, 10).await.unwrap().is_empty());
        assert!(s.get_session("s1").await.unwrap().is_none());
    }

    fn wt_row(id: &str, session: Option<&str>) -> WorktreeRow {
        WorktreeRow {
            id: id.into(),
            project_loc: Some("folder:proj#1:2".into()),
            project: "proj".into(),
            rel_path: format!(".anycon/worktrees/{id}"),
            branch: format!("anycon/{id}"),
            base: "refs/remotes/origin/main".into(),
            base_display: "origin/main".into(),
            base_sha: "abc123".into(),
            session_id: session.map(String::from),
            state: WorktreeState::Creating,
            identity: None,
            git_pointer: None,
            created_ms: 7,
        }
    }

    #[tokio::test]
    async fn worktrees_are_stored_with_the_session_and_keep_their_identity() {
        let (_d, s) = temp_store().await;
        assert!(s.list_worktrees().await.unwrap().is_empty());
        let mut sess = row("s1", Some("proj"));
        let w = wt_row("wt_a", Some("s1"));
        sess.worktree_id = Some(w.id.clone());
        s.insert_session_with_worktree(&sess, &w).await.unwrap();
        assert_eq!(s.get_session("s1").await.unwrap().unwrap().worktree_id.as_deref(), Some("wt_a"));
        assert_eq!(s.get_worktree("wt_a").await.unwrap().unwrap(), w);
        // 建好之后记下 (dev, ino)，状态改了；大的 inode 号也原样回来
        let mut done = w.clone();
        done.state = WorktreeState::Active;
        done.identity = Some((16777230, u64::MAX - 5));
        done.git_pointer = Some("gitdir: {project}/.git/worktrees/wt_a".into());
        s.upsert_worktree(&done).await.unwrap();
        assert_eq!(s.get_worktree("wt_a").await.unwrap().unwrap(), done);
        // 会话删了之后行可以留着（session_id 置空）
        done.session_id = None;
        done.state = WorktreeState::Kept;
        s.upsert_worktree(&done).await.unwrap();
        s.upsert_worktree(&wt_row("wt_b", None)).await.unwrap();
        let all = s.list_worktrees().await.unwrap();
        assert_eq!(all.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), vec!["wt_a", "wt_b"]);
        assert_eq!(all[0].state, WorktreeState::Kept);
        assert!(s.delete_worktree("wt_a").await.unwrap());
        assert!(!s.delete_worktree("wt_a").await.unwrap());
        assert!(s.get_worktree("wt_a").await.unwrap().is_none());
        // 每个状态都能往返
        for st in [
            WorktreeState::Creating,
            WorktreeState::Active,
            WorktreeState::Kept,
            WorktreeState::Removed,
            WorktreeState::Missing,
            WorktreeState::Failed,
        ] {
            assert_eq!(parse_worktree_state(worktree_state_str(st)), st);
        }
    }

    #[tokio::test]
    async fn linked_projects_table_enforces_unique_name_and_path() {
        let (_d, s) = temp_store().await;
        assert!(s.list_linked().await.unwrap().is_empty());
        let a = LinkedRow { name: "a".into(), path: "/x/a".into(), created_ms: 5 };
        s.insert_linked(&a).await.unwrap();
        s.insert_linked(&LinkedRow { name: "b".into(), path: "/x/b".into(), created_ms: 6 }).await.unwrap();
        assert!(s
            .insert_linked(&LinkedRow { name: "a".into(), path: "/x/other".into(), created_ms: 7 })
            .await
            .is_err());
        assert!(s.insert_linked(&LinkedRow { name: "c".into(), path: "/x/a".into(), created_ms: 7 }).await.is_err());
        assert_eq!(s.list_linked().await.unwrap().len(), 2);
        assert!(s.rename_linked("a", "z").await.unwrap());
        assert!(!s.rename_linked("a", "y").await.unwrap());
        let names: Vec<String> = s.list_linked().await.unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["b".to_string(), "z".to_string()]);
        assert!(s.delete_linked("z").await.unwrap());
        assert!(!s.delete_linked("z").await.unwrap());
        assert_eq!(
            s.list_linked().await.unwrap(),
            vec![LinkedRow { name: "b".into(), path: "/x/b".into(), created_ms: 6 }]
        );
    }

    #[tokio::test]
    async fn turns_peers_audit_jobs_kv() {
        let (_d, s) = temp_store().await;
        s.upsert_session(&row("s1", None)).await.unwrap();
        s.append_event(&ev(
            "s1",
            1,
            Event::UserMessage {
                message_id: "m1".into(),
                content: "x".into(),
                by_device: "d".into(),
                attachments: vec![],
            },
        ))
        .await
        .unwrap();
        s.turn_start("s1", 1, "m1", 100).await.unwrap();
        assert_eq!(s.turn_of_message("s1", "m1").await.unwrap(), Some(1));
        s.turn_end("s1", 1, 200, "completed", Some(&serde_json::json!({"x": 1}))).await.unwrap();

        let p = PeerRow {
            device_id: "d1".into(),
            pubkey: [7u8; 32],
            name: "phone".into(),
            kind: DeviceKind::Web,
            paired_at_ms: 5,
            last_seen_ms: None,
        };
        s.upsert_peer(&p).await.unwrap();
        s.touch_peer("d1", 99).await.unwrap();
        let peers = s.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].pubkey, [7u8; 32]);
        assert_eq!(peers[0].last_seen_ms, Some(99));
        s.delete_peer("d1").await.unwrap();
        assert!(s.list_peers().await.unwrap().is_empty());

        for i in 0..5u64 {
            s.insert_audit(&AuditEntry {
                ts_ms: 100 + i,
                device_id: if i % 2 == 0 { "a".into() } else { "b".into() },
                device_name: "n".into(),
                action: "session.create".into(),
                target: Some("t".into()),
                detail: serde_json::json!({"i": i}),
            })
            .await
            .unwrap();
        }
        let all = s.query_audit(None, None, 100).await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].ts_ms, 104);
        assert_eq!(s.query_audit(Some(102), Some("a"), 100).await.unwrap().len(), 2);
        assert_eq!(s.query_audit(None, None, 2).await.unwrap().len(), 2);
        assert_eq!(s.prune_audit(103).await.unwrap(), 3);

        let j = JobRow {
            id: "j1".into(),
            kind: "clone".into(),
            target: Some("x".into()),
            status: "running".into(),
            error: None,
            started_ms: 1,
            ended_ms: None,
        };
        s.upsert_job(&j).await.unwrap();
        assert_eq!(s.jobs_fail_running(50).await.unwrap(), 1);
        assert_eq!(s.get_job("j1").await.unwrap().unwrap().status, "failed");

        assert_eq!(s.kv_get("schema").await.unwrap(), None);
        s.kv_set("schema", "1").await.unwrap();
        s.kv_set("schema", "2").await.unwrap();
        assert_eq!(s.kv_get("schema").await.unwrap().as_deref(), Some("2"));
        assert!(s.db_bytes() > 0);
    }
}
