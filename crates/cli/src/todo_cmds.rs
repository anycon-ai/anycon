//! `anycon todo`：节点上的个人清单（docs/spec/cli.md § 3.8，docs/spec/daemon.md § 21）。
//!
//! 和别的客户端命令一样走 [`App::conn`]（本机 socket，或 `--hub` / `--node` 经 hub）。`<id>` 接受唯一前缀。
//!
//! 在 anycon 拉起的 agent 里运行时（环境里有 `ANYCON_AGENT`，和 `anycon mcp` 是同一个判断）：`add` 带 `agent: true` 和
//! `origin_session`（读 `ANYCON_SESSION_ID`），`ls` / `history` 照常，`done` / `drop` / `reopen` / `rm` / `run` 拒绝执行。
//! 这是和 `anycon mcp` 不给 agent 修改清单的工具同一条界线的**护栏，不是安全边界**：agent 能改自己的环境
//! （比如先清掉 `ANYCON_AGENT` 再运行），能执行 shell 的 agent 本来也能直接发 RPC。它防的是 agent 照常干活时顺手改掉人的清单。
//!
//! 清单项的标题和正文可能来自 agent，表格输出经 [`table`] 可见化；`--json` 输出原始内容。`history` 里的 `result` 文字是 agent 写的
//! （可能被注入），经 [`visible`] 之后缩进显示，终端转义不会被解释。

use anycon_daemon::env::{AGENT_MARKER_ENV, SESSION_ID_ENV};
use anycon_daemon::util::{fmt_utc, parse_utc};
use anycon_protocol::*;
use serde_json::{json, Value};

use crate::cli::{TodoAddArgs, TodoCmd, TodoHistoryArgs, TodoLsArgs, TodoRunArgs};
use crate::commands::App;
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR};
use crate::output::{sanitize_label, table, visible};

/// 这个进程是不是在 anycon 拉起的 agent 里。
fn inside_agent() -> bool {
    crate::mcp::started_inside_a_session()
}

/// 在 agent 里拒绝会改动清单的子命令。
fn refuse_inside_agent(what: &str) -> Result<()> {
    if inside_agent() {
        let message = if what == "run" {
            format!(
                "`anycon todo run` is not available inside an anycon agent session ({AGENT_MARKER_ENV} is set): handing \
                 a to-do to an agent is left to you. Run it from your own terminal."
            )
        } else {
            format!(
                "`anycon todo {what}` is not available inside an anycon agent session ({AGENT_MARKER_ENV} is set): \
                 an agent can add to the to-do list and read it, but closing, reopening and removing items is left to \
                 you. Run it from your own terminal."
            )
        };
        return Err(CliError::denied("forbidden", message));
    }
    Ok(())
}

/// `--due`：`YYYY-MM-DD`（当天 00:00 UTC），或 RFC 3339 时刻（`2026-12-01T09:00:00Z`、`2026-12-01T17:00+08:00`，秒可省，不带小数秒）。
pub fn parse_due(s: &str) -> Result<u64> {
    let bad = || {
        CliError::usage(format!(
            "--due: cannot read {s:?}; use a date like 2026-12-01 (midnight UTC) or an RFC 3339 time like \
             2026-12-01T09:00:00Z or 2026-12-01T17:00+08:00"
        ))
    };
    let t = s.trim();
    if let Some(ms) = parse_utc(t) {
        return Ok(ms);
    }
    // 带数字偏移：`…T17:00+08:00`
    let (date, clock) = t.split_once('T').ok_or_else(bad)?;
    let i = clock.rfind(['+', '-']).ok_or_else(bad)?;
    let (clock, off) = clock.split_at(i);
    let sign: i64 = if off.starts_with('+') { 1 } else { -1 };
    let (oh, om) = off[1..].split_once(':').ok_or_else(bad)?;
    let two =
        |x: &str| (x.len() == 2 && x.bytes().all(|b| b.is_ascii_digit())).then(|| x.parse::<i64>().ok()).flatten();
    let (oh, om) = (two(oh).ok_or_else(bad)?, two(om).ok_or_else(bad)?);
    if oh > 23 || om > 59 {
        return Err(bad());
    }
    let base = parse_utc(&format!("{date}T{clock}Z")).ok_or_else(bad)? as i64;
    u64::try_from(base - sign * (oh * 60 + om) * 60_000).map_err(|_| bad())
}

