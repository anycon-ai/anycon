//! 会话的 worktree 与 git（ADR-34，docs/spec/daemon.md § 10.1 / § 7.8）：
//!
//! - 拉起流程里建 / 重建 worktree（`prepare_worktree`），每个时机的目录复核（`check_worktree`、周期检查）；
//! - `session.git_status` / `diff` / `sync_base`；
//! - 归档 / 取消归档 / 删除时处置 worktree；
//! - `workspace.worktrees` / `remove_worktree`；
//! - PR 合并 / 关闭后自动归档。
//!
//! **锁**：本模块所有 git / 磁盘操作都在会话锁之外跑（见上级模块的「锁顺序」）。会话锁里只检查、标 `wt_busy`、读写 `St.wt`
//! 和发事件；持锁时不调会取 `ns_lock` 的 `Workspace` 方法。`project_path` / `project_loc` 是同步的，不取 `ns_lock`。

use std::path::{Path, PathBuf};
use std::time::Instant;

use anycon_protocol::{
    DiffAgainst, DiffFile, DiffFileStatus, ErrorCode, Event, GitAction, PullRequestInfo, PullRequestState, RpcError,
    SessionArchiveParams, SessionDiffParams, SessionDiffResult, SessionGitStatus, SessionGitStatusParams,
    SessionStatus, SessionSyncBaseParams, SessionSyncBaseResult, SyncOutcome, WorkspaceRemoveWorktreeParams,
    WorkspaceWorktreesParams, WorkspaceWorktreesResult, WorktreeDisposal, WorktreeInfo, WorktreeState,
};
use serde_json::json;

use super::{err, stable_code_str, RpcResult, Session, Sessions, St, WtBusy};
use crate::clients::Caller;
use crate::gitops;
use crate::store::WorktreeRow;
use crate::util::{now_ms, warn_err};
use crate::worktree::{
    base_spec, err_reason, inspect, verify, verify_full, Created, DisposeOpts, DisposeOutcome, WtCheck,
};

/// 文件列表最多这么多个。
const DIFF_FILES_MAX: usize = 1000;
/// 单个文件的 patch 最多这么多字节。
const PATCH_MAX: usize = 256 * 1024;
/// 未跟踪的文件超过这么大就不数行数（当作二进制）。
const UNTRACKED_COUNT_MAX: u64 = 1024 * 1024;
/// 一次 `session.diff` 列表请求里给未跟踪文件数行数的预算：累计读这么多字节、或花这么久，超了就不再读。
pub(crate) const COUNT_BYTES_MAX: u64 = 64 * 1024 * 1024;
pub(crate) const COUNT_TIME_MAX: std::time::Duration = std::time::Duration::from_secs(2);
const SYNC_CONFLICTS_MAX: usize = 200;
const SYNC_EVENT_FILES_MAX: usize = 50;
/// PR 自动归档每轮最多查这么多个会话。
const PR_ROUND_MAX: usize = 20;

/// 对已归档的会话 `send` / `sync_base`：`invalid`，`data.reason = "archived"`（先 `unarchive`）。
pub(super) fn archived_error() -> RpcError {
    err_reason(
        ErrorCode::Invalid,
        "the session is archived; unarchive it before sending a message or syncing its base",
        "archived",
    )
}

fn git_event(wt: &WorktreeRow, action: GitAction) -> Event {
    Event::Git { action, branch: wt.branch.clone(), base: None, count: None, files: vec![], note: None }
}

/// `worktree_restored`：和 `worktree_created` 一样带从 `.worktreeinclude` 复制的文件数；超限没复制完时 `note` 说明。
fn restored_event(wt: &WorktreeRow, created: &Created) -> Event {
    Event::Git {
        action: GitAction::WorktreeRestored,
        branch: wt.branch.clone(),
        base: None,
        count: Some(created.copied),
        files: vec![],
        note: created.note.clone(),
    }
}

/// worktree 复核不通过的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MissingWhy {
    /// 目录不存在。
    Gone,
    /// 路径存在但不是原来那个目录（符号链接、别的目录、`.git` 不是普通文件…）。
    Replaced(String),
    /// 目录没动，只是 `.git` 指针文件不再指向记下的那个管理目录。
    Pointer,
}

impl MissingWhy {
    /// 复核结果不是 `Ok` 时的原因。
    pub(super) fn of(check: WtCheck) -> Option<MissingWhy> {
        match check {
            WtCheck::Ok { .. } => None,
            WtCheck::Gone => Some(MissingWhy::Gone),
            WtCheck::Replaced(why) => Some(MissingWhy::Replaced(why)),
            WtCheck::PointerChanged => Some(MissingWhy::Pointer),
        }
    }

    /// `git { worktree_missing }` 事件的说明。
    fn note(&self) -> String {
        match self {
            MissingWhy::Gone => "the directory no longer exists".to_string(),
            MissingWhy::Replaced(why) => format!("the path was replaced: {why}"),
            MissingWhy::Pointer => {
                "the .git file inside it no longer points at the repository (run `git worktree repair` in the project directory); the directory itself was not touched"
                    .to_string()
            }
        }
    }
}

fn worktree_missing_error(wt: &WorktreeRow, why: &MissingWhy) -> RpcError {
    match why {
        MissingWhy::Gone => err_reason(
            ErrorCode::NotFound,
            format!(
                "the worktree directory {} of this session no longer exists; send a message again to recreate it from branch {}",
                wt.rel_path, wt.branch
            ),
            "worktree_missing",
        ),
        MissingWhy::Replaced(why) => err_reason(
            ErrorCode::NotFound,
            format!(
                "{} is no longer the worktree anycon created ({why}); move that path away and send a message again, or archive / delete this session",
                wt.rel_path
            ),
            "worktree_missing",
        ),
        MissingWhy::Pointer => err_reason(
            ErrorCode::NotFound,
            format!(
                "the .git file in {} no longer points at its repository (the directory itself is untouched); run `git worktree repair` in the project directory and send a message again, or archive / delete this session",
                wt.rel_path
            ),
            "worktree_missing",
        ),
    }
}

/// 创建 / 重建的结果之外，拉起流程需要知道的：turn 是不是还是这一个。
fn turn_is_current(st: &St, turn_id: u64, gen: u64) -> bool {
    !st.deleted && st.turn.as_ref().map(|t| t.turn_id) == Some(turn_id) && st.driver_gen == gen
}

/// `git status` 之类要在哪里跑。
struct GitTarget {
    cwd: PathBuf,
    wt: Option<WorktreeRow>,
    /// scratch 会话（没有项目）：要求 cwd 自己就是仓库顶层。
    scratch: bool,
}

impl Sessions {
    // ============================================================ 拉起流程里：建 / 重建 / 复核

