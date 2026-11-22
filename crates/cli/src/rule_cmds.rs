//! `anycon rule`：节点上的决策档案——用户自己的做法，生效的规则注入派发出去的会话的系统提示
//! （docs/spec/cli.md § 3.9，docs/spec/daemon.md § 23）。
//!
//! 和别的客户端命令一样走 [`App::conn`]（本机 socket，或 `--hub` / `--node` 经 hub）。`<id>` 接受唯一前缀。
//!
//! 在 anycon 拉起的 agent 里运行时（环境里有 `ANYCON_AGENT`，和 `anycon mcp`、`anycon todo` 是同一个判断）：`add` 带 `agent: true`
//! （规则记成待确认，要人确认才生效），`ls` 照常，`confirm` / `reject` / `disable` / `enable` / `rm` 拒绝执行。
//! 这是**护栏，不是安全边界**：agent 能改自己的环境（比如先清掉 `ANYCON_AGENT` 再运行），能执行 shell 的 agent 本来也能直接发 RPC；
//! 真正的闸是「待确认的规则要人确认才生效」。
//!
//! 规则的文字可能来自 agent（待确认的规则），表格输出经 [`sanitize_label`] 去掉控制字符；`--json` 输出原始内容。

use anycon_daemon::env::{AGENT_MARKER_ENV, SESSION_ID_ENV};
use anycon_daemon::rules::BUDGET_CAP;
use anycon_protocol::*;
use serde_json::{json, Value};

use crate::cli::{RuleAddArgs, RuleCmd, RuleLsArgs};
use crate::commands::App;
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR};
use crate::output::{sanitize_label, table, visible};

/// 这个进程是不是在 anycon 拉起的 agent 里。
fn inside_agent() -> bool {
    crate::mcp::started_inside_a_session()
}

