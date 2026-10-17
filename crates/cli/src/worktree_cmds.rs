//! 每会话 worktree 与 git 相关的子命令（docs/spec/cli.md § 3.5，ADR-34）：`new` 的工作位置、`archive` / `delete` 的处置方式、
//! `unarchive`、`diff`、`sync`、`worktrees`。
//!
//! 来自仓库和 agent 的文本（路径、分支名、diff 内容、冲突文件名）一律经 `visible` / `safe_text` 可见化再打印；`--json` 输出原始内容。

use std::io::IsTerminal;

use anycon_protocol::*;
use serde_json::{json, Value};

use crate::cli::*;
use crate::commands::{resolve_session, App};
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR};
use crate::output::{safe_text, short_id, table, visible};

/// 超过这么多个文件就不逐个取 patch，只打印 `--stat` 并提示用路径过滤。
pub const DIFF_PATCH_FILES_MAX: usize = 50;

// ============================================================ new：工作位置

/// `new` 的工作位置决定。
pub struct WorkspaceChoice {
    /// 放进 `session.create` 的 `workspace`（`None` = 不带，等于 local）。
    pub workspace: Option<Value>,
    /// 这是按节点默认值选的 worktree（不是用户明确要的）：项目开不了 worktree 时静默退回项目目录。
    pub defaulted: bool,
}

/// 选择新会话的工作位置：`--here` / `--worktree`（`--base` / `--branch` 隐含它）；都不给时，节点的 `worktree.default` 是
/// `worktree` 且节点支持就先试 worktree（项目不能开时 daemon 返回 `not_worktree_ready`，调用方退回项目目录），否则不带。
pub async fn choose_workspace(conn: &Conn, a: &NewArgs) -> Result<WorkspaceChoice> {
    let explicit = a.worktree || a.base.is_some() || a.branch.is_some();
    if a.here {
        return Ok(WorkspaceChoice { workspace: Some(json!({ "mode": "local" })), defaulted: false });
    }
    if explicit {
        if a.project.is_none() {
            return Err(CliError::usage(
                "--worktree / --base / --branch need a project (--project); scratch sessions work in their own folder",
            ));
        }
        let mut w = json!({ "mode": "worktree" });
        if let Some(b) = &a.base {
            w["base"] = json!(b);
        }
        if let Some(b) = &a.branch {
            w["branch"] = json!(b);
        }
        return Ok(WorkspaceChoice { workspace: Some(w), defaulted: false });
    }
    if a.project.is_none() {
        return Ok(WorkspaceChoice { workspace: None, defaulted: false });
    }
    // 节点信息拿不到（老 daemon）就当没有这个功能
    let info: Option<NodeInfo> = conn.call(methods::NODE_INFO, json!({})).await.ok();
    let wants = info
        .as_ref()
        .and_then(|i| i.worktree.as_ref().filter(|_| i.capabilities.worktree))
        .is_some_and(|w| w.default_mode == SessionWorkspaceMode::Worktree);
    Ok(if wants {
        WorkspaceChoice { workspace: Some(json!({ "mode": "worktree" })), defaulted: true }
    } else {
        WorkspaceChoice { workspace: None, defaulted: false }
    })
}

/// daemon 因为「项目不是仓库根目录 / 没有提交」拒绝了 worktree（`data.reason = not_worktree_ready`，CLI 把它附在消息后面）。
pub fn is_not_worktree_ready(e: &CliError) -> bool {
    e.code == "invalid" && e.message.contains("(not_worktree_ready)")
}

// ============================================================ archive / delete / unarchive

fn disposal_param(keep: bool, remove: bool) -> Option<&'static str> {
    match (keep, remove) {
        (true, _) => Some("keep"),
        (_, true) => Some("remove"),
        _ => None,
    }
}