/// 一页页翻到 `has_more = false`，按 `id` 去重。
async fn list_all(conn: &Conn, include_closed: bool, project: Option<&str>) -> Result<Vec<Todo>> {
    let mut out: Vec<Todo> = Vec::new();
    let mut before: Option<TodoCursor> = None;
    loop {
        let r: TodoListResult = conn
            .call(
                methods::TODO_LIST,
                TodoListParams {
                    include_closed: Some(include_closed),
                    project: project.map(str::to_string),
                    limit: None,
                    before: before.clone(),
                },
            )
            .await?;
        let Some(last) = r.todos.last() else { break };
        let next = TodoCursor { updated_ms: last.updated_ms, id: last.id.clone() };
        for t in r.todos {
            if !out.iter().any(|o| o.id == t.id) {
                out.push(t);
            }
        }
        // 游标没有前进（不该发生）就停，不死循环
        if !r.has_more || before.as_ref() == Some(&next) {
            break;
        }
        before = Some(next);
    }
    Ok(out)
}

/// 在已经列出的项里按前缀找：唯一匹配才算，0 个返回 `None`，多个报错。
fn match_prefix<'a>(all: &'a [Todo], key: &str) -> Result<Option<&'a Todo>> {
    let m: Vec<&Todo> = all.iter().filter(|t| t.id.starts_with(key)).collect();
    match m.len() {
        0 => Ok(None),
        1 => Ok(Some(m[0])),
        n => Err(CliError::usage(format!("{key:?} matches {n} to-dos; use a longer prefix"))),
    }
}

