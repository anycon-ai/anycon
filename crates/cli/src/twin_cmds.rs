//! `anycon twin`：读取和设置 Twin 范围、查看决定、回答问题；写操作在 anycon agent 环境中拒绝执行。

use anycon_protocol::*;
use serde_json::json;

use crate::cli::{TwinCmd, TwinSetArgs};
use crate::commands::App;
use crate::conn::Conn;
use crate::error::{CliError, Result};
use crate::output::{sanitize_label, visible};

fn write_allowed() -> Result<()> {
    if crate::mcp::started_inside_a_session() {
        return Err(CliError::denied(
            "forbidden",
            "twin write commands (`anycon twin ...`) are not available inside an anycon agent session (ANYCON_AGENT is set); \
             run them from your own terminal",
        ));
    }
    Ok(())
}

/// `--requester`：一台 Twin 设备的设备 id、唯一的 id 前缀或名称 → 完整的设备 id。只在种类是 `twin` 的设备里找。
async fn requester_id(conn: &Conn, key: &str) -> Result<String> {
    let devices: NodeDevicesResult = conn.call(methods::NODE_DEVICES, json!({})).await?;
    let twins: Vec<&PeerDevice> = devices.devices.iter().filter(|d| d.kind == DeviceKind::Twin).collect();
    let pick = |matches: Vec<&&PeerDevice>| (matches.len() == 1).then(|| matches[0].device_id.clone());
    twins
        .iter()
        .find(|d| d.device_id == key)
        .map(|d| d.device_id.clone())
        .or_else(|| pick(twins.iter().filter(|d| !key.is_empty() && d.device_id.starts_with(key)).collect()))
        .or_else(|| pick(twins.iter().filter(|d| d.name == key).collect()))
        .ok_or_else(|| {
            CliError::usage(format!(
                "no twin device matches {}; `anycon devices` lists this node's paired devices",
                sanitize_label(key, 100, "")
            ))
        })
}

fn set_params(a: TwinSetArgs, requester: Option<String>) -> TwinSetParams {
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
        requester,
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
    if !matches!(cmd, TwinCmd::Status(_) | TwinCmd::Runs | TwinCmd::Questions | TwinCmd::Links) {
        write_allowed()?;
    }
    let conn = app.conn().await?;
    match cmd {
        TwinCmd::Status(a) => {
            let requester = match &a.requester {
                Some(key) => Some(requester_id(&conn, key).await?),
                None => None,
            };
            let local = requester.is_none();
            let r: TwinGetResult = conn.call(methods::TWIN_GET, TwinGetParams { requester }).await?;
            print_status(app, &r, view(app, &conn, local).await);
        }
        TwinCmd::LinkCode => {
            let r: NodeTwinLinkCodeResult = match conn.call(methods::NODE_TWIN_LINK_CODE, json!({})).await {
                Ok(r) => r,
                // `unsupported`：节点没连上 hub，短码没处登记
                Err(e) if e.code == "unsupported" => return Err(CliError::offline(e.message)),
                Err(e) => return Err(e),
            };
            if app.json() {
                app.out.json_line(&r);
            } else {
                println!("Twin link code: {}", app.out.bold(&r.code));
                println!(
                    "Valid until {} UTC (10 minutes). Only another node's twin can use it; pairing with it adds a twin \
                     device here, which you can scope with `anycon twin set --requester <device>`.",
                    anycon_daemon::util::fmt_utc(r.expires_ms)
                );
            }
        }
        TwinCmd::Link { code, peer } => {
            let r: TwinLink = match conn.call_slow(methods::TWIN_LINK, TwinLinkParams { code, node_id: peer }).await {
                Ok(r) => r,
                // 这个节点没有配置 hub、或者连不上 hub：短码没处用
                Err(e) if matches!(e.reason.as_deref(), Some("no_hub" | "offline")) => {
                    return Err(CliError::offline(e.message))
                }
                Err(e) => return Err(e),
            };
            if app.json() {
                app.out.json_line(&r);
            } else {
                println!(
                    "Linked {} ({}). It has not opened anything to this twin yet: on that node, run `anycon twin set \
                     --requester <device> --projects <names>` (`anycon devices` lists the twin device).",
                    sanitize_label(&r.name, 64, "node"),
                    short_id(&r.node_id)
                );
            }
        }
        TwinCmd::Links => {
            let r: TwinLinksResult = conn.call(methods::TWIN_LINKS, json!({})).await?;
            if app.json() {
                app.out.json_line(&r);
            } else if r.links.is_empty() {
                println!("no linked nodes");
            } else {
                for l in &r.links {
                    print_link(l);
                }
            }
        }
        TwinCmd::Unlink { target } => {
            let links: TwinLinksResult = conn.call(methods::TWIN_LINKS, json!({})).await?;
            let l = pick_link(&links.links, &target)?;
            let _: Empty = conn.call(methods::TWIN_UNLINK, TwinUnlinkParams { node_id: l.node_id.clone() }).await?;
            if app.json() {
                app.out.json_line(&json!({ "unlinked": l.node_id }));
            } else {
                println!(
                    "Unlinked {} ({}). The twin device stays paired on that node until you revoke it there \
                     (`anycon revoke` on that node, or its device list).",
                    sanitize_label(&l.name, 64, "node"),
                    short_id(&l.node_id)
                );
            }
        }
        TwinCmd::Pause | TwinCmd::Resume => {
            let r: TwinGetResult = conn
                .call(
                    methods::TWIN_SET,
                    TwinSetParams { paused: Some(matches!(cmd, TwinCmd::Pause)), ..Default::default() },
                )
                .await?;
            print_status(app, &r, view(app, &conn, true).await);
        }
        TwinCmd::Set(a) => {
            let requester = match &a.requester {
                Some(key) => Some(requester_id(&conn, key).await?),
                None => None,
            };
            let local = requester.is_none();
            let r: TwinGetResult = conn.call(methods::TWIN_SET, set_params(a, requester)).await?;
            print_status(app, &r, view(app, &conn, local).await);
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
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `<node>`：连接的节点 id、唯一的 id 前缀或名称（名称不唯一就报错，让用户改用 id）。
fn pick_link<'a>(links: &'a [TwinLink], key: &str) -> Result<&'a TwinLink> {
    let by = |f: &dyn Fn(&TwinLink) -> bool| -> Vec<&'a TwinLink> { links.iter().filter(|l| f(l)).collect() };
    let exact = by(&|l| l.node_id == key);
    let prefix = by(&|l| !key.is_empty() && l.node_id.starts_with(key));
    let name = by(&|l| l.name == key);
    [exact, prefix, name].into_iter().find(|m| m.len() == 1).map(|m| m[0]).ok_or_else(|| {
        CliError::usage(format!(
            "no linked node matches {}; `anycon twin links` lists the nodes this twin uses",
            sanitize_label(key, 100, "")
        ))
    })
}

fn print_link(l: &TwinLink) {
    let mut line = format!(
        "{}  {}  {}",
        short_id(&l.node_id),
        sanitize_label(&l.name, 64, "?"),
        if l.online { "online" } else { "offline" }
    );
    if let Some(s) = &l.status {
        line.push_str(&format!(
            "\n    projects: {}\n    free sessions: {}  free dispatches today: {}{}",
            if s.projects.is_empty() {
                "none opened".to_string()
            } else {
                sanitize_label(&s.projects.join(", "), 1000, "")
            },
            s.remaining_sessions,
            s.remaining_dispatches,
            if s.paused { "  (paused by that node)" } else { "" }
        ));
    }
    println!("{line}");
}

fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}
/// 状态输出里运行状态那几行（唤醒、排队）怎么打印。
#[derive(Clone, Copy, PartialEq)]
enum View {
    /// Twin 设备的范围没有运行记录和排队，不打印。
    Scope,
    /// 本机 Twin，节点报告了 `capabilities.twin_stats`，数字可信。
    Full,
    /// 本机 Twin，但旧 daemon 不填这些字段：打印 `-`，不把「没有数据」当成「从没唤醒、没有排队」。
    Unknown,
}

