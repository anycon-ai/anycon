//! `anycon todo`：节点上的个人清单（docs/spec/cli.md § 3.8，docs/spec/daemon.md § 21）。
//!
//! 和别的客户端命令一样走 [`App::conn`]（本机 socket，或 `--hub` / `--node` 经 hub）。`<id>` 接受唯一前缀。
//!
//! 在 anycon 拉起的 agent 里运行时（环境里有 `ANYCON_AGENT`，和 `anycon mcp` 是同一个判断）：`add` 带 `agent: true` 和
//! `origin_session`（读 `ANYCON_SESSION_ID`），`ls` / `history` 照常，`done` / `drop` / `reopen` / `rm` / `run` / `repeat-now` 拒绝执行。
//! 这是和 `anycon mcp` 不给 agent 修改清单的工具同一条界线的**护栏，不是安全边界**：agent 能改自己的环境
//! （比如先清掉 `ANYCON_AGENT` 再运行），能执行 shell 的 agent 本来也能直接发 RPC。它防的是 agent 照常干活时顺手改掉人的清单。
//!
//! 清单项的标题和正文可能来自 agent，表格输出经 [`table`] 可见化；`--json` 输出原始内容。`history` 里的 `result` 文字是 agent 写的
//! （可能被注入），经 [`visible`] 之后缩进显示，终端转义不会被解释。

use anycon_daemon::env::{AGENT_MARKER_ENV, SESSION_ID_ENV};
use anycon_daemon::util::{fmt_utc, parse_utc};
use anycon_protocol::*;
use serde_json::{json, Value};

use crate::cli::{TodoAddArgs, TodoCmd, TodoEditArgs, TodoHistoryArgs, TodoLsArgs, TodoRunArgs};
use crate::commands::{is_method_not_found, App};
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR};
use crate::output::{sanitize_label, table, visible};
use crate::todo_state_cmds;

/// 这个进程是不是在 anycon 拉起的 agent 里。
pub(crate) fn inside_agent() -> bool {
    crate::mcp::started_inside_a_session()
}