    /// 拉起 agent 之前准备好会话的 worktree，返回它的目录（agent 的 cwd）。
    ///
    /// - `active`：复核目录（逐段 `lstat` + `(dev, ino)`）；不满足就标 `missing` 并以 `not_found` 结束 turn；
    /// - `creating` / `failed`：`git worktree add` + `info/exclude` + `.worktreeinclude`；失败清理干净、`state = failed`、
    ///   发 `git { worktree_failed }` 和 `error { worktree_failed, recoverable: false }`；
    /// - `missing`：目录又出现了（身份一致）就恢复；路径不存在就按分支重建（`worktree_restored`）；被换成别的东西就不重建；
    /// - `removed`：已经随归档删掉，要先取消归档。
    ///
    /// 失败时这个 turn 已经收尾（`Err(())`），调用方直接返回。git 操作不在会话锁里跑。
    pub(super) async fn prepare_worktree(
        self: &std::sync::Arc<Self>,
        sess: &std::sync::Arc<Session>,
        turn_id: u64,
        gen: u64,
        project_dir: &Path,
    ) -> Result<PathBuf, ()> {
        enum Plan {
            Verify(WorktreeRow),
            Create(WorktreeRow, bool),
        }
        let plan = loop {
            let mut st = sess.st.lock().await;
            if !turn_is_current(&st, turn_id, gen) {
                return Err(());
            }
            // 上一个 turn（已经取消了）留下的创建还没跑完：等它，不能同时对同一个路径跑两个 `git worktree add`
            if st.wt_busy == Some(WtBusy::Creating) {
                drop(st);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            let Some(wt) = st.wt.clone() else { return Err(()) };
            break match wt.state {
                WorktreeState::Active | WorktreeState::Kept => Plan::Verify(wt),
                WorktreeState::Creating | WorktreeState::Failed => {
                    st.wt_busy = Some(WtBusy::Creating);
                    Plan::Create(wt, false)
                }
                WorktreeState::Missing => match verify(project_dir, &wt) {
                    WtCheck::Ok { .. } => Plan::Verify(wt),
                    WtCheck::Gone => {
                        st.wt_busy = Some(WtBusy::Creating);
                        Plan::Create(wt, true)
                    }
                    other => {
                        let why = MissingWhy::of(other).unwrap_or(MissingWhy::Gone);
                        let e = worktree_missing_error(&wt, &why);
                        self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                        return Err(());
                    }
                },
                WorktreeState::Removed => {
                    let e = err_reason(
                        ErrorCode::Invalid,
                        "this session's worktree was removed when it was archived; unarchive the session to restore it",
                        "worktree_not_active",
                    );
                    self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                    return Err(());
                }
            };
        };
        match plan {
            Plan::Create(wt, rebuild) => self.run_create_worktree(sess, turn_id, gen, project_dir, wt, rebuild).await,
            Plan::Verify(wt) => match verify_full(project_dir, &wt) {
                WtCheck::Ok { dir, identity } => {
                    let mut st = sess.st.lock().await;
                    // 目录自己回来了（`missing` → `active`），归档时保留下来的（`kept`）复核通过要拉起了，或者本来就是 active：
                    // 拉起之后它就是正在工作的 worktree，之后每个时机都按 `active` 复核
                    if st.wt.as_ref().is_some_and(|w| matches!(w.state, WorktreeState::Missing | WorktreeState::Kept)) {
                        self.set_worktree_state(&mut st, WorktreeState::Active, Some(identity)).await;
                        self.push_changed(self.changed_payload(&st), true);
                    }
                    if turn_is_current(&st, turn_id, gen) {
                        if let Some(t) = st.turn.as_mut() {
                            t.preparing = false;
                        }
                    }
                    Ok(dir)
                }
                other => {
                    let mut st = sess.st.lock().await;
                    let why = MissingWhy::of(other).unwrap_or(MissingWhy::Gone);
                    let e = self.mark_worktree_missing(&mut st, why).await;
                    if turn_is_current(&st, turn_id, gen) {
                        self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                    }
                    Err(())
                }
            },
        }
    }

    /// 建 / 重建的执行部分：不持锁跑 git，回来重新加锁写结果。
    pub(super) async fn run_create_worktree(
        self: &std::sync::Arc<Self>,
        sess: &std::sync::Arc<Session>,
        turn_id: u64,
        gen: u64,
        project_dir: &Path,
        wt: WorktreeRow,
        rebuild: bool,
    ) -> Result<PathBuf, ()> {
        let res = self.d.worktrees.create_on_disk(project_dir, &wt, rebuild).await;
        let mut audit = None;
        let out = {
            let mut st = sess.st.lock().await;
            st.wt_busy = None;
            let current = turn_is_current(&st, turn_id, gen);
            match res {
                Ok(created) => {
                    if st.deleted {
                        let deleted_with = st.deleted_with.take();
                        drop(st);
                        let mut gone = wt.clone();
                        gone.identity = Some(created.identity);
                        gone.git_pointer = created.git_pointer.clone();
                        gone.state = WorktreeState::Active;
                        match deleted_with {
                            // 重建（目录不见了之后的下一次 `send`）时会话被删了：复用的是**已有的分支**，可能有很多提交——
                            // 按删除请求的处置方式处理，不带 `delete_branch` 就绝不 `-D`
                            Some((caller, p)) if rebuild => {
                                self.dispose_worktree_of_deleted(&caller, &gone, &p).await;
                            }
                            // 第一次创建时会话被删了：分支是刚从 base 开的、没有任何提交，没人要这个 worktree，连分支一起清掉
                            _ => {
                                let opts = DisposeOpts {
                                    disposal: WorktreeDisposal::Remove,
                                    delete_branch: true,
                                    force_remove: true,
                                };
                                self.d.worktrees.dispose(project_dir, &gone, opts).await;
                                warn_err(
                                    "delete the worktree record of a deleted session",
                                    self.d.store.delete_worktree(&wt.id).await,
                                );
                            }
                        }
                        return Err(());
                    }
                    let dir = project_dir.join(&wt.rel_path);
                    let caller = st.wt_caller.take();
                    self.set_worktree_created(&mut st, &created).await;
                    let row = st.wt.clone().unwrap_or(wt.clone());
                    let ev = if rebuild {
                        restored_event(&row, &created)
                    } else {
                        Event::Git {
                            action: GitAction::WorktreeCreated,
                            branch: row.branch.clone(),
                            base: Some(row.base_display.clone()),
                            count: Some(created.copied),
                            files: vec![],
                            note: created.note.clone(),
                        }
                    };
                    self.emit(&mut st, ev).await;
                    self.persist(&st).await;
                    self.push_changed(self.changed_payload(&st), true);
                    if current {
                        if let Some(t) = st.turn.as_mut() {
                            // agent 的启动超时从这里才开始算
                            t.preparing = false;
                            t.started_at = Instant::now();
                        }
                    }
                    if !rebuild {
                        audit = caller.map(|c| {
                            (
                                c,
                                st.row.id.clone(),
                                json!({ "branch": row.branch, "base": row.base_display, "path": row.rel_path }),
                            )
                        });
                    }
                    if current {
                        Ok(dir)
                    } else {
                        Err(())
                    }
                }
                Err(msg) => {
                    if st.deleted {
                        drop(st);
                        warn_err(
                            "delete the worktree record of a deleted session",
                            self.d.store.delete_worktree(&wt.id).await,
                        );
                        return Err(());
                    }
                    if rebuild {
                        // 重建失败：目录还是不在，状态保持 missing，下一次发消息再试
                        if current {
                            let m = format!("could not recreate the worktree from branch {}: {msg}", wt.branch);
                            self.fail_turn_with(&mut st, "worktree_failed", m, true).await;
                        }
                    } else {
                        self.set_worktree_state(&mut st, WorktreeState::Failed, None).await;
                        let mut ev = git_event(&wt, GitAction::WorktreeFailed);
                        if let Event::Git { note, .. } = &mut ev {
                            *note = Some(msg.clone());
                        }
                        self.emit(&mut st, ev).await;
                        self.push_changed(self.changed_payload(&st), true);
                        if current {
                            self.fail_turn_with(
                                &mut st,
                                "worktree_failed",
                                format!("could not create the worktree: {msg}"),
                                false,
                            )
                            .await;
                        }
                    }
                    Err(())
                }
            }
        };
        if let Some((caller, sid, detail)) = audit {
            self.d.audit.record(&caller, "worktree.create", Some(&sid), detail).await;
        }
        out
    }

    /// 改会话 worktree 的状态（和身份）并写透到库里。调用方持有会话锁。
    async fn set_worktree_state(&self, st: &mut St, state: WorktreeState, identity: Option<(u64, u64)>) {
        let Some(wt) = st.wt.as_mut() else { return };
        wt.state = state;
        if identity.is_some() {
            wt.identity = identity;
        }
        warn_err("save the worktree record", self.d.store.upsert_worktree(wt).await);
    }

    /// worktree 刚建好 / 重建好：`active`，记下目录的身份和 `.git` 指针文件的内容。调用方持有会话锁。
    async fn set_worktree_created(&self, st: &mut St, created: &Created) {
        let Some(wt) = st.wt.as_mut() else { return };
        wt.state = WorktreeState::Active;
        wt.identity = Some(created.identity);
        wt.git_pointer = created.git_pointer.clone();
        warn_err("save the worktree record", self.d.store.upsert_worktree(wt).await);
    }

    /// 目录不见了 / 被换掉：`state = missing`，发 `git { worktree_missing }`，推 `session.changed`。返回给 turn 用的 `not_found` 错误。
    pub(super) async fn mark_worktree_missing(self: &std::sync::Arc<Self>, st: &mut St, why: MissingWhy) -> RpcError {
        let Some(wt) = st.wt.clone() else { return err(ErrorCode::NotFound, "this session has no worktree") };
        if wt.state != WorktreeState::Missing {
            self.set_worktree_state(st, WorktreeState::Missing, None).await;
            let mut ev = git_event(&wt, GitAction::WorktreeMissing);
            if let Event::Git { note, .. } = &mut ev {
                *note = Some(why.note());
            }
            self.emit(st, ev).await;
            self.persist(st).await;
            self.push_changed(self.changed_payload(st), true);
        }
        worktree_missing_error(&wt, &why)
    }

    /// 放行权限请求之前 / 插话之前 / 每个 turn 开始前的复核（ADR-34 第 9 条）：`active` 和 `kept` 的 worktree 目录必须还是创建时的
    /// 那个，`.git` 指针文件也没变（`kept` 但没归档的会话——`archive { remove }` 删失败留下的——也在 turn 里工作）。
    /// 不满足就标 `missing`，返回 `not_found`（`data.reason = "worktree_missing"`）。其它状态不复核。
    pub(super) async fn check_worktree(self: &std::sync::Arc<Self>, st: &mut St) -> Option<RpcError> {
        let wt = st.wt.as_ref()?;
        if !matches!(wt.state, WorktreeState::Active | WorktreeState::Kept) {
            return None;
        }
        let dir = self.d.workspace.project_path(st.row.project.as_deref()?).ok()?;
        let why = MissingWhy::of(verify_full(&dir, wt))?;
        Some(self.mark_worktree_missing(st, why).await)
    }

    /// 进程起来之后（`finish_begin`）核对：worktree 目录这一刻还是创建时的那个吗。
    pub(super) fn worktree_changed_since(&self, st: &St, project_dir: &Path) -> Option<MissingWhy> {
        let w = st.wt.as_ref().filter(|w| matches!(w.state, WorktreeState::Active | WorktreeState::Kept))?;
        MissingWhy::of(verify_full(project_dir, w))
    }

    /// 周期检查里对一个 worktree 会话做的事。有 turn 或 agent 进程的会话都要查，**不看是否归档**（归档的会话本来不会有，
    /// 但哪怕有也不能漏）；已归档又没有 turn、没有进程的不查（`kept` 的目录没人在用，用户可以随便挪）。
    pub(super) async fn recheck_worktree_periodic(self: &std::sync::Arc<Self>, st: &mut St) {
        if st.wt_busy.is_some() || (st.row.archived && st.turn.is_none() && st.driver.is_none()) {
            return;
        }
        let Some(state) = st.wt.as_ref().map(|w| w.state) else { return };
        match state {
            WorktreeState::Active | WorktreeState::Kept => {
                if let Some(e) = self.check_worktree(st).await {
                    if st.turn.is_some() {
                        self.fail_turn_with(st, &stable_code_str(&e), e.message.clone(), true).await;
                    } else if let Some(d) = self.detach_driver(st).await {
                        // 没有 turn、进程还活着：收掉，会话保持原状态，下一次 send 时按 missing 处理
                        tracing::warn!(session = %st.row.id, "the worktree directory changed under an idle agent process; closing it");
                        self.retire(st, d);
                    }
                }
            }
            WorktreeState::Missing => {
                // 目录自己回来了（身份一致）：恢复
                let (Some(project), Some(wt)) = (st.row.project.clone(), st.wt.clone()) else { return };
                let Ok(dir) = self.d.workspace.project_path(&project) else { return };
                if let WtCheck::Ok { identity, .. } = verify_full(&dir, &wt) {
                    self.set_worktree_state(st, WorktreeState::Active, Some(identity)).await;
                    self.persist(st).await;
                    self.push_changed(self.changed_payload(st), true);
                }
            }
            _ => {}
        }
    }

    // ============================================================ session.git_status / diff / sync_base

    /// 会话的 git 工作目录。worktree 会话要求目录还在（逐段 `lstat` + 身份），其它状态按原因报错。
    async fn git_target(&self, sess: &std::sync::Arc<Session>) -> RpcResult<GitTarget> {
        let st = sess.st.lock().await;
        if st.deleted {
            return Err(err(ErrorCode::NotFound, "session was deleted"));
        }
        let Some(project) = st.row.project.as_deref() else {
            return Ok(GitTarget { cwd: self.d.workspace.scratch_path(&st.row.id), wt: None, scratch: true });
        };
        let (project_dir, _) = self.resolve_session_cwd(project, st.row.project_loc.as_deref())?;
        let Some(wt) = st.wt.clone() else {
            return Ok(GitTarget { cwd: project_dir, wt: None, scratch: false });
        };
        match wt.state {
            WorktreeState::Active | WorktreeState::Kept => {}
            WorktreeState::Creating => {
                return Err(err_reason(ErrorCode::Busy, "the worktree is still being created", "worktree_creating"))
            }
            WorktreeState::Failed => {
                return Err(err_reason(ErrorCode::Invalid, "creating the worktree failed", "worktree_failed"))
            }
            // `missing`：说明现在是目录不在、被换掉、还是只有 `.git` 指针不对（后者提示 `git worktree repair`）
            WorktreeState::Missing => {
                let why = MissingWhy::of(verify_full(&project_dir, &wt)).unwrap_or(MissingWhy::Gone);
                return Err(worktree_missing_error(&wt, &why));
            }
            WorktreeState::Removed => return Err(worktree_missing_error(&wt, &MissingWhy::Gone)),
        }
        match verify_full(&project_dir, &wt) {
            WtCheck::Ok { dir, .. } => Ok(GitTarget { cwd: dir, wt: Some(wt), scratch: false }),
            other => Err(worktree_missing_error(&wt, &MissingWhy::of(other).unwrap_or(MissingWhy::Gone))),
        }
    }

    /// 目录必须在 git 仓库里（scratch 会话还要求它自己就是仓库顶层）；否则 `invalid`（`not_git`）。
    async fn require_git(&self, t: &GitTarget) -> RpcResult<()> {
        let not_git = || err_reason(ErrorCode::Invalid, "this session's directory is not a git repository", "not_git");
        let out = self
            .d
            .worktrees
            .git()
            .at(&t.cwd)
            .args(["rev-parse", "--show-toplevel"])
            .secs(15)
            .run()
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("git: {e}")))?;
        if !out.success() {
            return Err(not_git());
        }
        if t.scratch {
            match (crate::sys::canonicalize(out.text()), crate::sys::canonicalize(&t.cwd)) {
                (Ok(a), Ok(b)) if a == b => {}
                _ => return Err(not_git()),
            }
        }
        Ok(())
    }

    /// `gh pr view … -- <branch>`：`Ok(PR)` 或 `pr_error` 的取值。分支名以 `-` 开头直接 `error`（不让它变成 `gh` 的选项），
    /// 参数里 `--` 之后才是分支名。
    async fn pr_lookup(&self, cwd: &Path, branch: &str) -> Result<PullRequestInfo, &'static str> {
        if branch.is_empty() || branch.starts_with('-') {
            return Err("error");
        }
        let git = self.d.worktrees.git();
        match git.at(cwd).args(["remote"]).secs(10).run().await {
            Ok(o) if o.success() && o.text().is_empty() => return Err("no_remote"),
            Ok(_) => {}
            Err(_) => return Err("error"),
        }
        let args =
            ["pr", "view", "--json", "number,url,title,state,isDraft,baseRefName,statusCheckRollup", "--", branch];
        match git.gh(cwd, &args, std::time::Duration::from_secs(20)).await {
            Err(e) if e.is_not_found() => Err("gh_not_installed"),
            Err(_) => Err("error"),
            Ok(o) if o.success() => gitops::parse_pr_json(&String::from_utf8_lossy(&o.stdout)).map_err(|e| {
                tracing::debug!(error = %e, "cannot parse the gh output");
                "error"
            }),
            Ok(o) => Err(gitops::classify_gh_failure(&String::from_utf8_lossy(&o.stderr))),
        }
    }

    /// `session.git_status`。
    pub async fn git_status(self: &std::sync::Arc<Self>, p: SessionGitStatusParams) -> RpcResult<SessionGitStatus> {
        let sess = self.get(&p.session_id)?;
        let t = self.git_target(&sess).await?;
        self.require_git(&t).await?;
        let wts = &self.d.worktrees;
        let status = wts.status_in(&t.cwd).await.ok_or_else(|| err(ErrorCode::Internal, "git status failed"))?;
        let mut out = SessionGitStatus {
            branch: status.head.clone(),
            head_sha: status.oid.clone().unwrap_or_default(),
            base: None,
            base_sha: None,
            ahead: None,
            behind: None,
            uncommitted: status.uncommitted,
            conflicts: status.conflicts,
            merging: false,
            upstream: status.upstream.clone(),
            unpushed: status.upstream.as_ref().and(status.ahead),
            pr: None,
            pr_error: None,
        };
        let git = wts.git();
        out.merging = matches!(
            git.at(&t.cwd).args(["rev-parse", "-q", "--verify", "MERGE_HEAD"]).secs(10).run().await,
            Ok(o) if o.success()
        );
        if let Some(wt) = &t.wt {
            // base 已经解析不了就用创建时的提交
            let base_sha = wts.resolve_commit(&t.cwd, &base_spec(wt)).await.unwrap_or_else(|| wt.base_sha.clone());
            let range = format!("{base_sha}...HEAD");
            if let Ok(o) = git.at(&t.cwd).args(["rev-list", "--left-right", "--count", &range]).secs(15).run().await {
                if o.success() {
                    let text = o.text();
                    let mut it = text.split_whitespace();
                    out.behind = it.next().and_then(|n| n.parse().ok());
                    out.ahead = it.next().and_then(|n| n.parse().ok());
                }
            }
            out.base = Some(wt.base_display.clone());
            out.base_sha = Some(base_sha);
        }
        if p.include_pr {
            // worktree 会话用记录的分支名，不用 worktree 现在检出的（agent 能让 HEAD 指向任何名字）
            let branch = match &t.wt {
                Some(wt) => Some(wt.branch.clone()),
                None => status.head.clone(),
            };
            match branch {
                None => out.pr_error = Some("no_pr".into()),
                Some(branch) => match self.pr_lookup(&t.cwd, &branch).await {
                    Ok(pr) => out.pr = Some(pr),
                    Err(e) => out.pr_error = Some(e.into()),
                },
            }
        }
        Ok(out)
    }

    /// `session.diff`：只读。
    pub async fn diff(self: &std::sync::Arc<Self>, p: SessionDiffParams) -> RpcResult<SessionDiffResult> {
        let sess = self.get(&p.session_id)?;
        let t = self.git_target(&sess).await?;
        self.require_git(&t).await?;
        let wts = &self.d.worktrees;
        let git = wts.git();
        let cwd = t.cwd.as_path();
        // 起点：merge-base(base, HEAD) 或 HEAD
        let from_sha = match p.against {
            DiffAgainst::Base => {
                let Some(wt) = &t.wt else {
                    return Err(err_reason(
                        ErrorCode::Invalid,
                        "diffing against the base needs a session in its own worktree; use against = uncommitted",
                        "not_worktree",
                    ));
                };
                let base_sha = wts.resolve_commit(cwd, &base_spec(wt)).await.unwrap_or_else(|| wt.base_sha.clone());
                match git.at(cwd).args(["merge-base", base_sha.as_str(), "HEAD"]).secs(15).run().await {
                    Ok(o) if o.success() && !o.text().is_empty() => o.text(),
                    // 没有共同祖先（base 被改写过）：退回创建时的提交
                    _ => wt.base_sha.clone(),
                }
            }
            DiffAgainst::Uncommitted => match wts.resolve_commit(cwd, "HEAD").await {
                Some(h) => h,
                None => return Err(err_reason(ErrorCode::Invalid, "the repository has no commits yet", "no_commits")),
            },
        };
        let diff_args = |extra: &[&str], paths: &[String]| -> Vec<String> {
            let mut a: Vec<String> =
                ["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--find-renames", "--relative"]
                    .map(String::from)
                    .to_vec();
            a.extend(extra.iter().map(|s| s.to_string()));
            a.push(from_sha.clone());
            a.push("--".into());
            a.extend(paths.iter().cloned());
            a
        };
        let run = |args: Vec<String>, cap: usize| async move {
            git.at(cwd)
                .args(args)
                .secs(15)
                .cap(cap)
                .run()
                .await
                .map_err(|e| err(ErrorCode::Internal, format!("git diff: {e}")))
        };
        let list_cap = 16 * 1024 * 1024;
        let numstat = run(diff_args(&["--numstat", "-z"], &[]), list_cap).await?;
        let names = run(diff_args(&["--name-status", "-z"], &[]), list_cap).await?;
        if !numstat.success() && !numstat.truncated {
            return Err(err(ErrorCode::Internal, format!("git diff failed: {}", numstat.failure_text(600))));
        }
        let statuses = gitops::parse_name_status_z(&names.stdout);
        let mut files: Vec<DiffFile> = gitops::parse_numstat_z(&numstat.stdout)
            .into_iter()
            .map(|n| {
                let (status, old) =
                    statuses.get(&n.path).cloned().unwrap_or((DiffFileStatus::Modified, n.old_path.clone()));
                DiffFile {
                    path: n.path,
                    old_path: old.or(n.old_path),
                    status,
                    untracked: false,
                    additions: n.additions.unwrap_or(0),
                    deletions: n.deletions.unwrap_or(0),
                    binary: n.additions.is_none(),
                    patch: None,
                    patch_truncated: false,
                }
            })
            .collect();
        // 未跟踪的文件：这里只要名字（`ls-files`），不读文件内容
        let untracked = git
            .at(cwd)
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .secs(15)
            .cap(list_cap)
            .run()
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("git ls-files: {e}")))?;
        files.extend(untracked.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|raw| DiffFile {
            path: String::from_utf8_lossy(raw).into_owned(),
            old_path: None,
            status: DiffFileStatus::Added,
            untracked: true,
            additions: 0,
            deletions: 0,
            binary: false,
            patch: None,
            patch_truncated: false,
        }));
        files.sort_by(|a, b| a.path.cmp(&b.path));
        // 单个文件：`path` 必须在名字列表里（只核对名字，不为此读别的文件），再只读这一个文件
        if let Some(path) = p.path.as_deref() {
            let Some(mut file) = files.into_iter().find(|f| f.path == path) else {
                return Err(err(ErrorCode::NotFound, format!("{path} is not in the list of changed files")));
            };
            if file.untracked {
                let cwd_owned = cwd.to_path_buf();
                let (counted, patch, cut, reads) = tokio::task::spawn_blocking(move || {
                    let (patch, cut) = untracked_patch(&cwd_owned, &file.path);
                    let reads = count_untracked(
                        &cwd_owned,
                        std::slice::from_mut(&mut file),
                        u64::MAX,
                        Instant::now() + COUNT_TIME_MAX,
                    );
                    (file, patch, cut, reads)
                })
                .await
                .map_err(|e| err(ErrorCode::Internal, format!("diff task failed: {e}")))?;
                self.d.worktrees.note_untracked_reads(reads);
                file = counted;
                file.patch = Some(patch);
                file.patch_truncated = cut;
            } else {
                // 改名时带上旧路径，git 才认得出这是改名
                let mut paths = vec![format!(":(literal){}", file.path)];
                if let Some(old) = &file.old_path {
                    paths.push(format!(":(literal){old}"));
                }
                let out = run(diff_args(&[], &paths), PATCH_MAX).await?;
                if !out.success() && !out.truncated {
                    return Err(err(ErrorCode::Internal, format!("git diff failed: {}", out.failure_text(600))));
                }
                file.patch = Some(String::from_utf8_lossy(&out.stdout).into_owned());
                file.patch_truncated = out.truncated;
            }
            return Ok(SessionDiffResult { from_sha, files: vec![file], truncated: false });
        }
        // 列表：合并、排序、先截到 1000 个，**再**给留下的未跟踪文件数行数；数行数有预算（整个请求 2 秒或累计读 64 MiB），
        // 超了的不再读、`additions = 0`。没被忽略的 `node_modules` 之类（十几万个文件）不会拖住请求
        let truncated = files.len() > DIFF_FILES_MAX || numstat.truncated;
        files.truncate(DIFF_FILES_MAX);
        let (max_bytes, max_time) = self.d.worktrees.count_budget();
        let cwd_owned = cwd.to_path_buf();
        let (files, reads) = tokio::task::spawn_blocking(move || {
            let mut files = files;
            let reads = count_untracked(&cwd_owned, &mut files, max_bytes, Instant::now() + max_time);
            (files, reads)
        })
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("diff task failed: {e}")))?;
        self.d.worktrees.note_untracked_reads(reads);
        Ok(SessionDiffResult { from_sha, files, truncated })
    }

    /// `session.sync_base`：把 base 合并进 worktree 分支（daemon.md § 10.1「同步 base」）。
    pub async fn sync_base(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        p: SessionSyncBaseParams,
    ) -> RpcResult<SessionSyncBaseResult> {
        let sess = self.get(&p.session_id)?;
        // 会话锁里：检查并标忙；释放后跑 git
        let (wt, cwd) = {
            let mut st = sess.st.lock().await;
            if st.deleted {
                return Err(err(ErrorCode::NotFound, "session was deleted"));
            }
            // 已归档的会话不能同步（所有会话，先 `unarchive`）
            if st.row.archived {
                return Err(archived_error());
            }
            let Some(wt) = st.wt.clone() else {
                return Err(err_reason(
                    ErrorCode::Invalid,
                    "only sessions in their own worktree can sync a base",
                    "not_worktree",
                ));
            };
            if wt.state != WorktreeState::Active {
                return Err(match wt.state {
                    WorktreeState::Creating => {
                        err_reason(ErrorCode::Busy, "the worktree is still being created", "worktree_creating")
                    }
                    WorktreeState::Failed => {
                        err_reason(ErrorCode::Invalid, "creating the worktree failed", "worktree_failed")
                    }
                    _ => err_reason(
                        ErrorCode::Invalid,
                        "the worktree is not active (kept, removed or missing); restore it first",
                        "worktree_not_active",
                    ),
                });
            }
            let running = matches!(
                st.row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            );
            if st.turn.is_some() || running || st.wt_busy.is_some() || st.promoting {
                return Err(err(ErrorCode::Busy, "a turn is in progress or the worktree is busy"));
            }
            let project = st.row.project.clone().ok_or_else(|| err(ErrorCode::Invalid, "no project"))?;
            let (project_dir, _) = self.resolve_session_cwd(&project, st.row.project_loc.as_deref())?;
            let cwd = match verify_full(&project_dir, &wt) {
                WtCheck::Ok { dir, .. } => dir,
                other => return Err(worktree_missing_error(&wt, &MissingWhy::of(other).unwrap_or(MissingWhy::Gone))),
            };
            st.wt_busy = Some(WtBusy::Syncing);
            (wt, cwd)
        };
        let result = self.run_sync(&wt, &cwd, p.abort).await;
        let mut st = sess.st.lock().await;
        st.wt_busy = None;
        let res = result?;
        let (action, count, files) = match res.outcome {
            SyncOutcome::UpToDate => (GitAction::SyncUpToDate, None, vec![]),
            SyncOutcome::Merged => (GitAction::SyncMerged, Some(res.commits), vec![]),
            SyncOutcome::Conflicts => {
                (GitAction::SyncConflicts, None, res.conflicts.iter().take(SYNC_EVENT_FILES_MAX).cloned().collect())
            }
            SyncOutcome::Aborted => (GitAction::SyncAborted, None, vec![]),
        };
        self.emit(
            &mut st,
            Event::Git {
                action,
                branch: wt.branch.clone(),
                base: Some(wt.base_display.clone()),
                count,
                files,
                note: None,
            },
        )
        .await;
        self.persist(&st).await;
        self.notify_changed(&st);
        drop(st);
        self.d
            .audit
            .record(
                caller,
                "session.sync_base",
                Some(&p.session_id),
                json!({ "outcome": res.outcome, "commits": res.commits }),
            )
            .await;
        Ok(res)
    }

    async fn run_sync(&self, wt: &WorktreeRow, cwd: &Path, abort: bool) -> RpcResult<SessionSyncBaseResult> {
        let wts = &self.d.worktrees;
        #[cfg(any(test, feature = "testing"))]
        {
            let ms = wts.hooks.sync_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
            if ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
        let git = wts.git();
        let git_err = |e: crate::gitops::GitError| err_reason(ErrorCode::Internal, format!("git: {e}"), "git_failed");
        let merging = git
            .at(cwd)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .secs(10)
            .run()
            .await
            .map_err(git_err)?
            .success();
        if abort {
            if !merging {
                return Err(err_reason(
                    ErrorCode::Invalid,
                    "there is no merge in progress to abort",
                    "no_merge_in_progress",
                ));
            }
            let out = git.at(cwd).args(["merge", "--abort"]).secs(30).run().await.map_err(git_err)?;
            if !out.success() {
                return Err(err_reason(
                    ErrorCode::Internal,
                    format!("git merge --abort failed: {}", out.failure_text(2048)),
                    "git_failed",
                ));
            }
            return Ok(SessionSyncBaseResult { outcome: SyncOutcome::Aborted, commits: 0, conflicts: vec![] });
        }
        if merging {
            return Err(err_reason(
                ErrorCode::Invalid,
                "a merge is already in progress; abort it or let the agent resolve it",
                "merge_in_progress",
            ));
        }
        if wts.uncommitted_in(cwd).await.is_none_or(|n| n > 0) {
            return Err(err_reason(
                ErrorCode::Invalid,
                "the worktree has uncommitted changes; commit or discard them first",
                "uncommitted_changes",
            ));
        }
        // base 是远端跟踪分支：先 fetch。用记下的写法（完整引用名或提交），不在 worktree 里按原来的写法重新解析
        let spec = base_spec(wt);
        let full = git
            .at(cwd)
            .args(["rev-parse", "--symbolic-full-name", spec.as_str()])
            .secs(10)
            .run()
            .await
            .map_err(git_err)?;
        if full.success() {
            if let Some(rest) = full.text().strip_prefix("refs/remotes/") {
                let remotes = git.at(cwd).args(["remote"]).secs(10).run().await.map_err(git_err)?.text();
                // 远端名里可以有 `/`：取能匹配上前缀的最长的那个
                let remote = remotes
                    .lines()
                    .filter(|r| rest.strip_prefix(*r).is_some_and(|t| t.starts_with('/')))
                    .max_by_key(|r| r.len());
                if let Some(remote) = remote {
                    let branch = &rest[remote.len() + 1..];
                    let out = git
                        .at(cwd)
                        .args(["-c", "protocol.ext.allow=never", "fetch", "--", remote, branch])
                        .secs(120)
                        .run()
                        .await
                        .map_err(|e| err_reason(ErrorCode::Internal, format!("git fetch: {e}"), "fetch_failed"))?;
                    if !out.success() {
                        return Err(err_reason(
                            ErrorCode::Internal,
                            format!("git fetch failed: {}", out.failure_text(2048)),
                            "fetch_failed",
                        ));
                    }
                }
            }
        }
        // base 现在的提交；已经解析不了就用创建时的
        let target = match wts.resolve_commit(cwd, &spec).await {
            Some(_) => spec.clone(),
            None => wt.base_sha.clone(),
        };
        let target_sha = wts.resolve_commit(cwd, &target).await.unwrap_or_else(|| wt.base_sha.clone());
        let range = format!("HEAD..{target_sha}");
        let behind: u32 = git
            .at(cwd)
            .args(["rev-list", "--count", &range])
            .secs(15)
            .run()
            .await
            .map_err(git_err)?
            .text()
            .parse()
            .unwrap_or(0);
        if behind == 0 {
            return Ok(SessionSyncBaseResult { outcome: SyncOutcome::UpToDate, commits: 0, conflicts: vec![] });
        }
        // 合并提交信息用展示名（`Merge main into anycon/x`），不是 `Merge branch 'refs/heads/main'`——它会进用户的历史
        let message = format!("Merge {} into {}", wt.base_display, wt.branch);
        let out = git
            .at(cwd)
            // daemon 建的合并提交不替用户签名：`--no-gpg-sign` 也不会去跑仓库配置的 `gpg.program`、不会卡在口令输入上
            .args(["merge", "--no-edit", "--no-gpg-sign", "-m", message.as_str(), target.as_str()])
            .env("GIT_MERGE_AUTOEDIT", "no")
            .secs(60)
            .run()
            .await
            .map_err(git_err)?;
        if out.success() {
            return Ok(SessionSyncBaseResult { outcome: SyncOutcome::Merged, commits: behind, conflicts: vec![] });
        }
        let now_merging = git
            .at(cwd)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .secs(10)
            .run()
            .await
            .map_err(git_err)?
            .success();
        // 冲突：`git merge` 退出码 1 且合并停在中途；别的失败（退出码 128：没配 user.email…）不是冲突
        if now_merging && out.code == Some(1) {
            let list = git
                .at(cwd)
                .args(["diff", "--name-only", "--diff-filter=U", "-z"])
                .secs(15)
                .run()
                .await
                .map_err(git_err)?;
            let conflicts: Vec<String> = list
                .stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .take(SYNC_CONFLICTS_MAX)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            return Ok(SessionSyncBaseResult { outcome: SyncOutcome::Conflicts, commits: 0, conflicts });
        }
        // 其他失败（比如没配 user.email）：收拾干净再报错
        let _ = git.at(cwd).args(["merge", "--abort"]).secs(30).run().await;
        Err(err_reason(ErrorCode::Internal, format!("git merge failed: {}", out.failure_text(2048)), "git_failed"))
    }

    // ============================================================ 归档 / 取消归档 / 删除

    /// 会话的 worktree 是否要在归档时处置（还有目录可处置的状态）。`failed` 的记录失败当时已经清理过，归档只是把会话归档。
    fn archive_touches_worktree(st: &St) -> bool {
        st.wt.as_ref().is_some_and(|w| !matches!(w.state, WorktreeState::Removed | WorktreeState::Failed))
    }

    /// `session.archive { session_id, worktree? }`：先检查状态（queued / starting / running / **waiting_permission** 时 `busy`，
    /// 先停止或答复；归档状态下不跑 turn），有 worktree 的会话再收掉 agent 进程、按处置方式处理 worktree，最后置 `archived`。
    pub async fn archive(self: &std::sync::Arc<Self>, caller: &Caller, p: SessionArchiveParams) -> RpcResult<()> {
        self.archive_inner(caller, p, false).await
    }

    /// daemon 自己发起的归档（PR 自动归档）：和用户的 `archive` 一样，但在会话锁里再查一遍——选候选到真正归档之间可以隔几十秒
    /// （每个 `gh` 最长 20 秒），期间用户可能又开始了新的 turn；有 turn、在等审批、有排队的消息就跳过，留到下一轮。
    async fn archive_by_daemon(self: &std::sync::Arc<Self>, p: SessionArchiveParams) -> RpcResult<()> {
        self.archive_inner(&Caller::daemon(), p, true).await
    }

    async fn archive_inner(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        p: SessionArchiveParams,
        by_daemon: bool,
    ) -> RpcResult<()> {
        let sess = self.get(&p.session_id)?;
        let (wt, closing) = {
            let mut st = sess.st.lock().await;
            if st.deleted {
                return Err(err(ErrorCode::NotFound, "session was deleted"));
            }
            if by_daemon && (st.turn.is_some() || !st.backlog.is_empty() || st.interjecting > 0) {
                return Err(err(ErrorCode::Busy, "the session got work since it was picked; try again next round"));
            }
            if matches!(
                st.row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            ) {
                return Err(err(
                    ErrorCode::Busy,
                    "a turn is in progress or waiting for an answer; stop it or answer first",
                ));
            }
            if st.row.archived || !Self::archive_touches_worktree(&st) {
                drop(st);
                return self.set_archived(&p.session_id, true).await;
            }
            if st.wt_busy.is_some() {
                return Err(err(ErrorCode::Busy, "the session's worktree is busy; try again in a moment"));
            }
            st.wt_busy = Some(WtBusy::Archiving);
            // 先收进程：等审批的 turn 一并收尾（进程没了，请求不可能再被答复）
            self.deny_all(&mut st, "session archived").await;
            let old = self.detach_driver(&mut st).await;
            if st.turn.is_some() {
                st.backlog.clear();
                self.end_turn(&mut st, anycon_protocol::TurnDoneReason::Cancelled, None, None).await;
            }
            let closing = old.map(|d| self.retire_tracked(&mut st, d));
            (st.wt.clone().expect("checked above"), closing)
        };
        if let Some(c) = closing {
            if let Some(r) = c.await {
                let mut st = sess.st.lock().await;
                super::adopt_closing_resume(&mut st, r);
                self.persist(&st).await;
            }
        }
        let opts = DisposeOpts { disposal: p.worktree, delete_branch: false, force_remove: true };
        let outcome = self.dispose_with_project(&wt, opts).await;
        let mut st = sess.st.lock().await;
        st.wt_busy = None;
        if st.deleted {
            return Ok(());
        }
        // 想删而没删成（`remove` 时 git 报错）：会话不归档，worktree 留着，把原因报给客户端
        if let (Some(e), WorktreeDisposal::Remove) = (&outcome.error, p.worktree) {
            let msg = format!("could not remove the worktree: {e}");
            self.apply_disposal(&mut st, &wt, &outcome, true).await;
            self.persist(&st).await;
            self.push_changed(self.changed_payload(&st), true);
            return Err(err_reason(ErrorCode::Internal, msg, "worktree_remove_failed"));
        }
        self.apply_disposal(&mut st, &wt, &outcome, true).await;
        st.row.archived = true;
        st.row.updated_ms = now_ms();
        self.persist(&st).await;
        self.push_changed(self.changed_payload(&st), true);
        drop(st);
        self.audit_worktree_remove(
            caller,
            &wt,
            &outcome,
            matches!(p.worktree, WorktreeDisposal::Remove),
            &p.session_id,
        )
        .await;
        Ok(())
    }

    /// 解析项目目录后处置；项目已经不在（回收站 / 解除关联）就什么都不动，结果是 `Kept`。
    async fn dispose_with_project(&self, wt: &WorktreeRow, opts: DisposeOpts) -> DisposeOutcome {
        match self.d.workspace.project_path(&wt.project) {
            Ok(dir) => self.d.worktrees.dispose(&dir, wt, opts).await,
            Err(e) => DisposeOutcome {
                state: WorktreeState::Kept,
                removed_dir: false,
                branch_deleted: false,
                branch_exists: true,
                note: Some(format!("the project is not available: {}", e.message)),
                error: None,
                record_only: false,
            },
        }
    }

    /// 处置结果写回会话：状态、库、`git` 事件。调用方持有会话锁。
    async fn apply_disposal(&self, st: &mut St, wt: &WorktreeRow, out: &DisposeOutcome, announce: bool) {
        let state = out.state;
        self.set_worktree_state(st, state, None).await;
        // 只改了库（磁盘和分支一概没碰）：没有什么可宣布的
        if !announce || out.record_only {
            return;
        }
        // 目录「被换掉」、一概没碰：不是 removed——记录保持 `missing`，事件说明目录没动过
        if state == WorktreeState::Missing {
            let mut ev = git_event(wt, GitAction::WorktreeMissing);
            if let Event::Git { note, .. } = &mut ev {
                *note = out.note.clone();
            }
            self.emit(st, ev).await;
            return;
        }
        if state == WorktreeState::Removed {
            // `note` 是稳定取值（客户端据此选文案）：分支一起删了（或者本来就不在了）/ 分支留着
            let mut ev = git_event(wt, GitAction::WorktreeRemoved);
            if let Event::Git { note, .. } = &mut ev {
                *note = Some(
                    if out.branch_deleted || !out.branch_exists { "branch_deleted" } else { "branch_kept" }.to_string(),
                );
            }
            if let Some(n) = &out.note {
                tracing::debug!(branch = %wt.branch, note = %n, "worktree disposal note");
            }
            self.emit(st, ev).await;
            return;
        }
        // 用户明确选了「保留」不发 kept 事件（事件的含义是「因为有未提交的改动所以留下了」）
        let Some(n) = &out.note else { return };
        let mut ev = git_event(wt, GitAction::WorktreeKept);
        if let Event::Git { note, .. } = &mut ev {
            *note = Some(n.clone());
        }
        self.emit(st, ev).await;
    }

    async fn audit_worktree_remove(
        &self,
        caller: &Caller,
        wt: &WorktreeRow,
        out: &DisposeOutcome,
        force: bool,
        session_id: &str,
    ) {
        if out.removed_dir || out.branch_deleted {
            self.d
                .audit
                .record(
                    caller,
                    "worktree.remove",
                    Some(session_id),
                    json!({ "branch": wt.branch, "force": force, "branch_deleted": out.branch_deleted }),
                )
                .await;
        }
    }

    /// `session.unarchive`：`removed` 的 worktree 按分支重建（路径被占用就换一个目录名并更新记录），`kept` 变回 `active`；
    /// 重建失败则保持归档并返回错误。
    pub async fn unarchive(self: &std::sync::Arc<Self>, session_id: &str) -> RpcResult<()> {
        let sess = self.get(session_id)?;
        let wt = {
            let mut st = sess.st.lock().await;
            if st.deleted {
                return Err(err(ErrorCode::NotFound, "session was deleted"));
            }
            let needs = st.row.archived
                && st.wt.as_ref().is_some_and(|w| {
                    matches!(w.state, WorktreeState::Kept | WorktreeState::Removed | WorktreeState::Missing)
                });
            if !needs {
                drop(st);
                return self.set_archived(session_id, false).await;
            }
            if st.wt_busy.is_some() {
                return Err(err(ErrorCode::Busy, "the session's worktree is busy; try again in a moment"));
            }
            st.wt_busy = Some(WtBusy::Archiving);
            st.wt.clone().expect("checked above")
        };
        let result = self.restore_worktree(&wt).await;
        let mut st = sess.st.lock().await;
        st.wt_busy = None;
        let (row, rebuilt) = result?;
        if st.deleted {
            return Ok(());
        }
        st.wt = Some(row.clone());
        warn_err("save the worktree record", self.d.store.upsert_worktree(&row).await);
        if let Some(created) = &rebuilt {
            self.emit(&mut st, restored_event(&row, created)).await;
        }
        st.row.archived = false;
        st.row.updated_ms = now_ms();
        self.persist(&st).await;
        self.push_changed(self.changed_payload(&st), true);
        Ok(())
    }

    /// 取消归档时让 worktree 回到 `active`：目录还在且是原来的就直接用；不在就重建；被占用就换目录名重建。
    async fn restore_worktree(&self, wt: &WorktreeRow) -> RpcResult<(WorktreeRow, Option<Created>)> {
        let project_dir = self.d.workspace.project_path(&wt.project)?;
        let mut row = wt.clone();
        let check = if wt.state == WorktreeState::Removed {
            inspect(&project_dir, &wt.rel_path)
        } else {
            verify_full(&project_dir, wt)
        };
        match check {
            WtCheck::Ok { identity, .. } if wt.state != WorktreeState::Removed => {
                row.state = WorktreeState::Active;
                row.identity = Some(identity);
                return Ok((row, None));
            }
            WtCheck::Gone => {}
            // 目录被换掉 / 只有 `.git` 指针不对（`kept` 或 `missing` 的）：取消归档照常完成，记录保持 `missing`，
            // 不换目录名重建（分支还检出在原来的目录里，重建会失败）；之后 `send` 按「cwd 与复核」的规则提示
            WtCheck::Replaced(_) | WtCheck::PointerChanged if wt.state != WorktreeState::Removed => {
                row.state = WorktreeState::Missing;
                return Ok((row, None));
            }
            // 路径被别的东西占着（或 removed 的路径上现在有目录）：换一个目录名
            _ => {
                row.rel_path = self.d.worktrees.fresh_rel_path(&project_dir, &wt.project, &wt.branch, &wt.id).await;
            }
        }
        // 重建不记旧身份：`create_on_disk` 把 identity 当作「已经创建过」，用 base_sha 之外的信息只靠分支
        row.identity = None;
        let created = self.d.worktrees.create_on_disk(&project_dir, &row, true).await.map_err(|msg| {
            err_reason(
                ErrorCode::Internal,
                format!("could not recreate the worktree from branch {}: {msg}", wt.branch),
                "git_failed",
            )
        })?;
        row.state = WorktreeState::Active;
        row.identity = Some(created.identity);
        row.git_pointer = created.git_pointer.clone();
        Ok((row, Some(created)))
    }

    /// 删除会话时处置它的 worktree，并更新（或删掉）`worktrees` 行。**不报错**：删除不能被 git 的问题挡住。
    pub(super) async fn dispose_worktree_of_deleted(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        wt: &WorktreeRow,
        p: &anycon_protocol::SessionDeleteParams,
    ) {
        // `failed` 的记录：失败当时已经清理过，没有目录也没有分支可处置，连记录一起删（名字释放出来）
        if wt.state == WorktreeState::Failed {
            warn_err("delete the record of a failed worktree", self.d.store.delete_worktree(&wt.id).await);
            return;
        }
        let opts = DisposeOpts { disposal: p.worktree, delete_branch: p.delete_branch, force_remove: true };
        let out = self.dispose_with_project(wt, opts).await;
        if let Some(e) = &out.error {
            tracing::warn!(session = %p.session_id, error = %e, "cannot fully dispose the worktree of a deleted session");
        }
        let mut row = wt.clone();
        row.session_id = None;
        row.state = out.state;
        let done = out.state == WorktreeState::Removed && (out.record_only || !out.branch_exists);
        let r = if done {
            self.d.store.delete_worktree(&row.id).await.map(|_| ())
        } else {
            self.d.store.upsert_worktree(&row).await
        };
        warn_err("update the worktree record of a deleted session", r);
        self.audit_worktree_remove(caller, wt, &out, matches!(p.worktree, WorktreeDisposal::Remove), &p.session_id)
            .await;
    }

    // ============================================================ workspace.worktrees / remove_worktree

    /// 有没有没归档的 worktree 会话（`workspace.trash` 前的保护：worktree 在项目目录里，会跟着进回收站）。
    pub async fn project_has_worktree_sessions(&self, project: &str) -> bool {
        for s in self.all() {
            let st = s.st.lock().await;
            if !st.deleted
                && !st.row.archived
                && st.wt.as_ref().is_some_and(|w| !matches!(w.state, WorktreeState::Removed | WorktreeState::Failed))
                && st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, project))
            {
                return true;
            }
        }
        false
    }

    /// `workspace.worktrees`。
    pub async fn worktrees(
        self: &std::sync::Arc<Self>,
        p: WorkspaceWorktreesParams,
    ) -> RpcResult<WorkspaceWorktreesResult> {
        let mut rows = self
            .d
            .store
            .list_worktrees()
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("cannot read the worktree records: {e}")))?;
        if let Some(project) = &p.project {
            rows.retain(|r| self.d.workspace.same_project_name(&r.project, project));
        }
        rows.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(a.id.cmp(&b.id)));
        // 现算的部分（git status / 目录大小）共用 3 秒预算，一次最多 4 个并发
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        let mut infos: Vec<WorktreeInfo> = Vec::with_capacity(rows.len());
        for chunk in rows.chunks(4) {
            let mut jobs = Vec::new();
            for r in chunk {
                let title = match r.session_id.as_deref().and_then(|sid| self.get(sid).ok()) {
                    Some(s) => {
                        let st = s.st.lock().await;
                        Some(st.row.title.clone()).filter(|t| !t.is_empty())
                    }
                    None => None,
                };
                jobs.push(self.d.worktrees.info_of(r, title, deadline));
            }
            infos.extend(futures::future::join_all(jobs).await);
        }
        Ok(WorkspaceWorktreesResult { worktrees: infos })
    }

    /// `workspace.remove_worktree`。
    pub async fn remove_worktree(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        p: WorkspaceRemoveWorktreeParams,
    ) -> RpcResult<()> {
        let row = self
            .d
            .store
            .get_worktree(&p.id)
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
            .ok_or_else(|| err(ErrorCode::NotFound, format!("worktree {} not found", p.id)))?;
        // 所属会话存在且没归档 → busy；有 turn 或 agent 进程的（哪怕已归档）同样 busy，不能把正在工作的 agent 的 cwd 删掉；
        // 已归档的标忙，删的过程中不能被取消归档
        let owner = row.session_id.as_deref().and_then(|sid| self.get(sid).ok());
        if let Some(sess) = &owner {
            let mut st = sess.st.lock().await;
            if !st.deleted {
                if st.turn.is_some() || st.driver.is_some() {
                    return Err(err(ErrorCode::Busy, "the session has a turn or an agent process running"));
                }
                if !st.row.archived {
                    return Err(err(ErrorCode::Busy, "the worktree belongs to a session that is not archived"));
                }
                if st.wt_busy.is_some() {
                    return Err(err(ErrorCode::Busy, "the session's worktree is busy; try again in a moment"));
                }
                st.wt_busy = Some(WtBusy::Archiving);
            }
        }
        let result = self.remove_worktree_inner(&row, &p).await;
        if let Some(sess) = &owner {
            let mut st = sess.st.lock().await;
            if st.wt_busy == Some(WtBusy::Archiving) {
                st.wt_busy = None;
            }
            if let Ok(out) = &result {
                if !st.deleted && st.wt.as_ref().is_some_and(|w| w.id == row.id) {
                    self.set_worktree_state(&mut st, WorktreeState::Removed, None).await;
                    self.persist(&st).await;
                    self.push_changed(self.changed_payload(&st), true);
                }
                let _ = out;
            }
        }
        let out = result?;
        self.d
            .audit
            .record(
                caller,
                "worktree.remove",
                Some(&row.id),
                json!({ "branch": row.branch, "force": p.force, "branch_deleted": out.branch_deleted }),
            )
            .await;
        Ok(())
    }

    async fn remove_worktree_inner(
        &self,
        row: &WorktreeRow,
        p: &WorkspaceRemoveWorktreeParams,
    ) -> RpcResult<DisposeOutcome> {
        let session_alive = row.session_id.as_deref().is_some_and(|sid| self.get(sid).is_ok());
        let project_dir = self.d.workspace.project_path(&row.project).ok();
        let out = match &project_dir {
            Some(dir) => {
                if !p.force {
                    if let WtCheck::Ok { dir: wt_dir, .. } = verify_full(dir, row) {
                        if self.d.worktrees.uncommitted_in(&wt_dir).await.is_none_or(|n| n > 0) {
                            return Err(err_reason(
                                ErrorCode::Invalid,
                                "the worktree has uncommitted changes; pass force to discard them",
                                "uncommitted_changes",
                            ));
                        }
                    }
                }
                let opts = DisposeOpts {
                    disposal: WorktreeDisposal::Remove,
                    delete_branch: p.delete_branch,
                    force_remove: p.force,
                };
                self.d.worktrees.dispose(dir, row, opts).await
            }
            // 项目已经不在（回收站 / 解除关联）：没有仓库可以跑 git，只删记录
            None => DisposeOutcome {
                state: WorktreeState::Removed,
                removed_dir: false,
                branch_deleted: false,
                branch_exists: true,
                note: None,
                error: None,
                record_only: false,
            },
        };
        if let Some(e) = &out.error {
            return Err(err_reason(
                ErrorCode::Internal,
                format!("could not remove the worktree: {e}"),
                "worktree_remove_failed",
            ));
        }
        // 会话已经删了的连行一起删；会话还在（已归档）的置 removed
        if session_alive {
            let mut updated = row.clone();
            updated.state = WorktreeState::Removed;
            warn_err("save the worktree record", self.d.store.upsert_worktree(&updated).await);
        } else {
            warn_err("delete the worktree record", self.d.store.delete_worktree(&row.id).await);
        }
        Ok(out)
    }

    // ============================================================ PR 合并 / 关闭后自动归档

    /// `worktree.auto_archive_on_pr_close`：挑没归档、没有 turn 在跑、有上游的 worktree 会话（每轮最多 20 个，最久没查的先查），
    /// `gh pr view` 拿到 `MERGED` / `CLOSED` 就按 `auto` 归档。`gh` 不可用（没装 / 没登录）整轮跳过。返回归档了几个会话。
    pub async fn auto_archive_round(self: &std::sync::Arc<Self>) -> usize {
        let cfg = self.d.config.get();
        if !cfg.worktree.auto_archive_on_pr_close || !self.d.worktrees.available() {
            return 0;
        }
        let mut cands: Vec<(Option<Instant>, String, PathBuf, String)> = Vec::new();
        for sess in self.all() {
            let Ok(st) = sess.st.try_lock() else { continue };
            let busy = matches!(
                st.row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            );
            if st.deleted || st.row.archived || st.turn.is_some() || busy || st.wt_busy.is_some() {
                continue;
            }
            let (Some(wt), Some(project)) =
                (st.wt.as_ref().filter(|w| w.state == WorktreeState::Active), st.row.project.as_deref())
            else {
                continue;
            };
            let Ok(project_dir) = self.d.workspace.project_path(project) else { continue };
            if let WtCheck::Ok { dir, .. } = verify_full(&project_dir, wt) {
                let last = self.pr_checked.lock().unwrap().get(&st.row.id).copied();
                cands.push((last, st.row.id.clone(), dir, wt.branch.clone()));
            }
        }
        // 从没查过的在前，其次最久没查的
        cands.sort_by_key(|c| c.0);
        cands.truncate(PR_ROUND_MAX);
        let mut archived = 0;
        for (_, sid, cwd, branch) in cands {
            self.pr_checked.lock().unwrap().insert(sid.clone(), Instant::now());
            if !self.d.worktrees.has_upstream(&cwd, &branch).await {
                continue;
            }
            match self.pr_lookup(&cwd, &branch).await {
                Ok(pr) if matches!(pr.state, PullRequestState::Merged | PullRequestState::Closed) => {
                    let p = SessionArchiveParams { session_id: sid.clone(), worktree: WorktreeDisposal::Auto };
                    match self.archive_by_daemon(p).await {
                        Ok(()) => archived += 1,
                        Err(e) => {
                            tracing::debug!(session = %sid, error = %e.message, "auto-archive after the PR closed did not go through")
                        }
                    }
                }
                Ok(_) | Err("no_pr" | "no_remote") => {}
                // gh 不可用：整轮跳过
                Err("gh_not_installed" | "gh_not_authenticated") => break,
                Err(_) => {}
            }
        }
        let live: std::collections::HashSet<String> = self.all().iter().map(|s| s.id.clone()).collect();
        self.pr_checked.lock().unwrap().retain(|k, _| live.contains(k));
        archived
    }
}