/// 会丢东西、所以要确认的选项（`--yes` 跳过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// `--remove-worktree`：丢弃 worktree 里未提交的改动。
    DiscardChanges,
    /// `--delete-branch`：删分支，没合并到别处的提交会丢。
    DeleteBranch,
    /// `worktrees remove --force`：丢弃 worktree 里未提交的改动（和 `--remove-worktree` 同一种风险）。
    Force,
}

impl Risk {
    fn flag(self) -> &'static str {
        match self {
            Risk::DiscardChanges => "--remove-worktree",
            Risk::DeleteBranch => "--delete-branch",
            Risk::Force => "--force",
        }
    }
    fn effect(self) -> &'static str {
        match self {
            Risk::DiscardChanges | Risk::Force => "discards any uncommitted changes in the worktree",
            Risk::DeleteBranch => "deletes the branch, and commits that are not merged anywhere else are lost",
        }
    }
}

fn describe(risks: &[Risk]) -> String {
    risks.iter().map(|r| format!("{} {}", r.flag(), r.effect())).collect::<Vec<_>>().join("; ")
}

/// 上面这些选项会丢东西：`--yes` 直接过；交互终端里问一次；其它情况（管道、`--json`）拒绝（退出码 2）。
/// 返回 `true` 表示需要提问；没有风险（`risks` 为空）也不用问。
pub fn needs_prompt(yes: bool, interactive: bool, risks: &[Risk]) -> Result<bool> {
    if yes || risks.is_empty() {
        return Ok(false);
    }
    if interactive {
        return Ok(true);
    }
    Err(CliError::usage(format!("{}; pass --yes to confirm", describe(risks))))
}

pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

async fn confirm_risks(app: &App, yes: bool, action: &str, risks: &[Risk]) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && !app.json();
    if !needs_prompt(yes, interactive, risks)? {
        return Ok(());
    }
    let prompt = format!("{action}: {}. Continue? [y/N] ", describe(risks));
    let answer = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s
    })
    .await
    .unwrap_or_default();
    if is_yes(&answer) {
        Ok(())
    } else {
        Err(CliError::new(EXIT_ERROR, "aborted", "nothing was changed"))
    }
}

/// 一个命令里哪些选项会丢东西。
fn risks_of(remove_worktree: bool, delete_branch: bool) -> Vec<Risk> {
    let mut r = Vec::new();
    if remove_worktree {
        r.push(Risk::DiscardChanges);
    }
    if delete_branch {
        r.push(Risk::DeleteBranch);
    }
    r
}

/// `worktrees remove` 的选项里哪些会丢东西。
pub fn remove_risks(force: bool, delete_branch: bool) -> Vec<Risk> {
    let mut r = Vec::new();
    if force {
        r.push(Risk::Force);
    }
    if delete_branch {
        r.push(Risk::DeleteBranch);
    }
    r
}

/// daemon 因为「会话已归档」拒绝了 `send`（`data.reason = archived`，CLI 把它附在消息后面）。
pub fn is_archived(e: &CliError) -> bool {
    e.code == "invalid" && e.message.contains("(archived)")
}

/// 删除会话之后要不要说「worktree … removed」：`failed`（创建失败当时已经清理过，这次只删了记录）和 `removed`
/// （归档时已经删过了）不说，免得让人以为刚刚删掉了什么。
pub fn deleted_worktree_is_announced(state: WorktreeState) -> bool {
    !matches!(state, WorktreeState::Failed | WorktreeState::Removed)
}

/// 处置之后的一行说明（有 worktree 的会话才有）。
fn worktree_state_line(s: &SessionSummary) -> Option<String> {
    let w = s.worktree.as_ref()?;
    let state = format!("{:?}", w.state).to_lowercase();
    Some(format!("worktree {}: {state}", visible(&w.branch)))
}