/// `<id>`（唯一前缀，不区分大小写；id 本身是小写 UUID）。完整的 UUID 不用列清单，直接返回 `Err(id)` 让调用方拿它去调用。
/// 前缀先在没关闭的项里找，找不到再翻已关闭的；`closed_first`（`reopen`）反过来先找已关闭的（一次翻全部），所以一个前缀同时匹配一个没关闭的和一个已关闭的项时不算歧义。
/// 没有匹配 → 退出码 1（`not_found`），多个 → 2。
async fn resolve_todo(conn: &Conn, key: &str, closed_first: bool) -> Result<std::result::Result<Todo, String>> {
    let key = key.trim().to_lowercase();
    if key.is_empty() {
        return Err(CliError::usage("the to-do id must not be empty"));
    }
    if key.len() == 36 && uuid::Uuid::parse_str(&key).is_ok() {
        return Ok(Err(key));
    }
    let found = if closed_first {
        // reopen：只有已关闭的项能被重新打开，先在它们里找；没有匹配才看没关闭的（它会得到「没变」的回答）
        let all = list_all(conn, true, None).await?;
        let (closed, open): (Vec<Todo>, Vec<Todo>) = all.into_iter().partition(|t| t.state.is_closed());
        match match_prefix(&closed, &key)?.cloned() {
            Some(t) => Some(t),
            None => match_prefix(&open, &key)?.cloned(),
        }
    } else {
        // 先在没关闭的项里找，找不到再翻已关闭的
        let open = list_all(conn, false, None).await?;
        match match_prefix(&open, &key)?.cloned() {
            Some(t) => Some(t),
            None => {
                let all = list_all(conn, true, None).await?;
                match_prefix(&all, &key)?.cloned()
            }
        }
    };
    found.map(Ok).ok_or_else(|| CliError::new(EXIT_ERROR, "not_found", format!("no to-do matches {key:?}")))
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `too_many` 等细分原因换成人话。
fn friendly(e: CliError) -> CliError {
    match e.reason.as_deref() {
        Some("too_many") => {
            CliError::new(e.exit, e.code, "the to-do list is full (1000 open items); close or remove some first")
        }
        Some("too_many_unreviewed") => CliError::new(
            e.exit,
            e.code,
            "too many agent-added to-dos are waiting for review (100); a person has to review or remove some first",
        ),
        _ => e,
    }
}

pub async fn todo(app: &App, cmd: TodoCmd) -> Result<u8> {
    match cmd {
        TodoCmd::Add(a) => add(app, a).await,
        TodoCmd::Ls(a) => ls(app, a).await,
        TodoCmd::Done { id } => set_state(app, "done", &id, TodoState::Done, "done").await,
        TodoCmd::Drop { id } => set_state(app, "drop", &id, TodoState::Dropped, "dropped").await,
        TodoCmd::Reopen { id } => set_state(app, "reopen", &id, TodoState::Open, "reopened").await,
        TodoCmd::Rm { id } => rm(app, &id).await,
        TodoCmd::Run(a) => run(app, a).await,
        TodoCmd::History(a) => history(app, a).await,
    }
}

async fn add(app: &App, a: TodoAddArgs) -> Result<u8> {
    let due_ms = a.due.as_deref().map(parse_due).transpose()?;
    let nested = inside_agent();
    let params = TodoAddParams {
        // 客户端生成 id：`todo.add` 按它幂等
        id: uuid::Uuid::new_v4().to_string(),
        title: a.title,
        body: a.body,
        project: a.project,
        due_ms,
        agent: nested.then_some(true),
        origin_session: nested.then(|| std::env::var(SESSION_ID_ENV).ok().filter(|s| !s.is_empty())).flatten(),
    };
    let conn = app.conn().await?;
    let t: Todo = conn.call(methods::TODO_ADD, params).await.map_err(friendly)?;
    if app.json() {
        app.out.json_line(&t);
    } else {
        println!("added {}  {}", short(&t.id), sanitize_label(&t.title, 80, ""));
    }
    Ok(0)
}

fn activity_str(a: TodoActivity) -> &'static str {
    match a {
        TodoActivity::Working => "working",
        TodoActivity::Waiting => "waiting",
        TodoActivity::Review => "review",
        // 更新版本的 daemon 给的值：不显示
        TodoActivity::Unknown => "",
    }
}

fn state_str(s: TodoState) -> &'static str {
    s.as_str()
}

async fn ls(app: &App, a: TodoLsArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let todos = list_all(&conn, a.all, a.project.as_deref()).await?;
    if app.json() {
        app.out.json_line(&json!({ "todos": todos }));
        return Ok(0);
    }
    if todos.is_empty() {
        println!("no to-dos");
        return Ok(0);
    }
    let mut rows = vec![["ID", "STATE", "ACTIVITY", "DUE", "PROJECT", "BY", "TITLE"]
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()];
    for t in &todos {
        let by = match (t.source.kind, t.unreviewed) {
            (TodoSourceKind::Agent, true) => app.out.yellow("agent (unreviewed)"),
            (TodoSourceKind::Agent, false) => "agent".to_string(),
            (TodoSourceKind::Unknown, true) => app.out.yellow("other (unreviewed)"),
            (TodoSourceKind::Unknown, false) => "other".to_string(),
            _ => String::new(),
        };
        rows.push(vec![
            short(&t.id),
            state_str(t.state).to_string(),
            t.activity.map(activity_str).unwrap_or_default().to_string(),
            t.due_ms.map(|d| fmt_utc(d)[..16].to_string()).unwrap_or_default(),
            t.project.clone().unwrap_or_default(),
            by,
            sanitize_label(&t.title, 80, ""),
        ]);
    }
    print!("{}", table(&rows));
    Ok(0)
}