/// 给列表里未跟踪的文件数行数（读文件；> 1 MiB 或二进制不数，标 `binary`；符号链接算 1 行）。限在 `cwd` 里（逐段 `openat` +
/// `O_NOFOLLOW`）。累计读了 `max_bytes` 或到了 `deadline` 就停：后面的文件不再读，`additions` 保持 0。
/// 返回读了几个文件。
fn count_untracked(cwd: &Path, files: &mut [DiffFile], max_bytes: u64, deadline: Instant) -> u64 {
    use std::io::Read;
    let (mut read_bytes, mut reads) = (0u64, 0u64);
    for file in files.iter_mut().filter(|f| f.untracked) {
        if read_bytes >= max_bytes || Instant::now() >= deadline {
            break;
        }
        let abs = cwd.join(&file.path);
        match std::fs::symlink_metadata(&abs) {
            Ok(md) if md.file_type().is_symlink() => file.additions = 1,
            Ok(_) => {
                let Ok(f) = crate::fsopen::open_file_in(&abs, cwd) else { continue };
                reads += 1;
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                if len > UNTRACKED_COUNT_MAX {
                    file.binary = true;
                    continue;
                }
                let mut buf = Vec::with_capacity(len as usize);
                if f.take(UNTRACKED_COUNT_MAX).read_to_end(&mut buf).is_ok() {
                    read_bytes += buf.len() as u64;
                    if buf[..buf.len().min(8192)].contains(&0) {
                        file.binary = true;
                    } else {
                        let nl = buf.iter().filter(|b| **b == b'\n').count() as u32;
                        file.additions = nl + u32::from(buf.last().is_some_and(|b| *b != b'\n'));
                    }
                }
            }
            Err(_) => {}
        }
    }
    reads
}