pub async fn archive(app: &App, a: ArchiveArgs) -> Result<u8> {
    let disposal = disposal_param(a.keep_worktree, a.remove_worktree);
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    confirm_risks(app, a.yes, "archive", &risks_of(a.remove_worktree, false)).await?;
    let mut p = json!({ "session_id": sid });
    if let Some(d) = disposal {
        p["worktree"] = json!(d);
    }
    let _: Empty = conn.call_slow(methods::SESSION_ARCHIVE, p).await?;
    let s: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "ok": true, "worktree": s.worktree }));
    } else {
        println!("archived {}", short_id(&sid));
        if let Some(l) = worktree_state_line(&s) {
            println!("{l}");
        }
    }
    Ok(0)
}

pub async fn delete(app: &App, a: DeleteArgs) -> Result<u8> {
    let disposal = disposal_param(a.keep_worktree, a.remove_worktree);
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    confirm_risks(app, a.yes, "delete", &risks_of(a.remove_worktree, a.delete_branch)).await?;
    // 删之前看一眼它的 worktree，删完要告诉用户那个 worktree 怎么样了（会话没了，之后就查不到了）
    let before: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    let mut p = json!({ "session_id": sid });
    if let Some(d) = disposal {
        p["worktree"] = json!(d);
    }
    if a.delete_branch {
        p["delete_branch"] = json!(true);
    }
    let _: Empty = conn.call_slow(methods::SESSION_DELETE, p).await?;
    let left = match &before.worktree {
        Some(w) => {
            let l: WorkspaceWorktreesResult = conn.call(methods::WORKSPACE_WORKTREES, json!({})).await?;
            l.worktrees.into_iter().find(|x| x.id == w.id)
        }
        None => None,
    };
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "ok": true, "worktree": left }));
    } else {
        println!("deleted {}", short_id(&sid));
        if let Some(w) = &left {
            println!(
                "worktree {} stays as {} ({}); clean it up with `anycon worktrees remove {}`",
                visible(&w.branch),
                visible(&w.path),
                format!("{:?}", w.state).to_lowercase(),
                visible(&w.id)
            );
        } else if let Some(w) = before.worktree.as_ref().filter(|w| deleted_worktree_is_announced(w.state)) {
            println!("worktree {} removed", visible(&w.branch));
        }
    }
    Ok(0)
}

pub async fn unarchive(app: &App, session: String) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &session).await?;
    let _: Empty = conn.call_slow(methods::SESSION_UNARCHIVE, json!({ "session_id": sid })).await?;
    let s: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "ok": true, "worktree": s.worktree }));
    } else {
        println!("unarchived {}", short_id(&sid));
        if let Some(l) = worktree_state_line(&s) {
            println!("{l}");
        }
    }
    Ok(0)
}

// ============================================================ diff

fn status_letter(f: &DiffFile) -> &'static str {
    if f.untracked {
        return "?";
    }
    match f.status {
        DiffFileStatus::Added => "A",
        DiffFileStatus::Modified => "M",
        DiffFileStatus::Deleted => "D",
        DiffFileStatus::Renamed => "R",
    }
}

/// `+N −M` / 二进制。
fn counts(f: &DiffFile) -> String {
    if f.binary {
        "binary".into()
    } else {
        format!("+{} −{}", f.additions, f.deletions)
    }
}

/// `git_status` 的一行：分支 ← base、领先 / 落后、未提交、冲突。
pub fn status_line(st: &SessionGitStatus) -> String {
    let branch = st.branch.clone().unwrap_or_else(|| "(detached)".into());
    let head = match &st.base {
        Some(b) => format!("{} ← {}", visible(&branch), visible(b)),
        None => visible(&branch),
    };
    let mut parts: Vec<String> = Vec::new();
    if let (Some(a), Some(b)) = (st.ahead, st.behind) {
        parts.push(format!("ahead {a}"));
        parts.push(format!("behind {b}"));
    }
    parts.push(format!("{} uncommitted", st.uncommitted));
    if st.conflicts > 0 {
        parts.push(format!("{} conflicted", st.conflicts));
    }
    if st.merging {
        parts.push("merging".into());
    }
    format!("{head}  {}", parts.join(" · "))
}