/// 在 agent 里拒绝会改动规则的子命令。
fn refuse_inside_agent(what: &str) -> Result<()> {
    if inside_agent() {
        return Err(CliError::denied(
            "forbidden",
            format!(
                "`anycon rule {what}` is not available inside an anycon agent session ({AGENT_MARKER_ENV} is set): an \
                 agent can propose a rule with `anycon rule add` and read the rules, but confirming, rejecting, \
                 disabling, enabling and removing rules is left to you. Run it from your own terminal."
            ),
        ));
    }
    Ok(())
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 一条规则在表格里的样子：多行的用 ` / ` 连起来，控制字符去掉。
fn one_line(text: &str) -> String {
    sanitize_label(&text.split('\n').map(str::trim).collect::<Vec<_>>().join(" / "), 100, "")
}

/// 几个错误换成人话。
fn friendly(e: CliError) -> CliError {
    match e.reason.as_deref() {
        Some("too_many") => {
            CliError::new(e.exit, e.code, "there are already 300 rules; remove some first (`anycon rule ls --all`)")
        }
        Some("too_many_pending") => CliError::new(
            e.exit,
            e.code,
            "too many agent-proposed rules are waiting for review (50); a person has to confirm or reject some first",
        ),
        Some("invisible") => CliError::new(
            e.exit,
            e.code,
            "the rule contains bidirectional controls or zero-width characters, which cannot be reviewed; remove them",
        ),
        Some("over_budget") => CliError::new(
            e.exit,
            e.code,
            format!(
                "the active rules would take more than the {BUDGET_CAP} bytes a session gets; merge or disable some \
                 first (`anycon rule ls`)"
            ),
        ),
        Some("changed") => {
            CliError::new(e.exit, e.code, "the rule changed since it was listed; run `anycon rule ls` and try again")
        }
        _ => e,
    }
}

async fn list_all(conn: &Conn) -> Result<RuleListResult> {
    conn.call(methods::RULE_LIST, RuleListParams::default()).await
}

/// `<id>`（唯一前缀，不区分大小写；id 本身是小写 UUID）。完整的 UUID 不用列表，直接返回 `Err(id)` 让调用方拿它去调用。
/// 没有匹配 → 退出码 1（`not_found`），多个 → 2。
async fn resolve_rule(conn: &Conn, key: &str) -> Result<std::result::Result<Rule, String>> {
    let key = key.trim().to_lowercase();
    if key.is_empty() {
        return Err(CliError::usage("the rule id must not be empty"));
    }
    if key.len() == 36 && uuid::Uuid::parse_str(&key).is_ok() {
        return Ok(Err(key));
    }
    let all = list_all(conn).await?.rules;
    let m: Vec<&Rule> = all.iter().filter(|r| r.id.starts_with(&key)).collect();
    match m.len() {
        0 => Err(CliError::new(EXIT_ERROR, "not_found", format!("no rule matches {key:?}"))),
        1 => Ok(Ok(m[0].clone())),
        n => Err(CliError::usage(format!("{key:?} matches {n} rules; use a longer prefix"))),
    }
}

pub async fn rule(app: &App, cmd: RuleCmd) -> Result<u8> {
    match cmd {
        RuleCmd::Extract { session, no_wait } => extract(app, &session, no_wait).await,
        RuleCmd::Runs { session } => runs(app, session.as_deref()).await,
        RuleCmd::Add(a) => add(app, a).await,
        RuleCmd::Ls(a) => ls(app, a).await,
        RuleCmd::Show { id } => show(app, &id).await,
        RuleCmd::Confirm { id } => set_status(app, "confirm", &id, RuleStatus::Active, "confirmed").await,
        RuleCmd::Reject { id } => set_status(app, "reject", &id, RuleStatus::Rejected, "rejected").await,
        RuleCmd::Disable { id } => set_status(app, "disable", &id, RuleStatus::Disabled, "disabled").await,
        RuleCmd::Enable { id } => set_status(app, "enable", &id, RuleStatus::Active, "enabled").await,
        RuleCmd::Rm { id } => rm(app, &id).await,
    }
}

async fn add(app: &App, a: RuleAddArgs) -> Result<u8> {
    let nested = inside_agent();
    let params = RuleAddParams {
        // 客户端生成 id：`rule.add` 按它幂等
        id: uuid::Uuid::new_v4().to_string(),
        text: a.text,
        project: a.project,
        status: None,
        source: None,
        agent: nested.then_some(true),
        origin_session: nested.then(|| std::env::var(SESSION_ID_ENV).ok().filter(|s| !s.is_empty())).flatten(),
    };
    let conn = app.conn().await?;
    let r: Rule = conn.call(methods::RULE_ADD, params).await.map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
    } else if r.status == RuleStatus::Pending {
        println!(
            "proposed {}  {}\nit does nothing until you confirm it: `anycon rule confirm {}`",
            short(&r.id),
            one_line(&r.text),
            short(&r.id)
        );
    } else {
        println!("added {}  {}", short(&r.id), one_line(&r.text));
    }
    Ok(0)
}

/// 一条规则的全部内容：文字经 [`visible`]（控制字符显示成转义，换行保留），缩进列在下面。
fn print_full(r: &Rule) {
    println!("    status:  {}", status_str(r.status));
    println!("    project: {}", sanitize_label(r.project.as_deref().unwrap_or("(all projects)"), 200, "?"));
    let by = match r.source.kind {
        RuleSourceKind::Manual => "typed by a person".to_string(),
        RuleSourceKind::Answer => "saved from an answer".to_string(),
        RuleSourceKind::Extracted => "extracted from a session".to_string(),
        RuleSourceKind::Agent => "proposed by an agent".to_string(),
        RuleSourceKind::Unknown => "unknown source".to_string(),
    };
    let session = r.source.session_id.as_deref().map(|s| format!(", session {}", sanitize_label(s, 60, "?")));
    println!("    source:  {by}{}", session.unwrap_or_default());
    println!("    text:");
    for line in visible(&r.text).lines() {
        println!("      {line}");
    }
}

async fn show(app: &App, key: &str) -> Result<u8> {
    let conn = app.conn().await?;
    let r = match resolve_rule(&conn, key).await? {
        Ok(r) => r,
        Err(id) => list_all(&conn)
            .await?
            .rules
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| CliError::new(EXIT_ERROR, "not_found", format!("no rule matches {id:?}")))?,
    };
    if app.json() {
        app.out.json_line(&r);
    } else {
        println!("{}", r.id);
        print_full(&r);
    }
    Ok(0)
}