async fn view(app: &App, conn: &Conn, local: bool) -> View {
    if !local {
        return View::Scope;
    }
    if app.json() {
        return View::Full;
    }
    // 节点信息拿不到（老 daemon）就当没有这个能力
    let info: Option<NodeInfo> = conn.call(methods::NODE_INFO, json!({})).await.ok();
    if info.is_some_and(|i| i.capabilities.twin_stats) {
        View::Full
    } else {
        View::Unknown
    }
}

fn print_status(app: &App, r: &TwinGetResult, view: View) {
    if app.json() {
        app.out.json_line(r);
        return;
    }
    println!(
        "{}\nprojects: {}\nagent: {}  model: {}  mode: {}\n\
         working: {}/{}  dispatches today: {}/{}  judge calls today: {}\nwaiting: {}",
        label(r.state),
        sanitize_label(&r.scope.projects.join(", "), 1000, ""),
        sanitize_label(r.scope.agent.as_deref().unwrap_or("default"), 100, ""),
        sanitize_label(r.scope.model.as_deref().unwrap_or("default"), 100, ""),
        sanitize_label(r.scope.mode.as_deref().unwrap_or("unattended or default"), 100, ""),
        r.working.len(),
        r.scope.max_sessions,
        r.today.dispatches,
        r.scope.daily_dispatches,
        r.today.judge_calls,
        r.waiting.len(),
    );
    if view == View::Unknown {
        println!("last wake: -\nnext wake: -\nqueued: -");
    }
    if view == View::Full {
        match &r.last_run {
            Some(run) => println!(
                "last wake: {} ({}{})",
                crate::output::ago(run.started_ms),
                label(run.result),
                if run.finished_ms.is_none() { ", still running" } else { "" }
            ),
            None => println!("last wake: never"),
        }
        match r.next_wake_ms {
            Some(at) => println!("next wake: {} UTC", anycon_daemon::util::fmt_utc(at)),
            None => println!("next wake: -"),
        }
        println!("queued: {}", r.queued.len());
    }
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
            vec!["status", "--requester", "dev1"],
            vec!["link-code"],
            vec!["link", "ABCD-ABCD-ABCD-ABCD", "--peer", "n1"],
            vec!["links"],
            vec!["links", "--json"],
            vec!["unlink", "laptop"],
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
        let p = set_params(a, None);
        assert_eq!(p.projects, Some(vec!["a".into(), "b".into()]));
        assert_eq!(p.clear.len(), 3);
        assert_eq!(p.max_sessions, Some(1));
    }
}