/// 路径过滤：每个参数是一个文件，或一个目录前缀（`src` 和 `src/` 都匹配 `src/a.rs`）。
pub fn path_matches(path: &str, filters: &[String]) -> bool {
    filters.is_empty()
        || filters.iter().any(|f| {
            let f = f.trim_start_matches("./");
            let dir = f.trim_end_matches('/');
            path == f || (!dir.is_empty() && path.starts_with(&format!("{dir}/")))
        })
}

fn stat_listing(files: &[&DiffFile]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let (mut add, mut del) = (0u64, 0u64);
    for f in files {
        let name = match &f.old_path {
            Some(old) => format!("{old} -> {}", f.path),
            None => f.path.clone(),
        };
        rows.push(vec![status_letter(f).into(), name, counts(f)]);
        add += u64::from(f.additions);
        del += u64::from(f.deletions);
    }
    let mut out = table(&rows);
    out.push_str(&format!("{} file(s) changed, +{add} −{del}\n", files.len()));
    out
}

pub async fn diff(app: &App, a: DiffArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    let s: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    // 在项目目录本身工作的会话没有 base，自动看未提交的
    let uncommitted = a.uncommitted || s.worktree.is_none();
    let against = if uncommitted { "uncommitted" } else { "base" };
    let list: SessionDiffResult =
        conn.call(methods::SESSION_DIFF, json!({ "session_id": sid, "against": against })).await?;
    let files: Vec<&DiffFile> = list.files.iter().filter(|f| path_matches(&f.path, &a.paths)).collect();
    let too_many = files.len() > DIFF_PATCH_FILES_MAX;
    let stat_only = a.stat || too_many;
    if app.json() {
        let mut out =
            SessionDiffResult { from_sha: list.from_sha.clone(), files: Vec::new(), truncated: list.truncated };
        for f in &files {
            let mut f = (*f).clone();
            if !stat_only {
                let one: SessionDiffResult = conn
                    .call(methods::SESSION_DIFF, json!({ "session_id": sid, "against": against, "path": f.path }))
                    .await?;
                if let Some(x) = one.files.into_iter().next() {
                    f = x;
                }
            }
            out.files.push(f);
        }
        app.out.json_line(&out);
        return Ok(0);
    }
    if a.stat {
        if let Ok(st) = conn.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": sid })).await {
            println!("{}", status_line(&st));
        }
    }
    if files.is_empty() {
        println!("no changes{}", if uncommitted { " (uncommitted)" } else { "" });
        return Ok(0);
    }
    if stat_only {
        print!("{}", stat_listing(&files));
        if too_many && !a.stat {
            eprintln!(
                "{} files changed: showing --stat only (patches are printed for up to {DIFF_PATCH_FILES_MAX} files); pass paths to see them",
                files.len()
            );
        }
        if list.truncated {
            eprintln!("(the node lists at most 1000 files)");
        }
        return Ok(0);
    }
    for f in &files {
        let one: SessionDiffResult =
            conn.call(methods::SESSION_DIFF, json!({ "session_id": sid, "against": against, "path": f.path })).await?;
        let Some(file) = one.files.into_iter().next() else { continue };
        if let Some(p) = &file.patch {
            print!("{}", safe_text(p));
            if !p.ends_with('\n') {
                println!();
            }
        }
        if file.patch_truncated {
            println!("(patch of {} truncated at 256 KiB)", visible(&file.path));
        }
    }
    Ok(0)
}

// ============================================================ sync