fn status_str(s: RuleStatus) -> &'static str {
    match s {
        RuleStatus::Unknown => "unknown",
        s => s.as_str(),
    }
}

async fn ls(app: &App, a: RuleLsArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let l = list_all(&conn).await?;
    let shown: Vec<&Rule> =
        l.rules.iter().filter(|r| a.all || matches!(r.status, RuleStatus::Active | RuleStatus::Pending)).collect();
    if app.json() {
        app.out.json_line(&json!({ "rules": shown, "budget": l.budget }));
        return Ok(0);
    }
    if shown.is_empty() {
        println!("no rules");
        return Ok(0);
    }
    let mut rows = vec![["ID", "STATUS", "PROJECT", "BY", "RULE"].iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for r in &shown {
        let by = match (r.source.kind, r.status) {
            (RuleSourceKind::Agent, RuleStatus::Pending) => app.out.yellow("agent (needs review)"),
            (RuleSourceKind::Agent, _) => "agent".to_string(),
            (RuleSourceKind::Extracted, _) => "extracted".to_string(),
            (RuleSourceKind::Unknown, _) => "other".to_string(),
            _ => String::new(),
        };
        let status = match r.status {
            RuleStatus::Pending => app.out.yellow(status_str(r.status)),
            s => status_str(s).to_string(),
        };
        rows.push(vec![
            short(&r.id),
            status,
            sanitize_label(r.project.as_deref().unwrap_or(""), 40, ""),
            by,
            one_line(&r.text),
        ]);
    }
    print!("{}", table(&rows));
    // 一个会话最多拿到多少字节的规则：全节点的加上最大的那个项目的
    let worst = l.budget.node_bytes + l.budget.projects.values().copied().max().unwrap_or(0);
    println!("a session gets at most {worst} of {} bytes of rules", l.budget.cap);
    Ok(0)
}

async fn set_status(app: &App, what: &str, key: &str, status: RuleStatus, done: &str) -> Result<u8> {
    refuse_inside_agent(what)?;
    let conn = app.conn().await?;
    let resolved = resolve_rule(&conn, key).await?;
    let (id, known) = match &resolved {
        Ok(r) => (r.id.clone(), Some(r.clone())),
        Err(id) => (id.clone(), None),
    };
    // 已经是这个状态：daemon 当空操作，这里也如实说没变
    if let Some(r) = known.as_ref().filter(|r| r.status == status) {
        if app.json() {
            app.out.json_line(r);
        } else {
            println!("nothing changed: {} is already {}  {}", short(&r.id), status_str(status), one_line(&r.text));
            if status == RuleStatus::Active {
                print_full(r);
            }
        }
        return Ok(0);
    }
    let params = RuleUpdateParams {
        id,
        text: None,
        project: None,
        clear: Vec::new(),
        status: Some(status),
        // 列过表的，只改我们看到的那个版本：这期间别的设备改过它（比如改了文字）就不确认
        if_updated_ms: known.as_ref().map(|r| r.updated_ms),
    };
    let r: Rule = conn.call(methods::RULE_UPDATE, params).await.map_err(friendly)?;
    if app.json() {
        app.out.json_line(&r);
    } else {
        if known.is_none() {
            println!("{} is {}  {}", short(&r.id), status_str(r.status), one_line(&r.text));
        } else {
            println!("{done} {}  {}", short(&r.id), one_line(&r.text));
        }
        // 生效的规则会原样进每个会话的系统提示：把确认的那条的全文给人看
        if r.status == RuleStatus::Active {
            print_full(&r);
        }
    }
    Ok(0)
}

async fn rm(app: &App, key: &str) -> Result<u8> {
    refuse_inside_agent("rm")?;
    let conn = app.conn().await?;
    let resolved = resolve_rule(&conn, key).await?;
    let (id, if_updated_ms) = match &resolved {
        Ok(r) => (r.id.clone(), Some(r.updated_ms)),
        Err(id) => (id.clone(), None),
    };
    let _: Value = conn
        .call_raw(methods::RULE_DELETE, RuleDeleteParams { id: id.clone(), if_updated_ms })
        .await
        .map_err(friendly)?;
    if app.json() {
        app.out.json_line(&json!({ "id": id, "deleted": true }));
    } else {
        match resolved {
            Ok(r) => println!("removed {}  {}", short(&r.id), one_line(&r.text)),
            Err(_) => println!("removed {}", short(&id)),
        }
    }
    Ok(0)
}

/// 本机配置可读时使用实际超时；远程只公开种类，所以用该种类允许的最大超时。
async fn extract_timeout(app: &App, conn: &Conn, backend: &str) -> Result<std::time::Duration> {
    use anycon_daemon::config::JudgeBackendConfig;
    use std::time::Duration;
    if conn.local {
        if let Ok(config) = anycon_daemon::config::Config::load(&app.paths) {
            if let Some(backend) = config.judge.backends.get(backend) {
                let timeout = match backend {
                    JudgeBackendConfig::Api(c) => c.timeout,
                    JudgeBackendConfig::Agent(c) => c.timeout,
                };
                return Ok(timeout + Duration::from_secs(30));
            }
        }
    }
    let info: anycon_protocol::NodeInfo = conn.call(methods::NODE_INFO, json!({})).await?;
    let api = info
        .judge
        .and_then(|j| j.backends.into_iter().find(|b| b.name == backend))
        .is_some_and(|b| b.kind == anycon_protocol::JudgeBackendKind::Api);
    Ok(Duration::from_secs(if api { 630 } else { 930 }))
}

async fn extract(app: &App, key: &str, no_wait: bool) -> Result<u8> {
    use anycon_protocol::{ExtractRunStatus, RuleExtractDone, RuleExtractResult};
    refuse_inside_agent("extract")?;
    let mut conn = app.conn().await?;
    let session_id = crate::commands::resolve_session(&conn, key).await?;
    // 请求之前取走接收端：极快的后端可能先通知、后收到 RPC 回应。
    let mut notes = conn.notifications();
    let result: RuleExtractResult =
        conn.call(methods::RULE_EXTRACT, json!({"session_id": session_id})).await.map_err(|mut e| {
            if matches!(e.reason.as_deref(), Some("budget" | "busy" | "not_configured")) {
                e.exit = EXIT_ERROR;
            }
            e
        })?;
    let Some(mut run) = result.run.clone() else {
        if app.json() {
            app.out.json_line(&result);
        } else {
            println!("no new decision points");
        }
        return Ok(0);
    };
    if no_wait {
        if app.json() {
            app.out.json_line(&result);
        } else {
            println!("extraction started: {} ({} decision points)", run.id, run.points);
        }
        return Ok(0);
    }
    let waiting_since = tokio::time::Instant::now();
    let timeout = extract_timeout(app, &conn, &run.backend).await?;
    let deadline = waiting_since + timeout;
    while run.status == ExtractRunStatus::Running {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err(CliError::new(EXIT_ERROR, "timeout", "timed out waiting for extraction; check `anycon rule runs`")),
            _ = tokio::signal::ctrl_c() => return Err(CliError::interrupted()),
            note = notes.recv() => {
                let note = note.ok_or_else(|| CliError::offline("connection lost while waiting for extraction"))?;
                if note.method == methods::RULE_EXTRACT_DONE {
                    let done: RuleExtractDone = serde_json::from_value(note.params)?;
                    if done.run.id == run.id { run = done.run; }
                }
            }
        }
    }
    if app.json() {
        app.out.json_line(&run);
    }
    if run.status != ExtractRunStatus::Done {
        let error = run
            .error
            .unwrap_or(anycon_protocol::JudgeError { code: anycon_protocol::JudgeErrorCode::Internal, status: None });
        let code = if error.code == anycon_protocol::JudgeErrorCode::Unknown {
            "internal".to_owned()
        } else {
            serde_json::to_value(error.code)?.as_str().unwrap_or("internal").to_owned()
        };
        let explanation = match error.code {
            anycon_protocol::JudgeErrorCode::Auth => "backend authentication failed",
            anycon_protocol::JudgeErrorCode::RateLimited => "backend rate limit reached",
            anycon_protocol::JudgeErrorCode::Timeout => "backend timed out",
            anycon_protocol::JudgeErrorCode::Network => "could not reach the backend",
            anycon_protocol::JudgeErrorCode::BadOutput => "backend did not return usable rules",
            anycon_protocol::JudgeErrorCode::AgentFailed => "the judge agent failed",
            anycon_protocol::JudgeErrorCode::Cancelled => "extraction was cancelled",
            anycon_protocol::JudgeErrorCode::Http => "backend HTTP request failed",
            _ => "extraction failed internally",
        };
        return Err(CliError::new(
            EXIT_ERROR,
            code,
            format!("{explanation}{}", error.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default()),
        ));
    }
    if !app.json() {
        let all = list_all(&conn).await?;
        for id in &run.added {
            if let Some(rule) = all.rules.iter().find(|r| r.id == *id) {
                println!("{}\n{}\nConfirm with `anycon rule confirm {}`", rule.id, visible(&rule.text), rule.id);
            } else {
                println!("{} (rule is no longer available)", id);
            }
        }
        println!(
            "added {}; dropped {} invalid, {} duplicate, {} full",
            run.added.len(),
            run.dropped.invalid,
            run.dropped.duplicate,
            run.dropped.full
        );
    }
    Ok(0)
}

