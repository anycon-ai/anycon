//! 分身连接的 A 这一侧的存储（docs/spec/daemon.md § 27.4）：分身连接的元数据（表 `twin_links`）和远程项（表 `todo_remotes`）。
//! 只存 id、数字和枚举，以及 B 的节点名和分组名；B 上项的内容只经「经过」的事件进库，不在这里。

use anycon_protocol::{TodoActivity, TodoRemote, TwinPeerStatus};
use sqlx::Row;

use super::{Result, Store};

/// 一条分身连接的元数据（`twin_links` 的一行）。
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub node_id: String,
    pub name: String,
    pub group: Option<String>,
    /// B 的静态公钥（hex），算指纹用。
    pub pubkey: String,
    pub paired_ms: u64,
    /// 上一次的 `twin.peer_status`。
    pub status: Option<TwinPeerStatus>,
}

/// 一个远程项和取回结果的进度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRow {
    pub todo_id: String,
    pub node_id: String,
    pub remote_todo_id: String,
    pub fetched_seq: u64,
    /// `None`：还没收到过 B 的 `activity`；`Some(None)`：B 说这一项没有 `activity`。
    pub activity: Option<Option<TodoActivity>>,
}

fn activity_text(a: Option<TodoActivity>) -> String {
    a.and_then(|a| serde_json::to_value(a).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "none".into())
}

fn activity_of(text: Option<String>) -> Option<Option<TodoActivity>> {
    let text = text?;
    Some(serde_json::from_value::<TodoActivity>(serde_json::Value::String(text)).ok())
}

fn link_of(r: &sqlx::sqlite::SqliteRow) -> LinkRow {
    LinkRow {
        node_id: r.get("node_id"),
        name: r.get("name"),
        group: r.get("grp"),
        pubkey: r.get("pubkey"),
        paired_ms: r.get::<i64, _>("paired_ms") as u64,
        status: r.get::<Option<String>, _>("status").and_then(|s| serde_json::from_str(&s).ok()),
    }
}

impl Store {
    pub async fn twin_links(&self) -> Result<Vec<LinkRow>> {
        let rows = sqlx::query("SELECT * FROM twin_links ORDER BY name, node_id").fetch_all(self.pool()).await?;
        Ok(rows.iter().map(link_of).collect())
    }

    /// 新建或整行替换（重新配对）；`status` 保留已有的。
    pub async fn twin_link_save(&self, l: &LinkRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO twin_links (node_id, name, grp, pubkey, paired_ms) VALUES (?,?,?,?,?) \
             ON CONFLICT(node_id) DO UPDATE SET name=excluded.name, grp=excluded.grp, pubkey=excluded.pubkey, \
             paired_ms=excluded.paired_ms",
        )
        .bind(&l.node_id)
        .bind(&l.name)
        .bind(&l.group)
        .bind(&l.pubkey)
        .bind(l.paired_ms as i64)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// 记下 B 最近一次的 `twin.peer_status`，名字和分组名跟着更新。
    pub async fn twin_link_status(&self, node_id: &str, name: &str, s: &TwinPeerStatus) -> Result<()> {
        sqlx::query("UPDATE twin_links SET name=?, grp=?, status=? WHERE node_id=?")
            .bind(name)
            .bind(&s.group)
            .bind(serde_json::to_string(s)?)
            .bind(node_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// 忘掉这条连接：元数据没了；远程项（和已经取回的「经过」）留着，A 上的历史不变。
    pub async fn twin_link_delete(&self, node_id: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM twin_links WHERE node_id=?")
            .bind(node_id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }

    /// 这一项的远程项，按派发的先后。
    pub async fn todo_remotes(&self, todo_id: &str) -> Result<Vec<TodoRemote>> {
        let rows = sqlx::query("SELECT node_id, remote_todo_id FROM todo_remotes WHERE todo_id=? ORDER BY rowid")
            .bind(todo_id)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.iter().map(|r| TodoRemote { node_id: r.get(0), todo_id: r.get(1) }).collect())
    }

    /// 一页清单项的远程项（一次查询）。
    pub async fn todo_remotes_of(
        &self,
        todo_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<TodoRemote>>> {
        let mut out: std::collections::HashMap<String, Vec<TodoRemote>> = Default::default();
        if todo_ids.is_empty() {
            return Ok(out);
        }
        let rows = sqlx::query(
            "SELECT todo_id, node_id, remote_todo_id FROM todo_remotes \
             WHERE todo_id IN (SELECT value FROM json_each(?)) ORDER BY todo_id, rowid",
        )
        .bind(serde_json::to_string(todo_ids)?)
        .fetch_all(self.pool())
        .await?;
        for r in rows {
            out.entry(r.get::<String, _>(0)).or_default().push(TodoRemote { node_id: r.get(1), todo_id: r.get(2) });
        }
        Ok(out)
    }

    /// 一个节点上的远程项（取回结果、收到 B 的通知时用）。
    pub async fn todo_remotes_on(&self, node_id: &str) -> Result<Vec<RemoteRow>> {
        let rows = sqlx::query("SELECT * FROM todo_remotes WHERE node_id=? ORDER BY rowid")
            .bind(node_id)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.iter().map(remote_of).collect())
    }

    /// B 上的这一项对应 A 上的哪些项（正常只有一个）。
    pub async fn todo_remote_by_remote(&self, node_id: &str, remote_todo_id: &str) -> Result<Vec<RemoteRow>> {
        let rows = sqlx::query("SELECT * FROM todo_remotes WHERE node_id=? AND remote_todo_id=?")
            .bind(node_id)
            .bind(remote_todo_id)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.iter().map(remote_of).collect())
    }

    /// 这一项的远程项和各自最近知道的 `activity`。
    pub async fn todo_remote_rows(&self, todo_id: &str) -> Result<Vec<RemoteRow>> {
        let rows = sqlx::query("SELECT * FROM todo_remotes WHERE todo_id=? ORDER BY rowid")
            .bind(todo_id)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.iter().map(remote_of).collect())
    }