async fn set_state(app: &App, what: &str, key: &str, state: TodoState, done: &str) -> Result<u8> {
    refuse_inside_agent(what)?;
    let conn = app.conn().await?;
    let resolved = resolve_todo(&conn, key, state == TodoState::Open).await?;
    // 已经是这个状态：daemon 当空操作，这里也如实说没变
    let (id, known_state) = match &resolved {
        Ok(t) => (t.id.clone(), Some(t.state)),
        Err(id) => (id.clone(), None),
    };
    let unchanged_known = known_state == Some(state);
    let t: Todo = if unchanged_known {
        resolved.expect("known")
    } else {
        conn.call(methods::TODO_UPDATE, TodoUpdateParams { id, state: Some(state), ..empty_update() }).await?
    };
    if app.json() {
        app.out.json_line(&t);
    } else if known_state.is_none() {
        // 完整 UUID 没列过清单，不知道原来的状态：只说现在是什么状态，不猜它变没变
        println!("{} is {}  {}", short(&t.id), state_str(t.state), sanitize_label(&t.title, 80, ""));
    } else if unchanged_known {
        println!(
            "nothing changed: {} is already {}  {}",
            short(&t.id),
            state_str(t.state),
            sanitize_label(&t.title, 80, "")
        );
    } else {
        println!("{done} {}  {}", short(&t.id), sanitize_label(&t.title, 80, ""));
    }
    Ok(0)
}

fn empty_update() -> TodoUpdateParams {
    TodoUpdateParams {
        id: String::new(),
        title: None,
        body: None,
        project: None,
        due_ms: None,
        state: None,
        reviewed: None,
        clear: Vec::new(),
    }
}

async fn rm(app: &App, key: &str) -> Result<u8> {
    refuse_inside_agent("rm")?;
    let conn = app.conn().await?;
    let resolved = resolve_todo(&conn, key, false).await?;
    let id = match &resolved {
        Ok(t) => t.id.clone(),
        Err(id) => id.clone(),
    };
    let _: Value = conn.call_raw(methods::TODO_DELETE, TodoIdParams { id: id.clone() }).await?;
    if app.json() {
        app.out.json_line(&json!({ "id": id, "deleted": true }));
    } else {
        match resolved {
            Ok(t) => println!("removed {}  {}", short(&t.id), sanitize_label(&t.title, 80, "")),
            Err(_) => println!("removed {}", short(&id)),
        }
    }
    Ok(0)
}

/// 把 `todo.dispatch` 的几个错误换成人话。
fn friendly_dispatch(e: CliError, agent: &str) -> CliError {
    match (e.code.as_str(), e.reason.as_deref()) {
        ("unsupported", Some("no_push")) => CliError::new(
            e.exit,
            e.code,
            format!(
                "agent {agent} has no way to block pushes and pull requests; add --allow-push to dispatch anyway (the \
                 agent may then push and open pull requests)"
            ),
        ),
        ("invalid", Some("closed")) => CliError::new(
            e.exit,
            e.code,
            "this to-do is closed (done or dropped); reopen it with `anycon todo reopen` first",
        ),
        ("invalid", Some("too_many_sessions")) => CliError::new(
            e.exit,
            e.code,
            "this to-do has already been handed to an agent 20 times; add a new item for what is left",
        ),
        _ => e,
    }
}