pub async fn sync(app: &App, a: SyncArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    let s: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    let r: SessionSyncBaseResult =
        conn.call_slow(methods::SESSION_SYNC_BASE, json!({ "session_id": sid, "abort": a.abort })).await?;
    let (base, branch) = s
        .worktree
        .as_ref()
        .map(|w| (visible(&w.base), visible(&w.branch)))
        .unwrap_or_else(|| ("the base".into(), "the branch".into()));
    let code = if r.outcome == SyncOutcome::Conflicts { EXIT_ERROR } else { 0 };
    if app.json() {
        app.out.json_line(&r);
        return Ok(code);
    }
    match r.outcome {
        SyncOutcome::UpToDate => println!("already up to date with {base}"),
        SyncOutcome::Merged => println!("merged {} commit(s) from {base} into {branch}", r.commits),
        SyncOutcome::Aborted => println!("merge aborted"),
        SyncOutcome::Conflicts => {
            println!("conflicts merging {base} into {branch}:");
            for f in &r.conflicts {
                println!("  {}", visible(f));
            }
            println!(
                "the merge is stopped half way: ask the agent to resolve it, or run `anycon sync {} --abort`",
                short_id(&sid)
            );
        }
    }
    Ok(code)
}

// ============================================================ worktrees

pub fn worktree_rows(list: &[WorktreeInfo]) -> Vec<Vec<String>> {
    let mut rows = vec![vec![
        "ID".into(),
        "PROJECT".into(),
        "BRANCH".into(),
        "SESSION".into(),
        "STATE".into(),
        "UNCOMMITTED".into(),
        "AHEAD".into(),
        "SIZE".into(),
    ]];
    for w in list {
        rows.push(vec![
            w.id.clone(),
            w.project.clone(),
            w.branch.clone(),
            w.session_id.as_deref().map(short_id).unwrap_or_else(|| "-".into()),
            format!("{:?}", w.state).to_lowercase(),
            w.uncommitted.to_string(),
            w.ahead.to_string(),
            w.bytes.map(anycon_daemon::util::human_bytes).unwrap_or_else(|| "-".into()),
        ]);
    }
    rows
}

async fn find_worktree(conn: &Conn, key: &str) -> Result<WorktreeInfo> {
    let l: WorkspaceWorktreesResult = conn.call(methods::WORKSPACE_WORKTREES, json!({})).await?;
    let key = key.strip_prefix("wt_").unwrap_or(key);
    let m: Vec<&WorktreeInfo> = l
        .worktrees
        .iter()
        .filter(|w| !key.is_empty() && w.id.strip_prefix("wt_").unwrap_or(&w.id).starts_with(key))
        .collect();
    match m.len() {
        1 => Ok(m[0].clone()),
        0 => {
            Err(CliError::new(EXIT_ERROR, "not_found", format!("no worktree matches {key:?}; see `anycon worktrees`")))
        }
        n => Err(CliError::usage(format!("{key:?} matches {n} worktrees; use a longer prefix"))),
    }
}

pub async fn worktrees(app: &App, a: WorktreesArgs) -> Result<u8> {
    let conn = app.conn().await?;
    match a.cmd {
        None => {
            let r: WorkspaceWorktreesResult =
                conn.call_slow(methods::WORKSPACE_WORKTREES, json!({ "project": a.project })).await?;
            if app.json() {
                app.out.json_line(&r);
            } else if r.worktrees.is_empty() {
                println!("no worktrees");
            } else {
                print!("{}", table(&worktree_rows(&r.worktrees)));
            }
        }
        Some(WorktreesCmd::Remove { id, force, delete_branch, yes }) => {
            // `--force` 丢弃未提交的改动、`--delete-branch` 丢没合并的提交：和 `archive` / `delete` 的对应选项一样要确认
            confirm_risks(app, yes, "remove worktree", &remove_risks(force, delete_branch)).await?;
            let w = find_worktree(&conn, &id).await?;
            let _: Empty = conn
                .call_slow(
                    methods::WORKSPACE_REMOVE_WORKTREE,
                    json!({ "id": w.id, "force": force, "delete_branch": delete_branch }),
                )
                .await?;
            if app.json() {
                app.out.json_line(&json!({ "id": w.id, "ok": true }));
            } else {
                println!(
                    "removed worktree {} ({}){}",
                    visible(&w.path),
                    visible(&w.branch),
                    if delete_branch { " and its branch" } else { "" }
                );
            }
        }
    }
    Ok(0)
}

