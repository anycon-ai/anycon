//! `anycon todo state`：周期项的跨次状态（docs/spec/cli.md § 3.8，docs/spec/daemon.md § 26.11）。
//!
//! `--todo` 是模板或它复制出来的某一项（接受唯一前缀），两者读写的是模板的同一份。
//!
//! 在 anycon 拉起的 agent 里运行时（环境里有 `ANYCON_AGENT`）：除 `clear` 以外都能用，请求带 `agent: true` 和
//! `origin_session`（读 `ANYCON_SESSION_ID`），daemon 据此核对会话、把读写次数记在这一轮上；`clear` 拒绝执行。
//! 这是护栏，不是安全边界（同 `anycon todo` 的其余子命令）。
//!
//! 键和值可能是 agent 写的：表格和 `get` 的输出经 [`visible`] / [`table`] 可见化，终端转义不会被解释；`--json` 给原文。
//! `unseen` 打印的键 daemon 已经校验过（不含控制字符和换行），原样一行一个，方便接管道。

use std::io::BufRead;

use anycon_daemon::env::{AGENT_MARKER_ENV, SESSION_ID_ENV};
use anycon_daemon::util::fmt_utc;
use anycon_protocol::*;
use serde_json::json;

use crate::cli::{TodoStateCmd, TodoStateTarget};
use crate::commands::{is_method_not_found, App};
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR};
use crate::output::{table, visible};
use crate::todo_cmds::{inside_agent, require_state_support, resolve_todo};

/// `todo.state_unseen` 一次给的键数（daemon 的上限）。
const UNSEEN_BATCH: usize = anycon_daemon::todo_state::MAX_UNSEEN_KEYS;

/// 在 agent 里：`agent: true` 和来源会话。
fn agent_marks() -> (Option<bool>, Option<String>) {
    if inside_agent() {
        (Some(true), std::env::var(SESSION_ID_ENV).ok().filter(|s| !s.is_empty()))
    } else {
        (None, None)
    }
}

/// 把 daemon 的拒绝原因换成人话。
fn friendly(e: CliError) -> CliError {
    let message = match (e.code.as_str(), e.reason.as_deref()) {
        ("invalid", Some("not_repeating")) => {
            "this to-do does not repeat, so it has no state; state belongs to a repeating to-do and its copies"
        }
        ("invalid", Some("template_gone")) => "the repeating to-do this item was copied from has been removed",
        ("invalid", Some("state_off")) => {
            "this repeating to-do does not keep state; turn it on with `anycon todo edit <id> --keep-state`"
        }
        ("forbidden", Some("origin_session")) => {
            "this session was not started from a copy of that repeating to-do, so it cannot use its state"
        }
        _ if is_method_not_found(&e) => {
            "this node's daemon is too old for that: it does not keep state for repeating to-dos; update anycon on that node"
        }
        _ => return e,
    };
    CliError::new(e.exit, e.code, message)
}

async fn target_id(conn: &Conn, t: &TodoStateTarget) -> Result<String> {
    Ok(match resolve_todo(conn, &t.todo, false).await? {
        Ok(todo) => todo.id,
        Err(id) => id,
    })
}

pub async fn state(app: &App, cmd: TodoStateCmd) -> Result<u8> {
    match cmd {
        TodoStateCmd::Unseen(t) => unseen(app, &t).await,
        TodoStateCmd::Set { target, key, value } => set(app, &target, key, value).await,
        TodoStateCmd::Get { target, key } => get(app, &target, key).await,
        TodoStateCmd::Ls { target, prefix } => ls(app, &target, prefix).await,
        TodoStateCmd::Rm { target, key } => rm(app, &target, key).await,
        TodoStateCmd::Clear(t) => clear(app, &t).await,
    }
}

/// stdin 一行一个键：去掉行尾的 `\r`，跳过空行。
fn read_keys(input: impl BufRead) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for line in input.lines() {
        let line = line?;
        let key = line.strip_suffix('\r').unwrap_or(&line);
        if !key.is_empty() {
            keys.push(key.to_string());
        }
    }
    Ok(keys)
}

