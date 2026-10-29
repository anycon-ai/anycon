//! clap 定义：全部子命令（docs/spec/cli.md § 3）。
//!
//! 这里的 `///` 文档注释就是 `--help` 的文字，面向用户，用英文写；其余注释照常用中文。

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "anycon",
    version,
    about = "Control your coding agents from anywhere: daemon + CLI in one binary",
    propagate_version = true
)]
pub struct Cli {
    /// Machine-readable output (streaming commands print NDJSON, one event per line)
    #[arg(long, global = true)]
    pub json: bool,
    /// Disable colors (setting NO_COLOR also works)
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Hub URL to use when there is more than one hub
    #[arg(long, global = true, value_name = "URL")]
    pub hub: Option<String>,
    /// Node to use when there are several (name or id prefix)
    #[arg(long, global = true, value_name = "NODE")]
    pub node: Option<String>,
    /// Always use the local daemon's socket (a named pipe on Windows)
    #[arg(long, global = true)]
    pub local: bool,
    /// How long to wait for each request, in seconds
    #[arg(long, global = true, value_name = "SECS", default_value_t = 30)]
    pub timeout: u64,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Generate the config and Noise keys, and create the workspace directory
    Init(InitArgs),
    /// Run the daemon in the foreground (leave backgrounding to launchd / systemd / the Windows scheduled task)
    Daemon(DaemonArgs),
    /// Install and manage the user-level service (launchd / systemd user unit / Windows scheduled task)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Have the daemon start pairing and print a pairing code (`--enroll`: a 6-digit join code for a new device signed
    /// in to the same account)
    Pair(PairArgs),
    /// Node password: lets a new device unlock this node when no paired device is at hand (local socket)
    Passwd(PasswdArgs),
    /// List paired client devices
    Devices,
    /// Revoke a device (name or id prefix)
    Revoke { device: String },
    /// Rename a device
    RenameDevice { device: String, name: String },
    /// Run diagnostic checks one by one
    Doctor,
    /// Local agents: status, version, models, permission modes
    Agents(AgentsArgs),
    /// Projects (folders in the workspace)
    Projects(ProjectsArgs),
    /// List sessions
    Ls(LsArgs),
    /// Start a session and send its first message
    New(NewArgs),
    /// Send another message to a session
    Send(SendArgs),
    /// Follow a session's event stream (Ctrl+C detaches; it does not cancel)
    Attach(AttachArgs),
    /// Interrupt the session's current turn
    Cancel { session: String },
    /// List permission requests waiting for approval
    Pending,
    /// Approve a permission request
    Approve(ApproveArgs),
    /// Deny a permission request
    Deny(DenyArgs),
    /// Print a session's history
    History(HistoryArgs),
    /// Change a session's title
    Rename { session: String, title: String },
    /// Archive a session (for a session in its own worktree, the worktree is removed by default only if it has no
    /// uncommitted changes)
    Archive(ArchiveArgs),
    /// Unarchive a session (a removed worktree is recreated from its branch)
    Unarchive { session: String },
    /// Delete a session with its history and scratch folder (its worktree is removed by default only if it has no
    /// uncommitted changes)
    Delete(DeleteArgs),
    /// Show a session's changes: against its base (default) or uncommitted only
    Diff(DiffArgs),
    /// Merge new commits from the base into the session's worktree branch
    Sync(SyncArgs),
    /// List the worktrees anycon created, or remove one
    Worktrees(WorktreesArgs),
    /// Full-text search across sessions
    Search(SearchArgs),
    /// Turn a scratch session into a project
    Promote { session: String, project_name: String },
    /// Show the local audit log
    Audit(AuditArgs),
    /// Read and write config.toml
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Show the daemon's logs
    Logs(LogsArgs),
    /// Show runtime statistics
    Stats,
    /// Show version information
    Version,
    /// Generate shell completions
    Completions { shell: clap_complete::Shell },
    /// Pair this machine's CLI with a daemon as a client device
    Connect(ConnectArgs),
    /// List the nodes this CLI is paired with
    Nodes,
    /// Forget this machine's pairing with a node
    Disconnect { node: String },
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Hub URL, e.g. `https://hub.example.com`
    #[arg(long)]
    pub hub: Option<String>,
    /// Workspace root directory (default: ~/anycon)
    #[arg(long)]
    pub workspace: Option<String>,
    /// Node display name (default: the hostname)
    #[arg(long)]
    pub name: Option<String>,
    /// Don't ask; use the defaults for everything not given
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Initialize again if already initialized (every paired device stops working)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Run in the foreground (already the default; kept for service units)
    #[arg(long)]
    pub foreground: bool,
    /// Data directory, instead of ANYCON_HOME (used by the Windows scheduled task, which cannot set environment variables)
    #[arg(long, value_name = "DIR", hide = true)]
    pub home: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

#[derive(Args, Debug)]
pub struct PairArgs {
    /// Render a QR code in the terminal
    #[arg(long)]
    pub qr: bool,
    /// Name to give the new device once paired (renames it; defaults to the name the device reports)
    #[arg(long)]
    pub name: Option<String>,
    /// Return as soon as the code is printed instead of waiting for pairing to finish
    #[arg(long)]
    pub no_wait: bool,
    /// 16-character code (about 79 bits). This is the default; the flag is a no-op kept for old scripts
    #[arg(long, conflicts_with = "short")]
    pub long: bool,
    /// 8-character code (about 40 bits, easier to type). Only for a hub you run yourself: a malicious hub run by
    /// someone else can brute-force it offline
    #[arg(long)]
    pub short: bool,
    /// Create a 6-digit join code (the web UI calls it an access code) for a new device already signed in to the same
    /// account. It is valid for 5 minutes and 3 tries in total, and a new code replaces the old one. On the new device,
    /// choose "I have an access code" on the web unlock page, or run `anycon connect <hub> --via code`. Can't be
    /// combined with `--short` / `--long` / `--qr`
    #[arg(long, conflicts_with_all = ["short", "long", "qr"])]
    pub enroll: bool,
}

/// `anycon passwd`: manage the node password (over the local socket). With no action flag it sets or changes the
/// password (typed twice, not echoed); the password is never taken as a command-line argument, so it stays out of
/// shell history.
#[derive(Args, Debug)]
pub struct PasswdArgs {
    /// Read the password as one line from stdin (required when stdin is not a terminal)
    #[arg(long, conflicts_with_all = ["clear", "enable", "disable", "status"])]
    pub stdin: bool,
    /// Remove the password; password unlock stops working
    #[arg(long, group = "action")]
    pub clear: bool,
    /// Turn password unlock on. If it is locked out, print the recent failures first, then on confirmation lift the
    /// lockout and reset the failure count
    #[arg(long, group = "action")]
    pub enable: bool,
    /// Turn password unlock off (the password is kept)
    #[arg(long, group = "action")]
    pub disable: bool,
    /// Show whether a password is set, its state (enabled / disabled / locked), consecutive failures, and when the next
    /// attempt is allowed
    #[arg(long, group = "action")]
    pub status: bool,
    /// Don't ask for confirmation (`--clear`, or `--enable` lifting a lockout)
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// How joining with your account (`anycon connect <hub-url>` without `--code`) unlocks the node.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViaArg {
    /// Draw a QR code in the terminal; scan and approve it in the web UI on a paired phone
    Scan,
    /// Ask a paired device for approval, then type the 6-digit code it shows
    Approve,
    /// Type a join code from a paired device's "Add device" or from `anycon pair --enroll`
    Code,
    /// Type the node password (set with `anycon passwd`)
    Password,
}

#[derive(Args, Debug)]
pub struct AgentsArgs {
    /// Probe again, including the model list
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Args, Debug)]
pub struct ProjectsArgs {
    #[command(subcommand)]
    pub cmd: Option<ProjectsCmd>,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCmd {
    /// Create an empty project
    Create {
        name: String,
        /// Also run git init
        #[arg(long)]
        git: bool,
    },
    /// Clone a repository (streams progress)
    Clone {
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Move a project to the trash (a linked directory can't be trashed; use `unlink`)
    Trash {
        name: String,
    },
    /// Link an existing local directory as a project (nothing is moved or copied; the directory must be inside the
    /// node's `workspace.link_roots`)
    Link {
        /// Directory. A relative path is resolved against the current directory before sending (sent as-is when
        /// `--node` points to a remote node; the node expands `~`)
        path: String,
        /// Project name (default: the directory name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Unlink a project: remove it from the project list without deleting the directory or its files
    Unlink {
        name: String,
    },
    /// List directories that can be linked (subdirectories inside `workspace.link_roots`)
    Browse {
        /// Directory to list (default: the first entry of link_roots)
        path: Option<String>,
        /// Also list directories whose names start with `.`
        #[arg(long)]
        all: bool,
    },
}

#[derive(Args, Debug)]
pub struct LsArgs {
    #[arg(long)]
    pub project: Option<String>,
    /// running | waiting | idle | ended | error | queued
    #[arg(long)]
    pub status: Option<String>,
    /// List everything (archived sessions included, no limit on the count)
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    /// Project name; omit it for a scratch session
    #[arg(long)]
    pub project: Option<String>,
    /// Agent id (as listed by `anycon agents`); defaults to the only ready agent
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub thinking: Option<String>,
    #[arg(long)]
    pub mode: Option<String>,
    /// Agent-specific option as key=value (the value is parsed as JSON, or taken as a string if that fails)
    #[arg(short = 'o', long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,
    /// Work in a new git worktree and branch (the project must be a repository root with at least one commit)
    #[arg(long, conflicts_with = "here")]
    pub worktree: bool,
    /// Work in the project directory itself
    #[arg(long, conflicts_with_all = ["base", "branch"])]
    pub here: bool,
    /// Ref to start the worktree's branch from (a branch, `origin/main`, or a commit); implies --worktree
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    /// Name of the worktree's branch (must not exist yet); implies --worktree
    #[arg(long, value_name = "NAME")]
    pub branch: Option<String>,
    /// Print only the session id; don't follow the output
    #[arg(long)]
    pub no_wait: bool,
    /// Approve every permission request automatically (dangerous)
    #[arg(long)]
    pub auto_approve: bool,
    /// Show the agent's thinking
    #[arg(long)]
    pub show_thinking: bool,
    pub prompt: String,
}

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    /// Session id (a prefix is enough) or `last`
    pub session: String,
    /// Keep the worktree, even if it is clean
    #[arg(long, conflicts_with = "remove_worktree")]
    pub keep_worktree: bool,
    /// Remove the worktree and discard uncommitted changes (asks first in an interactive terminal; needs --yes
    /// otherwise)
    #[arg(long)]
    pub remove_worktree: bool,
    /// Don't ask for confirmation
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Session id (a prefix is enough) or `last`
    pub session: String,
    /// Keep the worktree (it becomes a worktree with no session; clean it up later with `anycon worktrees remove`)
    #[arg(long, conflicts_with = "remove_worktree")]
    pub keep_worktree: bool,
    /// Remove the worktree and discard uncommitted changes (asks first in an interactive terminal; needs --yes
    /// otherwise)
    #[arg(long)]
    pub remove_worktree: bool,
    /// Once the worktree is removed, delete its branch too (unmerged commits on it are lost; asks first in an
    /// interactive terminal, needs --yes otherwise)
    #[arg(long)]
    pub delete_branch: bool,
    /// Don't ask for confirmation
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Session id (a prefix is enough) or `last`
    pub session: String,
    /// Show only uncommitted changes (always the case for a session working in the project directory itself)
    #[arg(long)]
    pub uncommitted: bool,
    /// List only the files and `+N −M`; don't print the patch
    #[arg(long)]
    pub stat: bool,
    /// Limit to these paths (files or directory prefixes)
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Session id (a prefix is enough) or `last`
    pub session: String,
    /// Abort a merge that ran into conflicts
    #[arg(long)]
    pub abort: bool,
}

#[derive(Args, Debug)]
pub struct WorktreesArgs {
    /// Only list this project's worktrees
    #[arg(long)]
    pub project: Option<String>,
    #[command(subcommand)]
    pub cmd: Option<WorktreesCmd>,
}

#[derive(Subcommand, Debug)]
pub enum WorktreesCmd {
    /// Remove a kept or session-less worktree (not allowed while its session is not archived)
    Remove {
        /// Worktree id (a prefix is enough; `wt_` can be left out)
        id: String,
        /// Remove it even with uncommitted changes, discarding them (asks first in an interactive terminal; needs --yes
        /// otherwise)
        #[arg(long)]
        force: bool,
        /// Delete the branch too (unmerged commits on it are lost; asks first in an interactive terminal, needs --yes
        /// otherwise)
        #[arg(long)]
        delete_branch: bool,
        /// Don't ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct SendArgs {
    /// Session id (a prefix is enough) or `last`
    pub session: String,
    /// Message text; can be left out when --image is given
    #[arg(required_unless_present = "images")]
    pub text: Option<String>,
    /// Attach an image (PNG / JPEG / GIF / WebP; repeatable, at most 20 per message and 20 MiB in total; not resized)
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<std::path::PathBuf>,
    #[arg(long)]
    pub no_wait: bool,
    #[arg(long)]
    pub auto_approve: bool,
    #[arg(long)]
    pub show_thinking: bool,
}

#[derive(Args, Debug)]
pub struct AttachArgs {
    pub session: String,
    #[arg(long)]
    pub auto_approve: bool,
    #[arg(long)]
    pub show_thinking: bool,
    /// First replay the history after this seq
    #[arg(long, value_name = "SEQ")]
    pub since: Option<u64>,
}

#[derive(Args, Debug)]
pub struct ApproveArgs {
    pub request_id: String,
    /// Allow for the rest of the session (picks an allow_always-style option)
    #[arg(long)]
    pub session: bool,
}

#[derive(Args, Debug)]
pub struct DenyArgs {
    pub request_id: String,
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    pub session: String,
    #[arg(long, value_name = "SEQ", default_value_t = 0)]
    pub since: u64,
    #[arg(long)]
    pub show_thinking: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Args, Debug)]
pub struct AuditArgs {
    /// How far back to look, e.g. 1h or 2d (or a Unix timestamp in milliseconds)
    #[arg(long)]
    pub since: Option<String>,
    /// Device id
    #[arg(long)]
    pub device: Option<String>,
    #[arg(long, short = 'n', default_value_t = 100)]
    pub limit: u32,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Read a key, e.g. sessions.max_concurrent
    Get { key: String },
    /// Write a key; hot-reloadable keys take effect immediately, otherwise you are told to restart
    Set { key: String, value: String },
    /// Open config.toml in $EDITOR
    Edit,
    /// Print the path of config.toml
    Path,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Keep following new lines
    #[arg(short = 'f', long)]
    pub follow: bool,
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
}

#[derive(Args, Debug)]
pub struct ConnectArgs {
    pub hub_url: String,
    /// Pairing code, or the pairing link printed by `anycon pair` or the web UI (`…/pair#code=XXXX-XXXX`). Without it
    /// you
    /// join with your account: sign in, then unlock one of the account's nodes (`--node`, `--via`, `--email`); see
    /// docs/spec/cli.md § 3.3
    #[arg(long, value_name = "CODE-OR-LINK", conflicts_with_all = ["via", "email"])]
    pub code: Option<String>,
    /// Joining with your account: how to unlock the node (omit it to choose interactively)
    #[arg(long, value_enum)]
    pub via: Option<ViaArg>,
    /// Joining with your account: the email address for signing in with an email code (omit it to be prompted)
    #[arg(long, value_name = "ADDR")]
    pub email: Option<String>,
    /// Name for this device (default: the hostname)
    #[arg(long)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["anycon"];
        v.extend(args);
        Cli::try_parse_from(v)
    }

    #[test]
    fn new_takes_a_working_place_and_rejects_contradictions() {
        let ok = parse(&["new", "--project", "p", "--worktree", "--base", "origin/main", "--branch", "x/y", "do it"])
            .unwrap();
        let Command::New(a) = ok.command else { panic!() };
        assert!(a.worktree && !a.here);
        assert_eq!((a.base.as_deref(), a.branch.as_deref()), (Some("origin/main"), Some("x/y")));
        let Command::New(a) = parse(&["new", "--here", "hi"]).unwrap().command else { panic!() };
        assert!(a.here);
        // --worktree 和 --here 互斥；--base / --branch 和 --here 互斥
        assert!(parse(&["new", "--worktree", "--here", "hi"]).is_err());
        assert!(parse(&["new", "--here", "--base", "main", "hi"]).is_err());
        assert!(parse(&["new", "--here", "--branch", "x", "hi"]).is_err());
    }

    #[test]
    fn passwd_takes_one_action_and_never_a_password_argument() {
        let Command::Passwd(a) = parse(&["passwd"]).unwrap().command else { panic!() };
        assert!(!a.stdin && !a.clear && !a.enable && !a.disable && !a.status && !a.yes);
        let Command::Passwd(a) = parse(&["passwd", "--stdin"]).unwrap().command else { panic!() };
        assert!(a.stdin);
        let Command::Passwd(a) = parse(&["passwd", "--clear", "--yes"]).unwrap().command else { panic!() };
        assert!(a.clear && a.yes);
        let Command::Passwd(a) = parse(&["passwd", "--status", "--json"]).unwrap().command else { panic!() };
        assert!(a.status);
        assert!(parse(&["--json", "passwd", "--status"]).unwrap().json);
        // 动作互斥；--stdin 只用于设置；口令不能写在命令行上
        assert!(parse(&["passwd", "--clear", "--enable"]).is_err());
        assert!(parse(&["passwd", "--enable", "--disable"]).is_err());
        assert!(parse(&["passwd", "--stdin", "--clear"]).is_err());
        assert!(parse(&["passwd", "--stdin", "--status"]).is_err());
        assert!(parse(&["passwd", "hunter2hunter2"]).is_err(), "no positional password");
        assert!(parse(&["passwd", "--password", "x"]).is_err());
    }

    #[test]
    fn pair_enroll_does_not_mix_with_the_short_code_options() {
        let Command::Pair(a) = parse(&["pair", "--enroll", "--no-wait", "--json"]).unwrap().command else { panic!() };
        assert!(a.enroll && a.no_wait);
        assert!(parse(&["pair", "--enroll", "--short"]).is_err());
        assert!(parse(&["pair", "--enroll", "--long"]).is_err());
        assert!(parse(&["pair", "--enroll", "--qr"]).is_err());
        let Command::Pair(a) = parse(&["pair", "--short"]).unwrap().command else { panic!() };
        assert!(!a.enroll && a.short);
    }

    #[test]
    fn connect_is_either_a_short_code_pairing_or_an_account_enrollment() {
        let Command::Connect(a) = parse(&["connect", "https://hub", "--code", "ABCD-EFGH"]).unwrap().command else {
            panic!()
        };
        assert_eq!((a.code.as_deref(), a.via), (Some("ABCD-EFGH"), None));
        let c =
            parse(&["connect", "https://hub", "--node", "mac-mini", "--via", "password", "--email", "a@b.c"]).unwrap();
        assert_eq!(c.node.as_deref(), Some("mac-mini"), "--node is the global option");
        let Command::Connect(a) = c.command else { panic!() };
        assert_eq!((a.code, a.via, a.email.as_deref()), (None, Some(ViaArg::Password), Some("a@b.c")));
        for via in ["scan", "approve", "code", "password"] {
            assert!(parse(&["connect", "https://hub", "--via", via]).is_ok(), "{via}");
        }
        assert!(parse(&["connect", "https://hub", "--via", "magic"]).is_err());
        // 短码配对和账号内接入的选项不能混着用
        assert!(parse(&["connect", "https://hub", "--code", "X", "--via", "scan"]).is_err());
        assert!(parse(&["connect", "https://hub", "--code", "X", "--email", "a@b.c"]).is_err());
    }

    #[test]
    fn archive_and_delete_take_a_disposal_and_a_confirmation() {
        let Command::Archive(a) = parse(&["archive", "last", "--remove-worktree", "--yes"]).unwrap().command else {
            panic!()
        };
        assert!(a.remove_worktree && a.yes && !a.keep_worktree);
        let Command::Delete(d) = parse(&["delete", "abc", "--keep-worktree", "--delete-branch"]).unwrap().command
        else {
            panic!()
        };
        assert!(d.keep_worktree && d.delete_branch && !d.remove_worktree);
        assert!(parse(&["archive", "x", "--keep-worktree", "--remove-worktree"]).is_err());
        assert!(parse(&["delete", "x", "--keep-worktree", "--remove-worktree"]).is_err());
        // 旧写法照常
        assert!(matches!(parse(&["archive", "x"]).unwrap().command, Command::Archive(_)));
        assert!(matches!(parse(&["unarchive", "x"]).unwrap().command, Command::Unarchive { .. }));
    }

    #[test]
    fn diff_sync_and_worktrees_parse() {
        let Command::Diff(d) = parse(&["diff", "last", "--uncommitted", "--stat", "src", "a.rs"]).unwrap().command
        else {
            panic!()
        };
        assert!(d.uncommitted && d.stat);
        assert_eq!(d.paths, vec!["src".to_string(), "a.rs".to_string()]);
        let Command::Sync(s) = parse(&["sync", "x", "--abort"]).unwrap().command else { panic!() };
        assert!(s.abort);
        let Command::Worktrees(w) = parse(&["worktrees", "--project", "p"]).unwrap().command else { panic!() };
        assert_eq!(w.project.as_deref(), Some("p"));
        assert!(w.cmd.is_none());
        let Command::Worktrees(w) =
            parse(&["worktrees", "remove", "wt_abc", "--force", "--delete-branch"]).unwrap().command
        else {
            panic!()
        };
        assert!(matches!(w.cmd, Some(WorktreesCmd::Remove { force: true, delete_branch: true, yes: false, .. })));
        let Command::Worktrees(w) = parse(&["worktrees", "remove", "wt_abc", "--delete-branch", "-y"]).unwrap().command
        else {
            panic!()
        };
        assert!(matches!(w.cmd, Some(WorktreesCmd::Remove { delete_branch: true, yes: true, .. })));
    }
}
