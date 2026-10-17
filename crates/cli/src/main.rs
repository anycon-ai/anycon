//! `anycon`：daemon 和 CLI 合一的二进制。子命令见 `cli.rs`，实现在 `commands.rs`。

mod attach;
mod cli;
mod commands;
mod conn;
mod enroll;
#[cfg(test)]
mod enroll_tests;
mod error;
mod follow;
mod output;
mod passwd;
mod remote;
mod render;
mod service;
mod worktree_cmds;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::commands::App;
use crate::error::{CliError, Result};

async fn run(cli: Cli) -> Result<u8> {
    let app = App {
        out: output::Out::new(cli.json, cli.no_color),
        hub: cli.hub,
        node: cli.node,
        local: cli.local,
        timeout: std::time::Duration::from_secs(cli.timeout.max(1)),
        paths: anycon_daemon::config::Paths::from_env(),
    };
    match cli.command {
        Command::Init(a) => commands::init(&app, a).await,
        Command::Daemon(a) => commands::daemon(&app, a).await,
        Command::Service(c) => commands::service(&app, c).await,
        Command::Pair(a) => commands::pair(&app, a).await,
        Command::Passwd(a) => passwd::passwd(&app, a).await,
        Command::Devices => commands::devices(&app).await,
        Command::Revoke { device } => commands::revoke(&app, device).await,
        Command::RenameDevice { device, name } => commands::rename_device(&app, device, name).await,
        Command::Doctor => commands::doctor(&app).await,
        Command::Agents(a) => commands::agents(&app, a).await,
        Command::Projects(a) => commands::projects(&app, a).await,
        Command::Ls(a) => commands::ls(&app, a).await,
        Command::New(a) => commands::new_session(&app, a).await,
        Command::Send(a) => commands::send(&app, a).await,
        Command::Attach(a) => commands::attach(&app, a).await,
        Command::Cancel { session } => commands::cancel(&app, session).await,
        Command::Pending => commands::pending(&app).await,
        Command::Approve(a) => commands::approve(&app, a).await,
        Command::Deny(a) => commands::deny(&app, a).await,
        Command::History(a) => commands::history(&app, a).await,
        Command::Rename { session, title } => commands::rename_session(&app, session, title).await,
        Command::Archive(a) => worktree_cmds::archive(&app, a).await,
        Command::Unarchive { session } => worktree_cmds::unarchive(&app, session).await,
        Command::Delete(a) => worktree_cmds::delete(&app, a).await,
        Command::Diff(a) => worktree_cmds::diff(&app, a).await,
        Command::Sync(a) => worktree_cmds::sync(&app, a).await,
        Command::Worktrees(a) => worktree_cmds::worktrees(&app, a).await,
        Command::Search(a) => commands::search(&app, a).await,
        Command::Promote { session, project_name } => commands::promote(&app, session, project_name).await,
        Command::Audit(a) => commands::audit(&app, a).await,
        Command::Config(c) => commands::config(&app, c).await,
        Command::Logs(a) => commands::logs(&app, a).await,
        Command::Stats => commands::stats(&app).await,
        Command::Version => commands::version(&app),
        Command::Completions { shell } => commands::completions(shell),
        Command::Connect(a) => commands::connect(&app, a).await,
        Command::Nodes => commands::nodes(&app).await,
        Command::Disconnect { node } => commands::disconnect(&app, node).await,
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // 不用 #[tokio::main]：交互提示用的 stdin 读取跑在阻塞线程上，终端里它会一直卡在 read；
    // 运行时 drop 时默认会等阻塞线程结束，导致命令跑完了进程却不退出。所以结束时 `shutdown_background`。
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("cannot start the async runtime");
    let result = rt.block_on(run(cli));
    rt.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(CliError { exit, code, message }) => {
            if exit != error::EXIT_INTERRUPTED {
                eprintln!("error: {code}: {message}");
            }
            ExitCode::from(exit)
        }
    }
}