/// 未跟踪文件的 `new file` patch（daemon 自己读文件拼，限在 `cwd` 里）。返回 (patch, 是否截断)。
fn untracked_patch(cwd: &Path, rel: &str) -> (String, bool) {
    use std::io::Read;
    let abs = cwd.join(rel);
    if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(&abs).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
        return (
            format!(
                "diff --git a/{rel} b/{rel}\nnew file mode 120000\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1 @@\n+{target}\n\\ No newline at end of file\n"
            ),
            false,
        );
    }
    let binary_patch =
        || format!("diff --git a/{rel} b/{rel}\nnew file mode 100644\nBinary files /dev/null and b/{rel} differ\n");
    let Ok(f) = crate::fsopen::open_file_in(&abs, cwd) else { return (binary_patch(), false) };
    let md = f.metadata().ok();
    // Windows 没有可执行位：一律按 100644
    let exec = md.as_ref().and_then(crate::sys::mode_bits).is_some_and(|m| m & 0o111 != 0);
    let mut buf = Vec::new();
    if f.take(UNTRACKED_COUNT_MAX + 1).read_to_end(&mut buf).is_err() || buf.len() as u64 > UNTRACKED_COUNT_MAX {
        return (binary_patch(), false);
    }
    if buf[..buf.len().min(8192)].contains(&0) {
        return (binary_patch(), false);
    }
    let (patch, _, cut) = gitops::synth_new_file_patch(rel, &buf, exec, PATCH_MAX);
    (patch, cut)
}