/// `ls` 的 BRANCH 列：有 worktree 的会话显示分支，其它 `-`。
pub fn branch_cell(s: &SessionSummary) -> String {
    s.worktree.as_ref().map(|w| w.branch.clone()).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: DiffFileStatus, add: u32, del: u32) -> DiffFile {
        DiffFile {
            path: path.into(),
            old_path: None,
            status,
            untracked: false,
            additions: add,
            deletions: del,
            binary: false,
            patch: None,
            patch_truncated: false,
        }
    }

    #[test]
    fn the_discarding_flags_need_a_yes_unless_a_person_can_be_asked() {
        use Risk::*;
        assert!(!needs_prompt(true, false, &[DiscardChanges]).unwrap());
        assert!(!needs_prompt(true, true, &[DeleteBranch]).unwrap());
        assert!(needs_prompt(false, true, &[DiscardChanges]).unwrap());
        assert!(needs_prompt(false, true, &[DeleteBranch]).unwrap());
        assert!(!needs_prompt(false, false, &[]).unwrap(), "nothing risky, nothing to confirm");
        let e = needs_prompt(false, false, &[DiscardChanges]).unwrap_err();
        assert_eq!(e.exit, crate::error::EXIT_USAGE);
        assert!(e.message.contains("--remove-worktree") && e.message.contains("--yes"), "{}", e.message);
        let e = needs_prompt(false, false, &[DeleteBranch]).unwrap_err();
        assert_eq!(e.exit, crate::error::EXIT_USAGE);
        assert!(e.message.contains("--delete-branch") && e.message.contains("--yes"), "{}", e.message);
        let both = needs_prompt(false, false, &risks_of(true, true)).unwrap_err();
        assert!(
            both.message.contains("--remove-worktree") && both.message.contains("--delete-branch"),
            "{}",
            both.message
        );
        assert_eq!(risks_of(false, false), Vec::<Risk>::new());
        // `worktrees remove --force`：丢弃未提交的改动，和 `--remove-worktree` 一样要确认
        assert!(needs_prompt(false, true, &[Force]).unwrap());
        assert!(!needs_prompt(true, false, &[Force]).unwrap());
        let e = needs_prompt(false, false, &remove_risks(true, false)).unwrap_err();
        assert_eq!(e.exit, crate::error::EXIT_USAGE);
        assert!(e.message.contains("--force") && e.message.contains("--yes"), "{}", e.message);
        assert_eq!(remove_risks(false, false), Vec::<Risk>::new());
        assert_eq!(remove_risks(true, true), vec![Force, DeleteBranch]);
        assert!(is_yes("y") && is_yes(" YES\n") && !is_yes("") && !is_yes("n") && !is_yes("yep"));
        assert_eq!(disposal_param(true, false), Some("keep"));
        assert_eq!(disposal_param(false, true), Some("remove"));
        assert_eq!(disposal_param(false, false), None);
    }

    #[test]
    fn path_filters_match_files_and_directory_prefixes() {
        let f = |p: &str, filters: &[&str]| path_matches(p, &filters.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(f("src/a.rs", &[]));
        assert!(f("src/a.rs", &["src"]) && f("src/a.rs", &["src/"]) && f("src/a.rs", &["./src"]));
        assert!(f("src/a.rs", &["src/a.rs"]));
        assert!(!f("srcs/a.rs", &["src"]) && !f("src/a.rs", &["src/b.rs"]) && !f("src/a.rs", &["a.rs"]));
        assert!(f("a.rs", &["x", "a.rs"]));
    }

    #[test]
    fn the_status_line_and_the_file_listing_read_well() {
        let mut st = SessionGitStatus {
            branch: Some("anycon/x".into()),
            head_sha: "abc".into(),
            base: Some("main".into()),
            base_sha: Some("def".into()),
            ahead: Some(2),
            behind: Some(0),
            uncommitted: 3,
            conflicts: 0,
            merging: false,
            upstream: None,
            unpushed: None,
            pr: None,
            pr_error: None,
        };
        assert_eq!(status_line(&st), "anycon/x ← main  ahead 2 · behind 0 · 3 uncommitted");
        st.conflicts = 1;
        st.merging = true;
        assert!(status_line(&st).ends_with("3 uncommitted · 1 conflicted · merging"));
        st.base = None;
        st.ahead = None;
        st.branch = None;
        assert_eq!(status_line(&st), "(detached)  3 uncommitted · 1 conflicted · merging");
        let mut renamed = file("new.txt", DiffFileStatus::Renamed, 0, 0);
        renamed.old_path = Some("old.txt".into());
        let mut bin = file("img.png", DiffFileStatus::Modified, 0, 0);
        bin.binary = true;
        let mut un = file("u.txt", DiffFileStatus::Added, 4, 0);
        un.untracked = true;
        let a = file("a.rs", DiffFileStatus::Modified, 3, 1);
        let listing = stat_listing(&[&a, &renamed, &bin, &un]);
        assert!(listing.contains("M  a.rs") && listing.contains("+3 −1"), "{listing}");
        assert!(listing.contains("R  old.txt -> new.txt"), "{listing}");
        assert!(listing.contains("binary") && listing.contains("?  u.txt"), "{listing}");
        assert!(listing.ends_with("4 file(s) changed, +7 −1\n"), "{listing}");
        // 仓库里的文件名里的转义序列显示成字面文字
        let evil = file("a\x1b[2Kb.rs", DiffFileStatus::Added, 1, 0);
        assert!(!stat_listing(&[&evil]).contains('\x1b'));
    }

    #[test]
    fn worktree_rows_have_every_column_and_blanks_for_unknowns() {
        let w = WorktreeInfo {
            id: "wt_abc".into(),
            project: "proj".into(),
            path: ".anycon/worktrees/x".into(),
            branch: "anycon/x".into(),
            base: "main".into(),
            session_id: None,
            session_title: None,
            state: WorktreeState::Kept,
            uncommitted: 2,
            ahead: 1,
            bytes: None,
            created_at_ms: 1,
        };
        let rows = worktree_rows(&[w]);
        assert_eq!(rows[0].join(","), "ID,PROJECT,BRANCH,SESSION,STATE,UNCOMMITTED,AHEAD,SIZE");
        assert_eq!(rows[1].join(","), "wt_abc,proj,anycon/x,-,kept,2,1,-");
    }

    #[test]
    fn deleting_a_session_whose_worktree_never_existed_does_not_claim_it_removed_one() {
        assert!(deleted_worktree_is_announced(WorktreeState::Active));
        assert!(deleted_worktree_is_announced(WorktreeState::Kept));
        assert!(deleted_worktree_is_announced(WorktreeState::Missing));
        assert!(!deleted_worktree_is_announced(WorktreeState::Failed));
        assert!(!deleted_worktree_is_announced(WorktreeState::Removed));
    }

    #[test]
    fn a_refused_send_to_an_archived_session_is_recognised_by_its_reason() {
        let e: CliError = {
            let mut r = RpcError::new(ErrorCode::Invalid, "the session is archived");
            r.data.as_mut().unwrap().reason = Some("archived".into());
            r.into()
        };
        assert!(is_archived(&e));
        assert_eq!(e.exit, crate::error::EXIT_USAGE);
        let other: CliError = RpcError::new(ErrorCode::Invalid, "content is empty").into();
        assert!(!is_archived(&other));
    }

    #[test]
    fn a_refused_default_worktree_is_recognised_by_its_reason() {
        let e: CliError = {
            let mut r = RpcError::new(ErrorCode::Invalid, "this project cannot have per-session worktrees");
            r.data.as_mut().unwrap().reason = Some("not_worktree_ready".into());
            r.into()
        };
        assert!(is_not_worktree_ready(&e));
        let other: CliError = RpcError::new(ErrorCode::Invalid, "bad base").into();
        assert!(!is_not_worktree_ready(&other));
    }
}