    pub async fn todo_remote_set_activity(
        &self,
        todo_id: &str,
        node_id: &str,
        remote_todo_id: &str,
        activity: Option<TodoActivity>,
    ) -> Result<bool> {
        let text = activity_text(activity);
        Ok(sqlx::query(
            "UPDATE todo_remotes SET activity=? WHERE todo_id=? AND node_id=? AND remote_todo_id=? \
             AND activity IS NOT ?",
        )
        .bind(&text)
        .bind(todo_id)
        .bind(node_id)
        .bind(remote_todo_id)
        .bind(&text)
        .execute(self.pool())
        .await?
        .rows_affected()
            > 0)
    }

    /// 这一项「经过」里最近的几条远程结果（`remote_result`）：`(节点 id, B 上的会话 id, 文字)`，最新的在前（分拣的输入用）。
    pub async fn todo_remote_results(&self, todo_id: &str, limit: u32) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            "SELECT data FROM todo_events WHERE todo_id = ? AND kind = 'remote_result' ORDER BY seq DESC LIMIT ?",
        )
        .bind(todo_id)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| match serde_json::from_str::<anycon_protocol::TodoEventData>(&r.get::<String, _>(0)) {
                Ok(anycon_protocol::TodoEventData::RemoteResult { node_id, session_id, text }) => {
                    Some((node_id, session_id, text))
                }
                _ => None,
            })
            .collect())
    }

    /// B 上这个远程项最近一次派发出的会话（`remote_dispatched` 里的 `session_id`，B 上的会话 id）。
    pub async fn todo_remote_session(
        &self,
        todo_id: &str,
        node_id: &str,
        remote_todo_id: &str,
    ) -> Result<Option<String>> {
        let rows = sqlx::query(
            "SELECT data FROM todo_events WHERE todo_id = ? AND kind = 'remote_dispatched' ORDER BY seq DESC",
        )
        .bind(todo_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().find_map(|r| {
            match serde_json::from_str::<anycon_protocol::TodoEventData>(&r.get::<String, _>(0)) {
                Ok(anycon_protocol::TodoEventData::RemoteDispatched { node_id: n, todo_id: t, session_id })
                    if n == node_id && t == remote_todo_id =>
                {
                    Some(session_id)
                }
                _ => None,
            }
        }))
    }

    /// 本地日从 `since_ms` 起派到别的节点的次数（和本机会话一起算进分身每日派发的次数）。
    pub async fn todo_remotes_since(&self, since_ms: u64) -> Result<u32> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM todo_remotes WHERE created_ms>=?")
            .bind(since_ms as i64)
            .fetch_one(self.pool())
            .await? as u32)
    }
}

fn remote_of(r: &sqlx::sqlite::SqliteRow) -> RemoteRow {
    RemoteRow {
        todo_id: r.get("todo_id"),
        node_id: r.get("node_id"),
        remote_todo_id: r.get("remote_todo_id"),
        fetched_seq: r.get::<i64, _>("fetched_seq") as u64,
        activity: activity_of(r.get("activity")),
    }
}

/// 记下一次远程派发（只追加；同一个远程项重复记不出错）。调用方在记「经过」的事务里调它。
pub(crate) async fn remote_add_on(
    db: &mut sqlx::SqliteConnection,
    todo_id: &str,
    node_id: &str,
    remote_todo_id: &str,
    now_ms: u64,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO todo_remotes (todo_id, node_id, remote_todo_id, created_ms) VALUES (?,?,?,?)")
        .bind(todo_id)
        .bind(node_id)
        .bind(remote_todo_id)
        .bind(now_ms as i64)
        .execute(db)
        .await?;
    Ok(())
}

/// 取回结果的进度只增不减（调用方在记「经过」的事务里调它）。
pub(crate) async fn set_fetched_seq(
    db: &mut sqlx::SqliteConnection,
    todo_id: &str,
    node_id: &str,
    remote_todo_id: &str,
    seq: u64,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE todo_remotes SET fetched_seq=MAX(fetched_seq, ?) WHERE todo_id=? AND node_id=? AND remote_todo_id=?",
    )
    .bind(seq as i64)
    .bind(todo_id)
    .bind(node_id)
    .bind(remote_todo_id)
    .execute(db)
    .await?;
    Ok(())
}