async fn run(app: &App, a: TodoRunArgs) -> Result<u8> {
    refuse_inside_agent("run")?;
    let conn = app.conn().await?;
    let id = match resolve_todo(&conn, &a.id, false).await? {
        Ok(t) => t.id,
        Err(id) => id,
    };
    if a.dry_run {
        // 只读：问 daemon 此刻会发什么，不开会话、不记任何东西（不需要选 agent）
        let r: TodoDispatchPreviewResult = conn
            .call(
                methods::TODO_DISPATCH_PREVIEW,
                TodoDispatchPreviewParams { id, no_push: a.allow_push.then_some(false), project: a.project.clone() },
            )
            .await
            .map_err(|e| friendly_dispatch(e, ""))?;
        if app.json() {
            app.out.json_line(&r);
        } else {
            // 这是要发给 agent 的全文，不加装饰；之前的报告是 agent 写的，终端转义和控制字符显示成转义形式（普通文字原样），
            // 要拿到一字不差的原文用 `--json`
            let text = visible(&r.first_message);
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            // 规则不在消息里，在会话的系统提示里（`anycon rule`）；有生效的规则时单独列出来
            if !r.rules_text.is_empty() {
                println!(
                    "\n--- Rules for the system prompt (see `anycon rule ls`); agents that cannot take a system prompt do not get them ---"
                );
                println!("{}", visible(&r.rules_text));
            }
        }
        return Ok(0);
    }
    let agent = crate::commands::pick_agent(&conn, a.agent).await?;
    let allow_push = a.allow_push;
    let params = TodoDispatchParams {
        id,
        agent: agent.clone(),
        project: a.project,
        // 缺省由 daemon 定（项目能开 worktree 就开）；`--local` 明确在项目目录里
        workspace: a.local.then_some(SessionWorkspace::Local),
        settings: SessionSettings { model: a.model, mode: a.mode, ..Default::default() },
        no_push: a.allow_push.then_some(false),
        // 一次命令一个：回执丢了、重发时 daemon 返回第一次的结果，不开第二个会话
        message_id: Some(uuid::Uuid::new_v4().to_string()),
    };
    let r: TodoDispatchResult =
        conn.call(methods::TODO_DISPATCH, params).await.map_err(|e| friendly_dispatch(e, &agent))?;
    if app.json() {
        app.out.json_line(&r);
        return Ok(0);
    }
    let place = match &r.session.worktree {
        Some(w) => format!("in a new worktree on branch {}", sanitize_label(&w.branch, 120, "?")),
        None => "in the project directory (no worktree, no isolation)".to_string(),
    };
    println!(
        "dispatched {}  {}\nsession {} ({}) {place}",
        short(&r.todo.id),
        sanitize_label(&r.todo.title, 80, ""),
        r.session.id,
        sanitize_label(&r.session.agent, 40, "?"),
    );
    // 没有 `--allow-push` 时，daemon 只在 agent 声明了选项并打开它时才派发成功，所以这里不用再查
    println!(
        "{}",
        if allow_push {
            "the agent may push and open pull requests"
        } else {
            "the agent is not to push or open pull requests"
        }
    );
    println!(
        "follow it with `anycon attach {}`; see the item's history with `anycon todo history {}`",
        r.session.id,
        short(&r.todo.id)
    );
    Ok(0)
}

fn describe_event(e: &TodoEvent) -> (String, Vec<String>) {
    let by = |a: &TodoActor| match (&a.kind, &a.device_id) {
        (TodoSourceKind::Device, Some(d)) => format!("device {}", sanitize_label(d, 40, "?")),
        (TodoSourceKind::Local, _) => "this machine".to_string(),
        (TodoSourceKind::Agent, _) => "an agent".to_string(),
        (TodoSourceKind::Device, None) => "a device".to_string(),
        (TodoSourceKind::Unknown, _) => "someone".to_string(),
    };
    match &e.data {
        TodoEventData::Dispatched { session_id, agent, workspace_mode, no_push, by: who, rules, rules_skipped } => (
            format!(
                "dispatched to {} (session {}, {}{}) by {}",
                sanitize_label(agent, 40, "?"),
                session_id,
                match workspace_mode {
                    SessionWorkspaceMode::Worktree => "worktree",
                    SessionWorkspaceMode::Local => "project directory",
                },
                if *no_push { ", no push" } else { ", push allowed" },
                by(who)
            ),
            // 规则（`anycon rule`）：注入了几条，或者这个 agent 收不到
            match (rules.len(), rules_skipped) {
                (_, Some(RulesSkipped::Unsupported)) => {
                    vec!["rules: not given, this agent cannot take a system prompt".to_string()]
                }
                (0, _) => vec![],
                (n, _) => vec![format!("rules: {n} given in the system prompt")],
            },
        ),
        TodoEventData::Result { session_id, turn, text, truncated } => (
            format!("report from session {session_id}, turn {turn}{}", if *truncated { " (cut at 4 KiB)" } else { "" }),
            if text.is_empty() {
                vec!["(no text)".to_string()]
            } else {
                visible(text).lines().map(str::to_string).collect()
            },
        ),
        TodoEventData::State { from, to, by: who } => {
            (format!("{} -> {} by {}", from.as_str(), to.as_str(), by(who)), vec![])
        }
    }
}

