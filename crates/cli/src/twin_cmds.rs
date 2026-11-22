//! `anycon twin`：读取和设置分身范围、查看决定、回答问题；写操作在 anycon agent 环境中拒绝执行。

use anycon_protocol::*;
use serde_json::json;

use crate::cli::{TwinCmd, TwinSetArgs};
use crate::commands::App;
use crate::error::{CliError, Result};
use crate::output::{sanitize_label, visible};

fn write_allowed() -> Result<()> {
    if crate::mcp::started_inside_a_session() {
        return Err(CliError::denied(
            "forbidden",
            "twin write commands are not available inside an anycon agent session (ANYCON_AGENT is set); run them from your own terminal",
        ));
    }
    Ok(())
}

fn set_params(a: TwinSetArgs) -> TwinSetParams {
    let mut clear = vec![];
    if a.clear_agent {
        clear.push(ClearableTwinField::Agent);
    }
    if a.clear_model {
        clear.push(ClearableTwinField::Model);
    }
    if a.clear_mode {
        clear.push(ClearableTwinField::Mode);
    }
    TwinSetParams {
        paused: None,
        projects: a.projects.map(|s| s.split(',').filter(|p| !p.trim().is_empty()).map(str::to_string).collect()),
        agent: a.agent,
        model: a.model,
        mode: a.mode,
        max_sessions: a.max_sessions,
        daily_dispatches: a.daily_dispatches,
        clear,
    }
}

pub async fn twin(app: &App, cmd: TwinCmd) -> Result<u8> {
    if !matches!(cmd, TwinCmd::Status | TwinCmd::Runs | TwinCmd::Questions) {
        write_allowed()?;
    }
    let conn = app.conn().await?;
    match cmd {
        TwinCmd::Status => {
            let r: TwinGetResult = conn.call(methods::TWIN_GET, json!({})).await?;
            print_status(app, &r);
        }
        TwinCmd::Pause | TwinCmd::Resume => {
            let r: TwinGetResult = conn
                .call(
                    methods::TWIN_SET,
                    TwinSetParams { paused: Some(matches!(cmd, TwinCmd::Pause)), ..Default::default() },
                )
                .await?;
            print_status(app, &r);
        }
        TwinCmd::Set(a) => {
            let r: TwinGetResult = conn.call(methods::TWIN_SET, set_params(a)).await?;
            print_status(app, &r);
        }
        TwinCmd::Wake => {
            let r: Empty = conn.call(methods::TWIN_WAKE, json!({})).await?;
            if app.json() {
                app.out.json_line(&r);
            } else {
                println!("wake requested");
            }
        }
        TwinCmd::DryRun => {
            let r: TwinDryRunResult = conn.call_slow(methods::TWIN_DRY_RUN, json!({})).await?;
            if app.json() {
                app.out.json_line(&r);
            } else {
                print_run(&r.run);
            }
        }
        TwinCmd::Runs => {
            let r: TwinRunsResult = conn.call(methods::TWIN_RUNS, TwinRunsParams::default()).await?;
            if app.json() {
                app.out.json_line(&r);
            } else if r.runs.is_empty() {
                println!("no runs");
            } else {
                for run in r.runs {
                    print_run(&run);
                }
            }
        }
        TwinCmd::Questions => {
            let r: TwinQuestionsResult =
                conn.call(methods::TWIN_QUESTIONS, TwinQuestionsParams { open_only: Some(true) }).await?;
            if app.json() {
                app.out.json_line(&r);
            } else if r.questions.is_empty() {
                println!("no open questions");
            } else {
                for q in r.questions {
                    println!("{}  to-do {}\n    {}", q.id, q.todo_id, visible(&q.question).replace('\n', "\n    "));
                    for (i, o) in q.options.iter().enumerate() {
                        println!("    {}. {}", i + 1, sanitize_label(o, 100, ""));
                    }
                }
            }
        }
        TwinCmd::Answer { id, text } => {
            let r: TwinQuestion = conn.call(methods::TWIN_ANSWER, TwinAnswerParams { id, answer: text }).await?;
            if app.json() {
                app.out.json_line(&r);
            } else {
                println!("answered {}", r.id);
            }
        }
    }
    Ok(0)
}
fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}
fn print_status(app: &App, r: &TwinGetResult) {
    if app.json() {
        app.out.json_line(r);
        return;
    }
    println!("{}\nprojects: {}\nagent: {}  model: {}  mode: {}\nworking: {}/{}  dispatches today: {}/{}  judge calls today: {}\nwaiting: {}",label(r.state),sanitize_label(&r.scope.projects.join(", "),1000,""),sanitize_label(r.scope.agent.as_deref().unwrap_or("default"),100,""),sanitize_label(r.scope.model.as_deref().unwrap_or("default"),100,""),sanitize_label(r.scope.mode.as_deref().unwrap_or("unattended or default"),100,""),r.working.len(),r.scope.max_sessions,r.today.dispatches,r.scope.daily_dispatches,r.today.judge_calls,r.waiting.len());
}
fn print_run(r: &TwinRun) {
    println!("{}  {}{}", r.id, label(r.result), if r.dry_run { " (dry run)" } else { "" });
    if let Some(error) = &r.error {
        println!("    error: {}", label(error.code));
    }
    for d in &r.decisions {
        println!(
            "    {}  {}  {}{}\n        {}",
            sanitize_label(&d.todo_id, 80, "?"),
            label(d.action),
            label(d.outcome),
            d.skip_reason.map(|s| format!(" ({})", label(s))).unwrap_or_default(),
            visible(&d.reason).replace('\n', "\n        ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    #[test]
    fn all_commands_parse_and_set_clears_defaults() {
        for args in [
            vec!["status"],
            vec!["pause"],
            vec!["resume"],
            vec!["wake"],
            vec!["dry-run"],
            vec!["runs"],
            vec!["runs", "--json"],
            vec!["questions"],
            vec!["answer", "q", "Use project a"],
        ] {
            let mut argv = vec!["anycon", "twin"];
            argv.extend(args);
            assert!(Cli::try_parse_from(argv).is_ok());
        }
        let cli = Cli::try_parse_from([
            "anycon",
            "twin",
            "set",
            "--projects",
            "a,b",
            "--agent",
            "fake",
            "--model",
            "m",
            "--mode",
            "auto",
            "--max-sessions",
            "1",
            "--daily-dispatches",
            "3",
            "--clear-agent",
            "--clear-model",
            "--clear-mode",
        ])
        .unwrap();
        let Command::Twin(TwinCmd::Set(a)) = cli.command else {
            panic!("set");
        };
        let p = set_params(a);
        assert_eq!(p.projects, Some(vec!["a".into(), "b".into()]));
        assert_eq!(p.clear.len(), 3);
        assert_eq!(p.max_sessions, Some(1));
    }
}