/// 在 agent 里拒绝会改动清单的子命令。
fn refuse_inside_agent(what: &str) -> Result<()> {
    if inside_agent() {
        let message = if what == "repeat-now" {
            format!(
                "`anycon todo repeat-now` is not available inside an anycon agent session ({AGENT_MARKER_ENV} is set): \
                 running a repeating to-do creates work for you and the twin. Run it from your own terminal."
            )
        } else if what == "run" {
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

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

const REPEAT_FORMS: &str = "use daily@09:00, weekdays@09:00 or weekly:mon@09:00, or several times a day as \
    <days>@<from>-<until>/<every> such as weekdays@09:00-18:00/2h or weekly:mon@09:00-12:00/30m \
    (<days> is daily, weekdays or weekly:<mon..sun>; <every> is <n>m or <n>h from 15m to 12h; \
    times are HH:MM on the node's clock)";

/// `<days>` 的写法：`daily`、`weekdays`、`weekly:mon`…`weekly:sun`（周一 = 1）。
fn parse_days(rule: &str) -> Option<(TodoRepeatDays, Option<u8>)> {
    match rule.split_once(':') {
        None if rule == "daily" => Some((TodoRepeatDays::Daily, None)),
        None if rule == "weekdays" => Some((TodoRepeatDays::Weekdays, None)),
        Some(("weekly", day)) => {
            let n = WEEKDAYS.iter().position(|d| d.eq_ignore_ascii_case(day))?;
            Some((TodoRepeatDays::Weekly, Some(n as u8 + 1)))
        }
        _ => None,
    }
}

/// `<n>m` 或 `<n>h` → 分钟数（范围由调用方检查）。
fn parse_every(s: &str) -> Option<u32> {
    // 按后缀拆，不按字节位置：用户写的单位可能是多字节字符，按字节切会落在字符中间
    let (n, scale) = if let Some(n) = s.strip_suffix('m') { (n, 1) } else { (s.strip_suffix('h')?, 60) };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse::<u32>().ok()?.checked_mul(scale)
}

/// `--repeat`：`daily@09:00`、`weekdays@09:00`、`weekly:mon@09:00`，或一天里重复多次的 `<days>@<from>-<until>/<every>`
/// （如 `weekdays@09:00-18:00/2h`）。节点的本地时间；星期写 `mon`…`sun`，周一 = 1。
pub fn parse_repeat(s: &str) -> Result<TodoRepeat> {
    let bad = || CliError::usage(format!("--repeat: cannot read {s:?}; {REPEAT_FORMS}"));
    let (rule, time) = s.trim().split_once('@').ok_or_else(bad)?;
    let plain = |kind, weekday, at: &str| TodoRepeat {
        kind,
        at: at.to_string(),
        weekday,
        days: None,
        every_min: None,
        until: None,
        auto_close: false,
        keep_state: false,
        state_hint_off: false,
        next_ms: None,
    };
    if let Some((range, every)) = time.split_once('/') {
        let (from, until) = range.split_once('-').ok_or_else(bad)?;
        let (days, weekday) = parse_days(rule).ok_or_else(bad)?;
        let start = anycon_daemon::todo_repeat::parse_at(from).ok_or_else(bad)?;
        let end = anycon_daemon::todo_repeat::parse_at(until).ok_or_else(bad)?;
        let every_min = parse_every(every).ok_or_else(bad)?;
        if end.0 * 60 + end.1 <= start.0 * 60 + start.1 {
            return Err(CliError::usage(format!(
                "--repeat: {s:?}: the end time must be later than the start time on the same day"
            )));
        }
        if !(anycon_daemon::todos::MIN_EVERY_MIN..=anycon_daemon::todos::MAX_EVERY_MIN).contains(&every_min) {
            return Err(CliError::usage(format!("--repeat: {s:?}: the interval must be between 15m and 12h")));
        }
        return Ok(TodoRepeat {
            days: Some(days),
            every_min: Some(every_min),
            until: Some(until.to_string()),
            ..plain(TodoRepeatKind::Interval, weekday, from)
        });
    }
    anycon_daemon::todo_repeat::parse_at(time).ok_or_else(bad)?;
    let (days, weekday) = parse_days(rule).ok_or_else(bad)?;
    let kind = match days {
        TodoRepeatDays::Daily => TodoRepeatKind::Daily,
        TodoRepeatDays::Weekdays => TodoRepeatKind::Weekdays,
        _ => TodoRepeatKind::Weekly,
    };
    Ok(plain(kind, weekday, time))
}

/// 规则写回 `--repeat` 的写法。
fn repeat_str(r: &TodoRepeat) -> String {
    let days = |d: TodoRepeatDays| match (d, r.weekday) {
        (TodoRepeatDays::Daily, _) => Some("daily".to_string()),
        (TodoRepeatDays::Weekdays, _) => Some("weekdays".to_string()),
        (TodoRepeatDays::Weekly, Some(w @ 1..=7)) => Some(format!("weekly:{}", WEEKDAYS[w as usize - 1])),
        _ => None,
    };
    let text = match r.kind {
        TodoRepeatKind::Daily => days(TodoRepeatDays::Daily).map(|d| format!("{d}@{}", r.at)),
        TodoRepeatKind::Weekdays => days(TodoRepeatDays::Weekdays).map(|d| format!("{d}@{}", r.at)),
        TodoRepeatKind::Weekly => days(TodoRepeatDays::Weekly).map(|d| format!("{d}@{}", r.at)),
        TodoRepeatKind::Interval => match (r.days.and_then(days), r.until.as_deref(), r.every_min) {
            (Some(d), Some(until), Some(m)) => {
                let every = if m % 60 == 0 { format!("{}h", m / 60) } else { format!("{m}m") };
                Some(format!("{d}@{}-{until}/{every}", r.at))
            }
            _ => None,
        },
        TodoRepeatKind::Unknown => None,
    };
    text.unwrap_or_else(|| "unknown".to_string())
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
pub(crate) async fn resolve_todo(
    conn: &Conn,
    key: &str,
    closed_first: bool,
) -> Result<std::result::Result<Todo, String>> {
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

pub(crate) fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `too_many` 等细分原因换成人话。
fn friendly(e: CliError) -> CliError {
    match e.reason.as_deref() {
        Some("too_many") => {
            CliError::new(e.exit, e.code, "the to-do list is full (1000 open items); close or remove some first")
        }
        Some("repeat_template") => CliError::new(
            e.exit,
            e.code,
            "a repeating to-do is a template and cannot be dispatched; dispatch one of the copies it creates",
        ),
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
        TodoCmd::Edit(a) => edit(app, a).await,
        TodoCmd::Assign { id, handler } => assign(app, &id, &handler).await,
        TodoCmd::Add(a) => add(app, a).await,
        TodoCmd::Ls(a) => ls(app, a).await,
        TodoCmd::Done { id } => set_state(app, "done", &id, TodoState::Done, "done").await,
        TodoCmd::Drop { id } => set_state(app, "drop", &id, TodoState::Dropped, "dropped").await,
        TodoCmd::Reopen { id } => set_state(app, "reopen", &id, TodoState::Open, "reopened").await,
        TodoCmd::Rm { id } => rm(app, &id).await,
        TodoCmd::Run(a) => run(app, a).await,
        TodoCmd::History(a) => history(app, a).await,
        TodoCmd::RepeatNow { id } => repeat_now(app, &id).await,
        TodoCmd::State(cmd) => todo_state_cmds::state(app, cmd).await,
    }
}

async fn add(app: &App, a: TodoAddArgs) -> Result<u8> {
    if a.twin {
        refuse_inside_agent("add --twin")?;
    }
    if a.repeat.is_some() {
        refuse_inside_agent("add --repeat")?;
    }
    let mut repeat = a.repeat.as_deref().map(parse_repeat).transpose()?;
    if let Some(r) = repeat.as_mut() {
        r.auto_close = a.auto_close;
        r.keep_state = a.keep_state;
        r.state_hint_off = a.no_state_hint;
    }
    let due_ms = a.due.as_deref().map(parse_due).transpose()?;
    let nested = inside_agent();
    let params = TodoAddParams {
        handler: a.twin.then_some(TodoHandler::Twin),
        // 客户端生成 id：`todo.add` 按它幂等
        id: uuid::Uuid::new_v4().to_string(),
        title: a.title,
        body: a.body,
        project: a.project,
        due_ms,
        repeat,
        agent: nested.then_some(true),
        origin_session: nested.then(|| std::env::var(SESSION_ID_ENV).ok().filter(|s| !s.is_empty())).flatten(),
    };
    let conn = app.conn().await?;
    if params.repeat.as_ref().is_some_and(needs_interval_support) {
        require_interval_support(&conn).await?;
    }
    if params.repeat.as_ref().is_some_and(|r| r.keep_state) {
        require_state_support(&conn).await?;
    }
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

/// `daily@09:00 next 2026-12-01 01:00Z`：规则按节点的时钟，下一次的时间换成 UTC 显示（和 DUE 一样）。
fn repeat_cell(r: &TodoRepeat) -> String {
    let mut text = repeat_str(r);
    if r.auto_close {
        text.push_str(" auto-close");
    }
    if r.keep_state {
        text.push_str(if r.state_hint_off { " keeps-state(no-note)" } else { " keeps-state" });
    }
    match r.next_ms {
        Some(next) => format!("{text} next {}Z", &fmt_utc(next)[..16]),
        None => text,
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
    let mut rows = vec![["ID", "STATE", "ACTIVITY", "DUE", "REPEAT", "PROJECT", "BY", "TITLE"]
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()];
    for t in &todos {
        let by = match (t.source.kind, t.unreviewed) {
            (TodoSourceKind::Agent, true) => app.out.yellow("agent (unreviewed)"),
            (TodoSourceKind::Agent, false) => "agent".to_string(),
            (TodoSourceKind::Repeat, _) => "repeat".to_string(),
            // 另一个节点的 Twin 经 Twin 连接记的项
            (TodoSourceKind::Twin, _) if t.source.device_id.is_some() => "twin device".to_string(),
            (TodoSourceKind::Twin | TodoSourceKind::Unknown, true) => app.out.yellow("other (unreviewed)"),
            (TodoSourceKind::Twin | TodoSourceKind::Unknown, false) => "other".to_string(),
            _ => String::new(),
        };
        rows.push(vec![
            short(&t.id),
            state_str(t.state).to_string(),
            t.activity.map(activity_str).unwrap_or_default().to_string(),
            t.due_ms.map(|d| fmt_utc(d)[..16].to_string()).unwrap_or_default(),
            t.repeat.as_ref().map(repeat_cell).unwrap_or_default(),
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
        handler: None,
        agent: None,
        id: String::new(),
        title: None,
        body: None,
        project: None,
        due_ms: None,
        state: None,
        reviewed: None,
        repeat: None,
        clear: Vec::new(),
    }
}

/// 这条规则用到了只有较新的 daemon 才认识的东西（间隔规则、`auto_close`）。
fn needs_interval_support(r: &TodoRepeat) -> bool {
    r.kind == TodoRepeatKind::Interval || r.auto_close
}

/// 节点的 daemon 不认识周期项的跨次状态（`keep_state`、`todo.state_*`）时说清楚。
pub(crate) async fn require_state_support(conn: &Conn) -> Result<()> {
    let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await?;
    if info.capabilities.todo_repeat_state {
        Ok(())
    } else {
        Err(CliError::general(
            "this node's daemon is too old for that: it does not keep state for repeating to-dos; update anycon on that node",
        ))
    }
}

/// 节点的 daemon 不认识间隔规则 / `auto_close` / `repeat-now` 时说清楚，而不是让它回一个看不懂的 `invalid`。
async fn require_interval_support(conn: &Conn) -> Result<()> {
    let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await?;
    if info.capabilities.todo_repeat_interval {
        Ok(())
    } else {
        Err(CliError::general(
            "this node's daemon is too old for that: it does not support several-times-a-day repeats, --auto-close or \
             repeat-now; update anycon on that node",
        ))
    }
}

async fn edit(app: &App, a: TodoEditArgs) -> Result<u8> {
    refuse_inside_agent("edit")?;
    let mut repeat = a.repeat.as_deref().map(parse_repeat).transpose()?;
    let wants_auto = a.auto_close || a.no_auto_close;
    let wants_keep = a.keep_state || a.no_keep_state;
    let wants_hint = a.state_hint || a.no_state_hint;
    let conn = app.conn().await?;
    let resolved = resolve_todo(&conn, &a.id, false).await?;
    let id = match &resolved {
        Ok(t) => t.id.clone(),
        Err(id) => id.clone(),
    };
    let mut clear = Vec::new();
    if a.no_repeat {
        clear.push(ClearableTodoField::Repeat);
    } else {
        // 读出原来的规则：只换时间时沿用原来的开关（不悄悄关掉），只改开关时原样发回去
        let old = fetch_rule(&conn, &id).await?;
        let mut r = match (repeat.take(), &old) {
            (Some(mut r), Some(old)) => {
                r.auto_close = old.auto_close;
                r.keep_state = old.keep_state;
                r.state_hint_off = old.state_hint_off;
                r
            }
            (Some(r), None) => r,
            (None, Some(old)) => old.clone(),
            (None, None) => return Err(CliError::usage(
                "--auto-close, --keep-state and their --no- forms only apply to a repeating item; this one does not \
                     repeat (give --repeat to make it one)",
            )),
        };
        r.next_ms = None;
        if wants_auto {
            r.auto_close = a.auto_close;
        }
        if wants_keep {
            r.keep_state = a.keep_state;
        }
        if wants_hint {
            r.state_hint_off = a.no_state_hint;
        }
        if r.state_hint_off && !r.keep_state {
            if a.no_state_hint {
                return Err(CliError::usage(
                    "--no-state-hint only applies to an item that keeps state (add --keep-state)",
                ));
            }
            // 关掉状态时「不带说明」没有意义，一起去掉
            r.state_hint_off = false;
        }
        repeat = Some(r);
    }
    if repeat.as_ref().is_some_and(needs_interval_support) {
        require_interval_support(&conn).await?;
    }
    if repeat.as_ref().is_some_and(|r| r.keep_state) {
        require_state_support(&conn).await?;
    }
    let t: Todo = conn
        .call(methods::TODO_UPDATE, TodoUpdateParams { id, repeat, clear, ..empty_update() })
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&t);
    } else if let Some(r) = &t.repeat {
        println!("{} repeats {}", short(&t.id), repeat_cell(r));
    } else {
        println!("{} no longer repeats", short(&t.id));
    }
    Ok(0)
}

/// 读出这一项现在的周期规则；不是周期项时为 `None`。
async fn fetch_rule(conn: &Conn, id: &str) -> Result<Option<TodoRepeat>> {
    let t: Todo = conn.call(methods::TODO_GET, TodoIdParams { id: id.to_string() }).await?;
    Ok(t.repeat)
}

/// `todo.repeat_now` 的 `invalid` 原因换成人话。
fn friendly_repeat_now(e: CliError) -> CliError {
    let message = match e.reason.as_deref() {
        Some("not_template") => "this to-do does not repeat, so there is nothing to run; see `anycon todo edit --repeat`".to_string(),
        Some("closed") => "this repeating to-do is closed (done or dropped); reopen it with `anycon todo reopen` first".to_string(),
        Some("unreviewed") => "this repeating to-do was added by an agent and is still waiting for your review; accept it first".to_string(),
        Some("previous_open") => format!(
            "the previous copy {} is still open; finish or drop it first (a new copy is only made once the last one is closed)",
            e.todo_id.as_deref().map(short).unwrap_or_else(|| "of this to-do".to_string())
        ),
        Some("too_many") => "the to-do list is full (1000 open items); close or remove some first".to_string(),
        _ if is_method_not_found(&e) => {
            "this node's daemon is too old for repeat-now; update anycon on that node".to_string()
        }
        _ => return e,
    };
    CliError::new(e.exit, e.code, message)
}

async fn repeat_now(app: &App, key: &str) -> Result<u8> {
    refuse_inside_agent("repeat-now")?;
    let conn = app.conn().await?;
    let id = match resolve_todo(&conn, key, false).await? {
        Ok(t) => t.id,
        Err(id) => id,
    };
    let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await?;
    if !info.capabilities.todo_repeat_interval {
        return Err(CliError::general("this node's daemon is too old for repeat-now; update anycon on that node"));
    }
    let t: Todo = conn.call(methods::TODO_REPEAT_NOW, TodoIdParams { id }).await.map_err(friendly_repeat_now)?;
    if app.json() {
        app.out.json_line(&t);
    } else {
        println!("added {}  {}", short(&t.id), sanitize_label(&t.title, 80, ""));
    }
    Ok(0)
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
        ("invalid", Some("repeat_template")) => CliError::new(
            e.exit,
            e.code,
            "this to-do repeats, so it is a template; dispatch one of the copies it creates instead",
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
        note: None,
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
        (TodoSourceKind::Twin, _) => "the twin".to_string(),
        (TodoSourceKind::Repeat, _) => "a repeating item".to_string(),
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
        TodoEventData::State { from, to, by: TodoActor { kind: TodoSourceKind::Repeat, .. } } => (
            format!(
                "{} -> {}: closed by the repeating item's auto-close setting after the twin decided it was done",
                from.as_str(),
                to.as_str()
            ),
            vec![],
        ),
        TodoEventData::State { from, to, by: who } => {
            (format!("{} -> {} by {}", from.as_str(), to.as_str(), by(who)), vec![])
        }
        TodoEventData::Handler { from, to, by: who } => {
            (format!("handler: {} -> {} by {}", from.as_str(), to.as_str(), by(who)), vec![])
        }
        TodoEventData::HandedBack { reason, .. } => {
            ("handed back to you".into(), visible(reason).lines().map(str::to_string).collect())
        }
        TodoEventData::TakenOver { by: who, .. } => (format!("taken over by {}", by(who)), vec![]),
        TodoEventData::TwinDecision { reason, .. } => {
            ("twin decision".into(), visible(reason).lines().map(str::to_string).collect())
        }
        TodoEventData::TwinQuestion { question_id } => {
            (format!("twin question {}", sanitize_label(question_id, 80, "?")), vec![])
        }
        TodoEventData::RepeatCreated { todo_id, at_ms, manual: true } => {
            (format!("repeated by hand: created {} at {}Z", short(todo_id), &fmt_utc(*at_ms)[..16]), vec![])
        }
        TodoEventData::RepeatCreated { todo_id, at_ms, manual: false } => {
            (format!("repeated: created {} for {}Z", short(todo_id), &fmt_utc(*at_ms)[..16]), vec![])
        }
        TodoEventData::RepeatSkipped { at_ms } => {
            (format!("repeated: skipped {}Z, the previous copy is still open", &fmt_utc(*at_ms)[..16]), vec![])
        }
        TodoEventData::PrEvent { session_id, event, url } => {
            let what = match event {
                PrEventKind::ChecksFailed => "checks failed",
                PrEventKind::Merged => "merged",
                PrEventKind::Closed => "closed without merging",
                PrEventKind::Reviewed => "new review activity",
                PrEventKind::Unknown => "changed",
            };
            let link =
                url.as_deref().filter(|u| u.starts_with("https://github.com/")).map(|u| sanitize_label(u, 200, ""));
            (format!("pull request {what} (session {session_id})"), link.into_iter().collect())
        }
        TodoEventData::StateUsage { session_id, turn, reads, writes } => {
            (format!("state used in session {session_id}, turn {turn}: {reads} reads, {writes} writes"), vec![])
        }
        TodoEventData::RemoteDispatched { node_id, todo_id, session_id } => (
            format!(
                "dispatched on node {} (to-do {}, session {})",
                sanitize_label(node_id, 40, "?"),
                short(todo_id),
                sanitize_label(session_id, 40, "?")
            ),
            vec![],
        ),
        TodoEventData::RemoteResult { node_id, session_id, text } => (
            format!(
                "report from node {}, session {}",
                sanitize_label(node_id, 40, "?"),
                sanitize_label(session_id, 40, "?")
            ),
            if text.is_empty() {
                vec!["(no text)".to_string()]
            } else {
                visible(text).lines().map(str::to_string).collect()
            },
        ),
        TodoEventData::Unknown => ("unrecognized event".into(), vec![]),
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

async fn assign(app: &App, key: &str, handler: &str) -> Result<u8> {
    refuse_inside_agent("assign")?;
    let conn = app.conn().await?;
    let id = match resolve_todo(&conn, key, false).await? {
        Ok(t) => t.id,
        Err(id) => id,
    };
    let t: Todo = conn
        .call(
            methods::TODO_UPDATE,
            TodoUpdateParams {
                id,
                handler: Some(if handler == "twin" { TodoHandler::Twin } else { TodoHandler::Me }),
                ..empty_update()
            },
        )
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&t);
    } else {
        println!("{} is handled by {}", short(&t.id), handler);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// daemon 因为 agent 没有「不 push」的选项拒绝派发时，CLI 说清楚是哪个 agent、加 `--allow-push` 才能派。
    /// 集成测试只有 mock agent，它声明了 `no_push`，所以这条拒绝在这里直接测。
    #[test]
    fn a_dispatch_refused_for_no_push_tells_how_to_go_ahead() {
        let mut e = CliError::new(1, "unsupported", "the agent cannot block pushes");
        e.reason = Some("no_push".into());
        let out = friendly_dispatch(e, "acme");
        assert_eq!((out.exit, out.code.as_str()), (1, "unsupported"));
        assert!(out.message.contains("agent acme") && out.message.contains("--allow-push"), "{}", out.message);
        // 别的原因不改写
        let mut other = CliError::new(1, "unsupported", "something else");
        other.reason = Some("other".into());
        assert_eq!(friendly_dispatch(other, "acme").message, "something else");
    }

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

    #[test]
    fn repeat_specs_parse_and_print_back() {
        let r = parse_repeat("daily@09:00").unwrap();
        assert_eq!((r.kind, r.at.as_str(), r.weekday), (TodoRepeatKind::Daily, "09:00", None));
        assert_eq!(parse_repeat("weekdays@23:59").unwrap().kind, TodoRepeatKind::Weekdays);
        let r = parse_repeat(" weekly:Sun@00:00 ").unwrap();
        assert_eq!((r.kind, r.weekday), (TodoRepeatKind::Weekly, Some(7)));
        assert_eq!(repeat_str(&r), "weekly:sun@00:00");
        assert_eq!(parse_repeat("weekly:mon@09:00").unwrap().weekday, Some(1));
        for spec in [
            "daily",
            "daily@9:00",
            "daily@24:00",
            "daily@09:60",
            "daily:mon@09:00",
            "weekly@09:00",
            "weekly:xyz@09:00",
            "weekdays:mon@09:00",
            "monthly@09:00",
            "@09:00",
            "",
        ] {
            assert!(parse_repeat(spec).is_err(), "{spec:?} should be rejected");
        }
    }

    #[test]
    fn interval_specs_parse_and_print_back() {
        let r = parse_repeat("weekdays@09:00-18:00/2h").unwrap();
        assert_eq!(
            (r.kind, r.at.as_str(), r.days, r.every_min, r.until.as_deref(), r.weekday),
            (TodoRepeatKind::Interval, "09:00", Some(TodoRepeatDays::Weekdays), Some(120), Some("18:00"), None)
        );
        assert_eq!(repeat_str(&r), "weekdays@09:00-18:00/2h");
        let r = parse_repeat("weekly:Mon@09:00-12:00/30m").unwrap();
        assert_eq!((r.days, r.weekday, r.every_min), (Some(TodoRepeatDays::Weekly), Some(1), Some(30)));
        assert_eq!(repeat_str(&r), "weekly:mon@09:00-12:00/30m");
        assert_eq!(repeat_str(&parse_repeat("daily@00:00-23:59/90m").unwrap()), "daily@00:00-23:59/90m");
        assert_eq!(parse_repeat("daily@09:00-18:00/15m").unwrap().every_min, Some(15));
        assert_eq!(parse_repeat("daily@09:00-23:00/12h").unwrap().every_min, Some(720));
        for spec in [
            "daily@09:00-18:00",
            "daily@09:00/2h",
            "daily@09:00-18:00/2",
            "daily@09:00-18:00/2d",
            "daily@09:00-18:00/h",
            "daily@09:00-18:00/14m",
            "daily@09:00-18:00/13h",
            "daily@09:00-18:00/0m",
            "daily@18:00-09:00/2h",
            "daily@09:00-09:00/15m",
            "weekly@09:00-18:00/2h",
            "weekly:xyz@09:00-18:00/2h",
            "monthly@09:00-18:00/2h",
            "daily@9:00-18:00/2h",
            // 多字节的单位：要报写法不对，不能 panic
            "daily@09:00-18:00/2时",
            "daily@09:00-18:00/时",
            "daily@09:00-18:00/+2h",
        ] {
            let e = parse_repeat(spec).expect_err(spec);
            assert_eq!(e.exit, crate::error::EXIT_USAGE, "{spec}");
        }
        // 错误信息列出所有写法
        let msg = parse_repeat("nope").unwrap_err().message;
        assert!(msg.contains("weekly:mon@09:00") && msg.contains("weekdays@09:00-18:00/2h"), "{msg}");
    }

    #[test]
    fn the_repeat_cell_shows_auto_close() {
        let mut r = parse_repeat("weekdays@09:00-18:00/2h").unwrap();
        r.auto_close = true;
        r.next_ms = parse_utc("2026-12-04T10:30Z");
        assert_eq!(repeat_cell(&r), "weekdays@09:00-18:00/2h auto-close next 2026-12-04 10:30Z");
    }

    #[test]
    fn repeat_now_refusals_are_explained_with_the_open_copy() {
        let mut e = CliError::usage("id: previous_open");
        e.reason = Some("previous_open".into());
        e.todo_id = Some("12345678-aaaa-bbbb-cccc-1234567890ab".into());
        assert!(friendly_repeat_now(e).message.contains("12345678"));
        for reason in ["not_template", "closed", "unreviewed", "too_many"] {
            let mut e = CliError::usage(format!("id: {reason}"));
            e.reason = Some(reason.into());
            assert!(!friendly_repeat_now(e).message.starts_with("id:"), "{reason}");
        }
    }

    #[test]
    fn the_repeat_cell_shows_the_rule_and_the_next_time_in_utc() {
        let mut r = parse_repeat("weekly:fri@18:30").unwrap();
        assert_eq!(repeat_cell(&r), "weekly:fri@18:30");
        r.next_ms = parse_utc("2026-12-04T10:30Z");
        assert_eq!(repeat_cell(&r), "weekly:fri@18:30 next 2026-12-04 10:30Z");
    }
}