async fn history(app: &App, a: TodoHistoryArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let id = match resolve_todo(&conn, &a.id, false).await? {
        Ok(t) => t.id,
        Err(id) => id,
    };
    let mut events: Vec<TodoEvent> = Vec::new();
    let mut before: Option<u64> = None;
    loop {
        let r: TodoHistoryResult = conn
            .call(methods::TODO_HISTORY, TodoHistoryParams { id: id.clone(), before_seq: before, limit: Some(200) })
            .await?;
        let Some(last) = r.events.last() else { break };
        let next = last.seq;
        events.extend(r.events);
        // 游标没有前进（不该发生）就停，不死循环
        if !r.has_more || before == Some(next) {
            break;
        }
        before = Some(next);
    }
    if app.json() {
        app.out.json_line(&json!({ "events": events }));
        return Ok(0);
    }
    if events.is_empty() {
        println!("nothing has happened on this item yet");
        return Ok(0);
    }
    for e in &events {
        let (head, body) = describe_event(e);
        println!("#{}  {} UTC  {head}", e.seq, &fmt_utc(e.ts_ms)[..19]);
        for line in body {
            println!("    {line}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_accepts_dates_and_rfc3339_times() {
        let day = parse_utc("2026-12-01").unwrap();
        assert_eq!(parse_due("2026-12-01").unwrap(), day);
        assert_eq!(parse_due("2026-12-01T00:00:00Z").unwrap(), day);
        assert_eq!(parse_due("2026-12-01T09:30Z").unwrap(), day + (9 * 60 + 30) * 60_000);
        // 偏移：17:00+08:00 就是 09:00Z；负偏移往后推
        assert_eq!(parse_due("2026-12-01T17:00:00+08:00").unwrap(), day + 9 * 3_600_000);
        assert_eq!(parse_due("2026-12-01T17:00-05:30").unwrap(), day + (22 * 60 + 30) * 60_000);
        assert_eq!(parse_due(" 2026-12-01 ").unwrap(), day);
    }

    #[test]
    fn due_rejects_everything_else() {
        for bad in [
            "tomorrow",
            "",
            "2026-02-30",
            "2026-12-01 09:00",
            "2026-12-01T09:00",
            "2026-12-01T09:00:00.5Z",
            "2026-12-01T09:00+8:00",
            "2026-12-01T09:00+24:00",
            "2026-12-01T09:00+08:60",
            "12/01/2026",
            "1970-01-01T00:00+01:00",
        ] {
            let e = parse_due(bad).expect_err(bad);
            assert_eq!(e.exit, crate::error::EXIT_USAGE, "{bad}");
        }
    }

    #[test]
    fn friendly_messages_replace_the_cap_reasons_only() {
        let mut e = CliError::usage("todos: too_many");
        e.reason = Some("too_many".into());
        assert!(friendly(e).message.contains("full"));
        let mut e = CliError::usage("title: x");
        e.reason = None;
        assert_eq!(friendly(e).message, "title: x");
    }
}