async fn runs(app: &App, session: Option<&str>) -> Result<u8> {
    let conn = app.conn().await?;
    let session_id = match session {
        Some(s) => Some(crate::commands::resolve_session(&conn, s).await?),
        None => None,
    };
    let result: anycon_protocol::RuleExtractRunsResult =
        conn.call(methods::RULE_EXTRACT_RUNS, json!({"session_id":session_id})).await?;
    if app.json() {
        app.out.json_line(&result);
        return Ok(0);
    }
    println!("today: {} / {} calls", result.used_today, result.daily_calls);
    let mut rows = vec![vec!["TIME", "SESSION", "TRIGGER", "STATUS", "ADDED", "DROPPED", "ERROR"]
        .into_iter()
        .map(str::to_owned)
        .collect()];
    for run in result.runs {
        let string = |v: Value| v.as_str().unwrap_or("unknown").to_owned();
        rows.push(vec![
            crate::output::ago(run.started_ms),
            short(&run.session_id),
            string(json!(run.trigger)),
            string(json!(run.status)),
            run.added.len().to_string(),
            (run.dropped.invalid + run.dropped.duplicate + run.dropped.full).to_string(),
            run.error.map(|e| string(json!(e.code))).unwrap_or_default(),
        ]);
    }
    print!("{}", table(&rows));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_line_rules_show_on_one_line_without_control_characters() {
        assert_eq!(one_line("first\nsecond \n\n third"), "first / second /  / third");
        assert_eq!(one_line("a\u{1b}[31mred"), "a[31mred");
        assert_eq!(one_line(&"x".repeat(300)).chars().count(), 100);
    }

    #[test]
    fn friendly_messages_replace_the_named_reasons_only() {
        let mut e = CliError::usage("rules: too_many");
        e.reason = Some("too_many".into());
        assert!(friendly(e).message.contains("300"));
        let mut e = CliError::usage("rules: too_many_pending");
        e.reason = Some("too_many_pending".into());
        assert!(friendly(e).message.contains("50"));
        let mut e = CliError::usage("text: over_budget");
        e.reason = Some("over_budget".into());
        assert!(friendly(e).message.contains("4096"));
        let mut e = CliError::usage("if_updated_ms: changed");
        e.reason = Some("changed".into());
        assert!(friendly(e).message.contains("anycon rule ls"));
        let mut e = CliError::usage("text: x");
        e.reason = None;
        assert_eq!(friendly(e).message, "text: x");
    }
}