async fn unseen(app: &App, t: &TodoStateTarget) -> Result<u8> {
    let keys = read_keys(std::io::stdin().lock())?;
    let conn = app.conn().await?;
    let id = target_id(&conn, t).await?;
    let (agent, origin_session) = agent_marks();
    let mut out: Vec<String> = Vec::new();
    let mut template_id = id.clone();
    // 分批问；跨批去重（每一批里 daemon 自己去重）
    for chunk in keys.chunks(UNSEEN_BATCH) {
        let r: RepeatStateUnseenResult = conn
            .call(
                methods::TODO_STATE_UNSEEN,
                RepeatStateUnseenParams {
                    id: id.clone(),
                    keys: chunk.to_vec(),
                    agent,
                    origin_session: origin_session.clone(),
                },
            )
            .await
            .map_err(friendly)?;
        template_id = r.template_id;
        for k in r.unseen {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    if keys.is_empty() {
        // 没有键也要确认这一项能用状态（错误照常报出来）
        let _: RepeatStateListResult = conn
            .call(
                methods::TODO_STATE_LIST,
                RepeatStateListParams {
                    id: id.clone(),
                    prefix: None,
                    after: None,
                    limit: Some(1),
                    agent,
                    origin_session: origin_session.clone(),
                },
            )
            .await
            .map_err(friendly)?;
    }
    if app.json() {
        app.out.json_line(&json!({ "template_id": template_id, "unseen": out }));
    } else {
        for k in out {
            println!("{k}");
        }
    }
    Ok(0)
}

async fn set(app: &App, t: &TodoStateTarget, key: String, value: String) -> Result<u8> {
    let conn = app.conn().await?;
    let id = target_id(&conn, t).await?;
    let (agent, origin_session) = agent_marks();
    let r: RepeatStateSetResult = conn
        .call(methods::TODO_STATE_SET, RepeatStateSetParams { id, key, value, agent, origin_session })
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
    } else if r.evicted > 0 {
        println!("stored; {} entries ({} written longest ago dropped to stay under the limit)", r.count, r.evicted);
    } else {
        println!("stored; {} entries", r.count);
    }
    Ok(0)
}

async fn get(app: &App, t: &TodoStateTarget, key: String) -> Result<u8> {
    let conn = app.conn().await?;
    let id = target_id(&conn, t).await?;
    let (agent, origin_session) = agent_marks();
    let r: RepeatStateGetResult = conn
        .call(methods::TODO_STATE_GET, RepeatStateGetParams { id, key, agent, origin_session })
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
        return Ok(if r.entry.is_some() { 0 } else { EXIT_ERROR });
    }
    match r.entry {
        Some(e) => {
            println!("{}", visible(&e.value));
            Ok(0)
        }
        None => Err(CliError::new(EXIT_ERROR, "not_found", "that key is not stored")),
    }
}

async fn ls(app: &App, t: &TodoStateTarget, prefix: Option<String>) -> Result<u8> {
    let conn = app.conn().await?;
    let id = target_id(&conn, t).await?;
    let (agent, origin_session) = agent_marks();
    let mut entries: Vec<RepeatStateEntry> = Vec::new();
    let mut after: Option<String> = None;
    let (template_id, total) = loop {
        let r: RepeatStateListResult = conn
            .call(
                methods::TODO_STATE_LIST,
                RepeatStateListParams {
                    id: id.clone(),
                    prefix: prefix.clone(),
                    after: after.clone(),
                    limit: Some(anycon_daemon::todo_state::LIST_MAX_LIMIT),
                    agent,
                    origin_session: origin_session.clone(),
                },
            )
            .await
            .map_err(friendly)?;
        let next = r.entries.last().map(|e| e.key.clone());
        entries.extend(r.entries);
        // 游标没有前进（不该发生）就停，不死循环
        if !r.has_more || next.is_none() || next == after {
            break (r.template_id, r.total);
        }
        after = next;
    };
    if app.json() {
        app.out.json_line(&json!({ "template_id": template_id, "total": total, "entries": entries }));
        return Ok(0);
    }
    if entries.is_empty() {
        println!("no entries");
        return Ok(0);
    }
    let mut rows = vec![["KEY", "WRITTEN", "VALUE"].iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for e in &entries {
        rows.push(vec![
            e.key.clone(),
            format!("{}Z", &fmt_utc(e.written_ms)[..16]),
            crate::output::sanitize_label(&e.value, 80, ""),
        ]);
    }
    print!("{}", table(&rows));
    Ok(0)
}

async fn rm(app: &App, t: &TodoStateTarget, key: String) -> Result<u8> {
    let conn = app.conn().await?;
    let id = target_id(&conn, t).await?;
    let (agent, origin_session) = agent_marks();
    let r: RepeatStateDeleteResult = conn
        .call(methods::TODO_STATE_DELETE, RepeatStateDeleteParams { id, key, agent, origin_session })
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
    } else if r.deleted {
        println!("removed; {} entries", r.count);
    } else {
        println!("nothing removed: that key was not stored");
    }
    Ok(0)
}

async fn clear(app: &App, t: &TodoStateTarget) -> Result<u8> {
    if inside_agent() {
        return Err(CliError::denied(
            "forbidden",
            format!(
                "`anycon todo state clear` is not available inside an anycon agent session ({AGENT_MARKER_ENV} is set): \
                 an agent can read and write single keys, but wiping the whole state is left to you. Run it from your \
                 own terminal."
            ),
        ));
    }
    let conn = app.conn().await?;
    require_state_support(&conn).await?;
    let id = target_id(&conn, t).await?;
    let r: RepeatStateClearResult =
        conn.call(methods::TODO_STATE_CLEAR, RepeatStateClearParams { id, agent: None }).await.map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
    } else {
        println!("cleared {} entries", r.removed);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::read_keys;

    #[test]
    fn keys_are_read_one_per_line_without_blank_lines_or_carriage_returns() {
        let keys = read_keys("a\r\n\nb\n  c  \n".as_bytes()).unwrap();
        assert_eq!(keys, vec!["a", "b", "  c  "]);
        assert!(read_keys("".as_bytes()).unwrap().is_empty());
    }
}
