//! 每会话 worktree 的完整性：`failed` 记录占着的名字、冷缓存、已归档会话、`gh` 与合并提交、base 的规范记法、
//! 改名与删除的竞争、`session.diff` 的预算、`.git` 指针。真实 `git`，不联网；慢 git / 慢 `gh` 用 PATH 里的
//! 包装脚本制造（`TestDaemon::slow_git` / `set_gh_sleep`），不依赖机器负载。
//!
//! 每个用例都对着一个具体的现象：去掉对应的修复就红。

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anycon_daemon::store::WorktreeRow;
#[cfg(unix)]
use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use common::wt::*;
use serde_json::json;

async fn archive(f: &mut Fx, sid: &str, worktree: Option<&str>) -> Result<Empty, RpcError> {
    let mut p = json!({ "session_id": sid });
    if let Some(w) = worktree {
        p["worktree"] = json!(w);
    }
    f.call(methods::SESSION_ARCHIVE, p).await
}

async fn unarchive(f: &mut Fx, sid: &str) -> Result<Empty, RpcError> {
    f.call(methods::SESSION_UNARCHIVE, json!({ "session_id": sid })).await
}

async fn delete(f: &mut Fx, sid: &str, worktree: Option<&str>, delete_branch: bool) -> Result<Empty, RpcError> {
    let mut p = json!({ "session_id": sid, "delete_branch": delete_branch });
    if let Some(w) = worktree {
        p["worktree"] = json!(w);
    }
    f.call(methods::SESSION_DELETE, p).await
}

async fn worktrees(f: &mut Fx) -> Vec<WorktreeInfo> {
    let r: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({})).await.unwrap();
    r.worktrees
}

async fn git_status(f: &mut Fx, sid: &str, include_pr: bool) -> SessionGitStatus {
    f.call(methods::SESSION_GIT_STATUS, json!({ "session_id": sid, "include_pr": include_pr })).await.unwrap()
}

async fn diff(f: &mut Fx, sid: &str, against: &str, path: Option<&str>) -> Result<SessionDiffResult, RpcError> {
    let mut p = json!({ "session_id": sid, "against": against });
    if let Some(path) = path {
        p["path"] = json!(path);
    }
    f.call(methods::SESSION_DIFF, p).await
}

async fn sync(f: &mut Fx, sid: &str) -> Result<SessionSyncBaseResult, RpcError> {
    f.call(methods::SESSION_SYNC_BASE, json!({ "session_id": sid })).await
}

/// 一个本地裸仓库当 `origin`，`main` 已经推上去。
fn add_remote(f: &Fx) -> PathBuf {
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    remote
}

fn branch_exists(f: &Fx, branch: &str) -> bool {
    f.branches().iter().any(|b| b == branch)
}

/// 重启 daemon（同一个 home），换一个新客户端；周期复核先关掉（用例自己决定要不要）。
async fn restart(f: &mut Fx) {
    f.td.restart().await;
    f.c = f.td.client("a");
}

async fn row_of(f: &Fx, id: &str) -> WorktreeRow {
    f.td.ctx.store.get_worktree(id).await.unwrap().expect("the worktree row exists")
}

async fn wait_until<F: FnMut() -> bool>(secs: u64, what: &str, mut f: F) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// =====================================================================================================================
// `failed` 的记录占着名字；处置 `failed` / 重名的创建残局只改库
// =====================================================================================================================

/// 创建失败的会话（`fail_after_add` 钩子）。
async fn failed_session(f: &mut Fx, prompt: &str, branch: Option<&str>) -> SessionSummary {
    f.td.ctx.worktrees.hooks.fail_after_add.store(true, Ordering::SeqCst);
    let mut w = json!({ "mode": "worktree" });
    if let Some(b) = branch {
        w["branch"] = json!(b);
    }
    let s = f.create(Some("proj"), Some(prompt), Some(w)).await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Failed).await;
    f.td.ctx.worktrees.hooks.fail_after_add.store(false, Ordering::SeqCst);
    f.get(&s.id).await
}

#[tokio::test]
async fn a_failed_worktree_keeps_its_branch_and_directory_name_so_no_other_session_can_get_them() {
    let mut f = Fx::new().await;
    let s1 = failed_session(&mut f, "write the notes", None).await;
    let w1 = s1.worktree.clone().unwrap();
    assert_eq!((w1.branch.as_str(), w1.path.as_str()), ("anycon/write-the-notes", ".anycon/worktrees/write-the-notes"));
    // 失败当时已经清理干净：目录和分支都不在
    assert!(!f.dir_of(&s1).exists() && !branch_exists(&f, &w1.branch));
    // 同一句话再建一个：名字被失败的那个占着，拿 `-2`
    let s2 = f.ready("write the notes").await;
    let w2 = s2.worktree.clone().unwrap();
    assert_eq!(
        (w2.branch.as_str(), w2.path.as_str()),
        ("anycon/write-the-notes-2", ".anycon/worktrees/write-the-notes-2")
    );
    // 显式给同一个分支名也不行
    let s3 = failed_session(&mut f, "explicit branch", Some("wt/reuse")).await;
    let e = f
        .create(Some("proj"), Some("again"), Some(json!({ "mode": "worktree", "branch": "wt/reuse" })))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists, "{e:?}");
    // 把失败的会话删掉，名字就释放了（记录一起删）
    delete(&mut f, &s1.id, None, false).await.unwrap();
    assert!(worktrees(&mut f).await.iter().all(|w| w.id != w1.id), "the record of the failed worktree is gone");
    let s4 = f.ready("write the notes").await;
    assert_eq!(s4.worktree.unwrap().branch, "anycon/write-the-notes");
    delete(&mut f, &s3.id, None, false).await.unwrap();
    let s5 = f
        .create(Some("proj"), Some("explicit again"), Some(json!({ "mode": "worktree", "branch": "wt/reuse" })))
        .await
        .unwrap();
    assert_eq!(s5.worktree.unwrap().branch, "wt/reuse");
    f.td.shutdown().await;
}

/// 老数据里会有的局面（`failed` 释放名字的版本）：S1 失败了（或崩溃留下 `creating`），S2 后来拿到同一个分支名和目录名，
/// 而且已经在工作（有未提交的文件）。返回 (S1, S2, S2 的目录, S2 的未提交文件)。daemon 已经重启过（内存里的副本和库一致）。
async fn legacy_twin(f: &mut Fx, s1_state: WorktreeState) -> (SessionSummary, SessionSummary, PathBuf, PathBuf) {
    let s2 = f.ready("second twin").await;
    let dir = f.dir_of(&s2);
    let dirty = dir.join("dirty.txt");
    std::fs::write(&dirty, "uncommitted work of the second session").unwrap();
    let s1 = if s1_state == WorktreeState::Failed {
        failed_session(f, "first twin", None).await
    } else {
        // 没有 prompt：worktree 的创建要等第一次 send，库里一直是 creating（等同崩溃留下的残局）
        f.create(Some("proj"), None, Some(json!({ "mode": "worktree" }))).await.unwrap()
    };
    let w2 = s2.worktree.clone().unwrap();
    let mut row = row_of(f, &s1.worktree.as_ref().unwrap().id).await;
    row.rel_path = w2.path.clone();
    row.branch = w2.branch.clone();
    row.state = s1_state;
    f.td.ctx.store.upsert_worktree(&row).await.unwrap();
    restart(f).await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    (s1, s2, dir, dirty)
}

/// S2 的 worktree、未提交的文件、分支、git 登记都还在，状态还是 active。
async fn assert_twin_intact(f: &mut Fx, s2: &SessionSummary, dir: &Path, dirty: &Path, why: &str) {
    let branch = s2.worktree.as_ref().unwrap().branch.clone();
    assert!(dir.join("README.md").is_file(), "{why}: the worktree directory of the other session is gone");
    assert_eq!(std::fs::read_to_string(dirty).unwrap(), "uncommitted work of the second session", "{why}");
    assert!(branch_exists(f, &branch), "{why}: the branch of the other session was deleted");
    assert!(
        f.registered_worktrees().iter().any(|p| Path::new(p) == dir),
        "{why}: git no longer knows the other session's worktree: {:?}",
        f.registered_worktrees()
    );
    assert_eq!(f.get(&s2.id).await.worktree.unwrap().state, WorktreeState::Active, "{why}");
}

#[tokio::test]
async fn archiving_a_failed_session_never_touches_the_worktree_that_reuses_its_name() {
    for disposal in [None, Some("auto"), Some("remove"), Some("keep")] {
        let mut f = Fx::new().await;
        let (s1, s2, dir, dirty) = legacy_twin(&mut f, WorktreeState::Failed).await;
        archive(&mut f, &s1.id, disposal).await.unwrap();
        assert!(f.get(&s1.id).await.archived);
        // 事件里也没有「worktree 被删了」之类的假话
        assert!(f.git_actions(&s1.id).await.iter().all(|a| !matches!(a, GitAction::WorktreeRemoved)));
        assert_twin_intact(&mut f, &s2, &dir, &dirty, &format!("archive {disposal:?}")).await;
        f.td.shutdown().await;
    }
}

#[tokio::test]
async fn deleting_a_failed_session_never_touches_the_worktree_that_reuses_its_name() {
    // 三种处置方式，外加 `delete_branch`
    for (disposal, delete_branch) in
        [(None, false), (Some("auto"), false), (Some("keep"), false), (Some("remove"), false), (Some("remove"), true)]
    {
        let mut f = Fx::new().await;
        let (s1, s2, dir, dirty) = legacy_twin(&mut f, WorktreeState::Failed).await;
        let w1 = s1.worktree.clone().unwrap();
        delete(&mut f, &s1.id, disposal, delete_branch).await.unwrap();
        assert_twin_intact(&mut f, &s2, &dir, &dirty, &format!("delete {disposal:?} delete_branch={delete_branch}"))
            .await;
        // 失败的那一行没有东西可留：连记录一起删了；S2 的那一行还在
        let list = worktrees(&mut f).await;
        assert!(list.iter().all(|w| w.id != w1.id), "{list:?}");
        assert!(list.iter().any(|w| Some(&w.id) == s2.worktree.as_ref().map(|x| &x.id)));
        f.td.shutdown().await;
    }
}

#[tokio::test]
async fn a_failed_record_never_deletes_a_branch_or_a_worktree_that_someone_created_at_its_name_later() {
    let mut f = Fx::new().await;
    let s1 = failed_session(&mut f, "later appears", None).await;
    let w1 = s1.worktree.clone().unwrap();
    assert!(!f.dir_of(&s1).exists() && !branch_exists(&f, &w1.branch), "the failed creation cleaned up after itself");
    // 之后用户（或别的工具）自己在同一个路径上建了同名分支的 worktree，里面有自己的提交和未提交的文件
    let dir = f.dir_of(&s1);
    f.td.git(&f.repo, &["worktree", "add", "-q", "-b", &w1.branch, dir.to_str().unwrap(), "main"]);
    f.td.commit_file(&dir, "mine.txt", "the user's own commit", "user work");
    std::fs::write(dir.join("wip.txt"), "uncommitted").unwrap();
    // 归档、再从列表里删、最后删会话（每一步都带最狠的选项）：这条 failed 的记录只改库
    archive(&mut f, &s1.id, Some("remove")).await.unwrap();
    let _: Empty = f
        .call(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": w1.id, "force": true, "delete_branch": true }))
        .await
        .unwrap();
    delete(&mut f, &s1.id, Some("remove"), true).await.unwrap();
    assert!(dir.join("mine.txt").is_file() && dir.join("wip.txt").is_file(), "the user's worktree is gone");
    assert!(branch_exists(&f, &w1.branch), "the user's branch is gone");
    assert!(f.registered_worktrees().iter().any(|p| Path::new(p) == dir));
    f.td.shutdown().await;
}

#[tokio::test]
async fn remove_worktree_of_a_failed_record_never_touches_the_worktree_that_reuses_its_name() {
    let mut f = Fx::new().await;
    let (s1, s2, dir, dirty) = legacy_twin(&mut f, WorktreeState::Failed).await;
    archive(&mut f, &s1.id, None).await.unwrap();
    let w1 = s1.worktree.clone().unwrap();
    let _: Empty = f
        .call(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": w1.id, "force": true, "delete_branch": true }))
        .await
        .unwrap();
    assert_twin_intact(&mut f, &s2, &dir, &dirty, "remove_worktree of the failed record").await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_retention_cleanup_never_touches_the_worktree_that_reuses_the_name_of_an_old_failed_session() {
    let mut f = Fx::new().await;
    let (s1, s2, dir, dirty) = legacy_twin(&mut f, WorktreeState::Failed).await;
    // S1 已经「很久」没动过了；S2 刚刚用过
    tokio::time::sleep(Duration::from_millis(1500)).await;
    f.send(&s2.id, "touch the second session").await.unwrap();
    f.wait_status(&s2.id, SessionStatus::Idle).await;
    anycon_daemon::config::set_value(&f.td.paths, "sessions.history_retention", "\"1s\"").unwrap();
    f.td.ctx.config.reload().unwrap();
    f.td.ctx.sessions.cleanup().await;
    let e = f.call::<SessionSummary>(methods::SESSION_GET, json!({ "session_id": s1.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "the old failed session was deleted by the retention cleanup");
    assert_twin_intact(&mut f, &s2, &dir, &dirty, "retention cleanup").await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_leftovers_of_a_crashed_creation_are_not_cleaned_up_when_another_record_uses_the_same_path_or_branch() {
    // 崩溃残局（creating）的路径 / 分支和另一个正在工作的会话重名：archive remove、delete remove + delete_branch 都不碰
    for via_delete in [false, true] {
        let mut f = Fx::new().await;
        let (s1, s2, dir, dirty) = legacy_twin(&mut f, WorktreeState::Creating).await;
        if via_delete {
            delete(&mut f, &s1.id, Some("remove"), true).await.unwrap();
        } else {
            archive(&mut f, &s1.id, Some("remove")).await.unwrap();
        }
        assert_twin_intact(&mut f, &s2, &dir, &dirty, "creating leftover").await;
        f.td.shutdown().await;
    }
}

#[tokio::test]
async fn a_leftover_path_that_was_swapped_for_a_symlink_to_another_worktree_is_not_followed() {
    let mut f = Fx::new().await;
    let s2 = f.ready("link target").await;
    let dir = f.dir_of(&s2);
    let dirty = dir.join("dirty.txt");
    std::fs::write(&dirty, "uncommitted work of the second session").unwrap();
    // S1：崩溃残局（creating），它的路径事先被换成了指向 S2 的 worktree 的符号链接
    let s1 = f.create(Some("proj"), None, Some(json!({ "mode": "worktree" }))).await.unwrap();
    let mut row = row_of(&f, &s1.worktree.as_ref().unwrap().id).await;
    let link = f.repo.join(".anycon/worktrees/the-link");
    anycon_daemon::testutil::symlink_dir(&dir, &link).unwrap();
    row.rel_path = ".anycon/worktrees/the-link".into();
    f.td.ctx.store.upsert_worktree(&row).await.unwrap();
    restart(&mut f).await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    delete(&mut f, &s1.id, Some("remove"), true).await.unwrap();
    assert_twin_intact(&mut f, &s2, &dir, &dirty, "symlinked leftover path").await;
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link itself is left alone");
    f.td.shutdown().await;
}

// =====================================================================================================================
// 冷缓存时 `workspace.list` 等 git 元数据算完；超时的算完之后发 `workspace.changed { updated }`
// =====================================================================================================================

#[cfg(unix)]
async fn list_projects(td: &TestDaemon, c: &mut anycon_daemon::testutil::TestClient) -> WorkspaceListResult {
    c.call(td, methods::WORKSPACE_LIST, json!({})).await.unwrap()
}

#[cfg(unix)]
fn pending_of(l: &WorkspaceListResult, name: &str) -> bool {
    l.projects.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("{name} missing")).git_pending
}

#[cfg(unix)]
fn git_of<'a>(l: &'a WorkspaceListResult, name: &str) -> Option<&'a GitMeta> {
    l.projects.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("{name} missing")).git.as_ref()
}

#[cfg(unix)]
fn updated_in(n: &RpcNotification) -> Vec<String> {
    if n.method != methods::WORKSPACE_CHANGED {
        return vec![];
    }
    serde_json::from_value::<WorkspaceChanged>(n.params.clone()).map(|c| c.updated).unwrap_or_default()
}

#[cfg(unix)]
#[tokio::test]
async fn a_cold_cache_waits_for_the_git_metadata_instead_of_returning_projects_without_it() {
    let td = TestDaemon::start_git(&[]).await;
    for n in ["r1", "r2", "r3", "r4"] {
        td.new_repo(n);
    }
    // 每个项目的 `git status` 要 1 秒：远超以前冷缓存时的 50 ms 预算（每个项目要跑 5–6 个子进程，真实机器上也超）
    td.slow_git(&[("*status*", 1.0)]);
    td.ctx.workspace.clear_git_cache_for_test();
    let mut c = td.client("a");
    let t = Instant::now();
    let l = list_projects(&td, &mut c).await;
    for n in ["r1", "r2", "r3", "r4"] {
        let g = git_of(&l, n).unwrap_or_else(|| panic!("{n} has no git metadata on a cold cache: {l:?}"));
        assert_eq!((g.branch.as_deref(), g.worktree_ready), (Some("main"), true), "{n}");
    }
    // 项目之间并行：四个 1 秒的项目不是 4 秒（串行的话后面的早就超过 3 秒的预算、拿不到 git 了）
    assert!(
        t.elapsed() < Duration::from_millis(2800),
        "the projects were computed one after another: {:?}",
        t.elapsed()
    );
    td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn projects_slower_than_the_cold_budget_are_reported_later_with_workspace_changed_updated() {
    let td = TestDaemon::start_git(&[]).await;
    for n in ["fastrepo", "slowrepo", "slowtoo"] {
        td.new_repo(n);
    }
    // 4 秒 > 前台 3 秒的预算（也 < 5 秒的 git 硬超时）
    td.slow_git(&[("*slowrepo*status*", 4.0), ("*slowtoo*status*", 4.0)]);
    let mut c = td.client("watcher");
    let mut other = td.client("other");
    let t = Instant::now();
    let l = list_projects(&td, &mut c).await;
    let took = t.elapsed();
    assert!(git_of(&l, "slowrepo").is_none() && git_of(&l, "slowtoo").is_none(), "{l:?}");
    assert!(pending_of(&l, "slowrepo") && pending_of(&l, "slowtoo"), "no git yet, but still being computed: {l:?}");
    // 项目之间并行，每个项目最多等 3 秒：两个慢项目一共也是 3 秒左右，不是 6 秒
    assert!(took >= Duration::from_millis(2800) && took < Duration::from_millis(5500), "{took:?}");
    // 第二个 list 在计算还没完的时候来：加入同一次计算（再等一会儿就拿到了），也不会让通知变成两条
    let l2 = list_projects(&td, &mut other).await;
    assert!(git_of(&l2, "slowrepo").is_some(), "the second list joined the running computation");
    // 算完之后发 `workspace.changed { updated }`，两个客户端都收到；在这之后的 list 一定带 git
    let mut seen: Vec<String> = Vec::new();
    while !(seen.iter().any(|n| n == "slowrepo") && seen.iter().any(|n| n == "slowtoo")) {
        let n = c
            .wait_notification(Duration::from_secs(10), |n| !updated_in(n).is_empty())
            .await
            .expect("workspace.changed { updated } never arrived");
        seen.extend(updated_in(&n));
    }
    // 快的那个前台就拿到了（机器忙到连它也超时的话，它同样会被通知）：不带 git 的项目一定都被通知了
    assert!(git_of(&l, "fastrepo").is_some() || seen.iter().any(|n| n == "fastrepo"), "{l:?} {seen:?}");
    let l3 = list_projects(&td, &mut c).await;
    for n in ["fastrepo", "slowrepo", "slowtoo"] {
        let g = git_of(&l3, n).unwrap_or_else(|| panic!("{n} has no git metadata after `updated`: {l3:?}"));
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert!(!pending_of(&l3, n), "{n} is no longer pending after `updated`");
    }
    assert!(
        other
            .wait_notification(Duration::from_secs(2), |n| updated_in(n).iter().any(|x| x == "slowrepo"))
            .await
            .is_some(),
        "every client is told"
    );
    // 同一个项目只通知一次（两次 list 共用一次计算）
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rest: Vec<String> = c.drain_now().iter().flat_map(updated_in).collect();
    assert!(rest.iter().all(|n| n != "slowrepo" && n != "slowtoo"), "notified twice: {rest:?}");
    td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_project_whose_git_metadata_is_still_being_computed_is_pending_until_it_arrives_and_a_plain_folder_never_is()
{
    let td = TestDaemon::start_git(&[]).await;
    td.new_repo("fastrepo");
    td.new_repo("slowrepo");
    std::fs::create_dir_all(td.workspace.join("plainfolder")).unwrap();
    // 冷缓存的前台预算缩到 0.5 秒；`slowrepo` 的 `git status` 要 4 秒
    td.ctx.workspace.set_git_cold_budget_for_test(Duration::from_millis(500));
    td.slow_git(&[("*slowrepo*status*", 4.0)]);
    let mut c = td.client("a");
    let t = Instant::now();
    // 第一次 list：超时的项目 `git` 为空且 `git_pending`，不是 git 的文件夹 `git` 为空但不 pending
    let l1 = list_projects(&td, &mut c).await;
    assert!(git_of(&l1, "slowrepo").is_none() && pending_of(&l1, "slowrepo"), "{l1:?}");
    assert!(git_of(&l1, "plainfolder").is_none() && !pending_of(&l1, "plainfolder"), "{l1:?}");
    // 有 git 的项目：要么拿到了（不 pending），要么（机器很忙）也在算（pending）——两者不会同时成立
    assert_ne!(git_of(&l1, "fastrepo").is_some(), pending_of(&l1, "fastrepo"), "{l1:?}");
    // 后台还没算完：再来的 list 仍然 pending（加入同一次计算，各自再等一个预算）
    let l2 = list_projects(&td, &mut c).await;
    assert!(t.elapsed() < Duration::from_millis(3500), "the computation must still be running: {:?}", t.elapsed());
    assert!(git_of(&l2, "slowrepo").is_none() && pending_of(&l2, "slowrepo"), "{l2:?}");
    assert!(!pending_of(&l2, "plainfolder"));
    // 算完并写进缓存之后才发 `updated`；之后的 list 带 git、不 pending
    let n = c
        .wait_notification(Duration::from_secs(10), |n| updated_in(n).iter().any(|x| x == "slowrepo"))
        .await
        .expect("workspace.changed { updated } never arrived");
    assert!(updated_in(&n).iter().all(|x| x != "plainfolder"));
    let l3 = list_projects(&td, &mut c).await;
    assert_eq!(git_of(&l3, "slowrepo").and_then(|g| g.branch.as_deref()), Some("main"), "{l3:?}");
    assert!(!pending_of(&l3, "slowrepo") && !pending_of(&l3, "fastrepo") && !pending_of(&l3, "plainfolder"), "{l3:?}");
    // 非 git 的文件夹永远不 pending（缓存过期之后也一样）
    td.ctx.workspace.clear_git_cache_for_test();
    let l4 = list_projects(&td, &mut c).await;
    assert!(git_of(&l4, "plainfolder").is_none() && !pending_of(&l4, "plainfolder"));
    td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn with_a_cached_value_the_list_keeps_its_fifty_millisecond_front_budget() {
    let td = TestDaemon::start_git(&[]).await;
    td.new_repo("cachedrepo");
    let mut c = td.client("a");
    assert!(git_of(&list_projects(&td, &mut c).await, "cachedrepo").is_some());
    // 缓存过期之后重新算很慢（2 秒）：前台只等 50 ms，先给旧值（带 git），不等
    td.slow_git(&[("*cachedrepo*status*", 2.0)]);
    tokio::time::sleep(Duration::from_millis(5100)).await;
    let t = Instant::now();
    let l = list_projects(&td, &mut c).await;
    assert!(git_of(&l, "cachedrepo").is_some(), "the stale value is returned");
    assert!(t.elapsed() < Duration::from_millis(800), "{:?}", t.elapsed());
    // 过期刷新不发 `updated`（旧值本来就带 git，客户端不用重新拉）
    assert!(c.wait_notification(Duration::from_secs(3), |n| !updated_in(n).is_empty()).await.is_none());
    td.shutdown().await;
}

// =====================================================================================================================
// 已归档不能 send / sync_base；kept 也复核；周期检查不看 archived；remove_worktree 看 turn / 进程
// =====================================================================================================================

#[tokio::test]
async fn an_archived_session_cannot_send_or_sync_until_it_is_unarchived() {
    let mut f = Fx::new().await;
    // worktree 会话（auto 归档删掉干净的 worktree）
    let w = f.ready("archived worktree").await;
    archive(&mut f, &w.id, None).await.unwrap();
    let e = f.send(&w.id, "hello?").await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("archived")), "{e:?}");
    let e = sync(&mut f, &w.id).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("archived")), "{e:?}");
    // 没有 worktree 的会话（项目目录 / scratch）同样
    let local = f.create(Some("proj"), Some("local one"), None).await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    let scratch = f.create(None, Some("scratch one"), None).await.unwrap();
    f.wait_status(&scratch.id, SessionStatus::Idle).await;
    for s in [&local, &scratch] {
        archive(&mut f, &s.id, None).await.unwrap();
        let e = f.send(&s.id, "hello?").await.unwrap_err();
        assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("archived")), "{e:?}");
        let e = sync(&mut f, &s.id).await.unwrap_err();
        assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("archived")), "{e:?}");
        assert_eq!(f.get(&s.id).await.status, SessionStatus::Idle, "nothing started");
    }
    let spawns = f.td.fake.spawn_count();
    // 取消归档之后一切照常（worktree 按分支重建）
    unarchive(&mut f, &w.id).await.unwrap();
    f.send(&w.id, "now it works").await.unwrap();
    f.wait_status(&w.id, SessionStatus::Idle).await;
    assert!(f.dir_of(&w).join("README.md").is_file());
    unarchive(&mut f, &local.id).await.unwrap();
    f.send(&local.id, "and this one").await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    assert!(f.td.fake.spawn_count() > spawns);
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_kept_session_that_is_not_archived_is_rechecked_like_an_active_one_and_becomes_active_when_it_starts() {
    let mut f = Fx::new().await;
    let s = f.ready("kept but working").await;
    let dir = f.dir_of(&s);
    // `archive { remove }` 删失败时会留下「kept 但没归档」：这里直接改库来造
    let mut row = row_of(&f, &s.worktree.as_ref().unwrap().id).await;
    row.state = WorktreeState::Kept;
    f.td.ctx.store.upsert_worktree(&row).await.unwrap();
    restart(&mut f).await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Kept);
    f.send(&s.id, "needs permission").await.unwrap();
    f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
    // 拉起时复核通过，kept 置回 active
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    let pending: PermissionListResult = f.call(methods::PERMISSION_LIST, json!({ "session_id": s.id })).await.unwrap();
    let req = pending.requests[0].clone();
    let elsewhere = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    anycon_daemon::testutil::symlink_dir(&elsewhere, &dir).unwrap();
    let allow = req.options.iter().find(|o| o.kind == PermissionOptionKind::AllowOnce).unwrap().id.clone();
    let e = f
        .call::<Empty>(methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": allow }))
        .await
        .unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("worktree_missing")), "{e:?}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_periodic_check_looks_at_archived_sessions_that_still_have_an_agent_process() {
    let mut f = Fx::new().await;
    let s = f.ready("archived with a process").await;
    let dir = f.dir_of(&s);
    let handle = f.td.fake.last_handle();
    // 「已归档的标记 + 进程还活着」（等审批时已经不能归档了；空闲的会话只改标记，进程不动）
    f.td.ctx.sessions.set_archived(&s.id, true).await.unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    // 约 2 秒的周期检查发现目录没了：标 missing、收掉进程
    let after = f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    assert!(after.archived);
    wait_until(10, "the idle agent process to be closed", || handle.shutdowns.load(Ordering::SeqCst) > 0).await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn remove_worktree_is_busy_while_the_session_still_has_an_agent_process_even_when_archived() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let s = f.ready("busy remove").await;
    let id = s.worktree.clone().unwrap().id;
    let dir = f.dir_of(&s);
    // 空闲但进程还活着（已归档的标记）
    f.td.ctx.sessions.set_archived(&s.id, true).await.unwrap();
    let e = f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": id, "force": true })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
    assert!(dir.join("README.md").is_file(), "the cwd of a live agent process was not deleted");
    // 进程收掉之后就可以了
    let _: Empty = f.call(methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let _: Empty = f.call(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": id, "force": true })).await.unwrap();
    assert!(!dir.exists());
    f.td.shutdown().await;
}

// =====================================================================================================================
// PR 轮次不在 sweeper 的同步路径上
// =====================================================================================================================

#[cfg(unix)]
fn open_pr_json() -> String {
    json!({ "number": 9, "url": "https://example.test/pr/9", "title": "t", "state": "OPEN", "isDraft": false, "baseRefName": "main", "statusCheckRollup": [] }).to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn a_hung_gh_does_not_stall_the_periodic_directory_check() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let with_pr = f.ready("has an upstream").await;
    f.td.git(&f.dir_of(&with_pr), &["push", "-q", "-u", "origin", "HEAD"]);
    let canary = f.ready("canary session").await;
    anycon_daemon::config::set_value(&f.td.paths, "worktree.auto_archive_on_pr_close", "true").unwrap();
    f.td.ctx.config.reload().unwrap();
    f.td.set_gh(&open_pr_json(), "", 0);
    f.td.set_gh_sleep(9.0);
    f.td.ctx.sessions.set_pr_round_every(2); // 约 1 秒一次
    wait_until(10, "the PR round to ask gh", || f.td.gh_calls() >= 1).await;
    // 这一轮在 `gh` 里卡着（9 秒）；这期间目录被换掉的会话仍要在约 2 秒内被发现
    let t = Instant::now();
    f.td.git(&f.repo, &["worktree", "remove", "--force", f.dir_of(&canary).to_str().unwrap()]);
    loop {
        if f.get(&canary.id).await.worktree.unwrap().state == WorktreeState::Missing {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "the periodic check was stalled by the PR round");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 上一轮没跑完时不会再起新的一轮（原子标记）：这几秒里 `gh` 只被问过一次
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(f.td.gh_calls(), 1, "a second PR round started while the first one was still running");
    f.td.shutdown().await;
}

// =====================================================================================================================
// `gh pr view … -- <记录的分支>`；合并不签名
// =====================================================================================================================

#[cfg(unix)]
#[tokio::test]
async fn gh_is_asked_about_the_recorded_branch_after_a_double_dash_and_never_about_a_dash_name() {
    let mut f = Fx::new().await;
    add_remote(&f);
    f.td.set_gh(&open_pr_json(), "", 0);
    // worktree 会话：agent 在 worktree 里切到别的分支，PR 查的仍是记录的分支
    let s = f.ready("recorded branch").await;
    let dir = f.dir_of(&s);
    f.td.git(&dir, &["checkout", "-q", "-b", "somewhere-else"]);
    let st = git_status(&mut f, &s.id, true).await;
    assert_eq!(st.branch.as_deref(), Some("somewhere-else"));
    assert!(st.pr.is_some());
    let calls = std::fs::read_to_string(f.td.dir.path().join("bin/gh.calls")).unwrap();
    assert!(calls.trim_end().ends_with("-- anycon/recorded-branch"), "{calls}");
    // 项目目录里的会话：HEAD 指向一个以 `-` 开头的分支名——`gh` 一次都不能被调用
    let local = f.create(Some("proj"), Some("dashed branch"), None).await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    f.td.git(&f.repo, &["update-ref", "refs/heads/--web", "HEAD"]);
    f.td.git(&f.repo, &["symbolic-ref", "HEAD", "refs/heads/--web"]);
    let calls_before = f.td.gh_calls();
    let st = git_status(&mut f, &local.id, true).await;
    assert_eq!(st.branch.as_deref(), Some("--web"));
    assert_eq!((st.pr.is_none(), st.pr_error.as_deref()), (true, Some("error")));
    assert_eq!(f.td.gh_calls(), calls_before, "gh must not be started with a branch name that looks like an option");
    f.td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn the_merge_commit_of_a_sync_is_never_signed_and_never_runs_the_configured_gpg_program() {
    let mut f = Fx::new().await;
    let s = f.ready("sync unsigned").await;
    let dir = f.dir_of(&s);
    // 分叉：worktree 里一个提交，base 上另一个（不冲突）——合并要产生一个合并提交
    f.td.commit_file(&dir, "mine.txt", "mine", "my own commit");
    f.td.commit_file(&f.repo, "theirs.txt", "theirs", "base moved");
    // 之后才打开签名并指向一个会留下标记文件的假 gpg
    let marker = f.td.dir.path().join("GPG-RAN");
    let gpg = f.td.dir.path().join("fake-gpg.sh");
    std::fs::write(&gpg, format!("#!/bin/sh\nPATH=/usr/bin:/bin\ntouch {}\nexit 1\n", marker.display())).unwrap();
    std::fs::set_permissions(&gpg, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = f.td.dir.path().join("gitconfig");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text = text.replace("gpgsign = false", "gpgsign = true");
    text.push_str(&format!("[gpg]\n\tprogram = {}\n", gpg.display()));
    std::fs::write(&cfg, text).unwrap();
    let r = sync(&mut f, &s.id).await.unwrap();
    assert_eq!((r.outcome, r.commits), (SyncOutcome::Merged, 1));
    assert!(!marker.exists(), "the configured gpg program was executed by the daemon's merge");
    assert!(dir.join("theirs.txt").is_file() && dir.join("mine.txt").is_file());
    f.td.shutdown().await;
}

// =====================================================================================================================
// base 的规范记法
// =====================================================================================================================

#[tokio::test]
async fn a_base_written_as_head_is_recorded_as_a_commit_and_committed_work_stays_visible_against_it() {
    let mut f = Fx::new().await;
    let main_sha = f.td.git(&f.repo, &["rev-parse", "HEAD"]);
    let s = f
        .create(Some("proj"), Some("base is head"), Some(json!({ "mode": "worktree", "base": "HEAD" })))
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let w = f.get(&s.id).await.worktree.unwrap();
    // 只记提交；展示的是前 12 位，完整的在 `base_sha`
    assert_eq!((w.base.as_str(), w.base_sha.as_str()), (&main_sha[..12], main_sha.as_str()));
    let row = row_of(&f, &w.id).await;
    assert_eq!((row.base.as_str(), row.base_display.as_str()), (main_sha.as_str(), &main_sha[..12]));
    // worktree 里提交一个文件：HEAD 在 worktree 里指向它自己的 HEAD，以前这个提交从「相对 base」里消失、ahead = 0
    let dir = f.dir_of(&s);
    f.td.commit_file(&dir, "committed.txt", "x\n", "work in the worktree");
    let d = diff(&mut f, &s.id, "base", None).await.unwrap();
    assert_eq!(d.from_sha, main_sha);
    assert!(d.files.iter().any(|x| x.path == "committed.txt"), "{:?}", d.files);
    let st = git_status(&mut f, &s.id, false).await;
    assert_eq!((st.ahead, st.behind, st.base.as_deref()), (Some(1), Some(0), Some(&main_sha[..12])));
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_base_written_as_a_branch_is_recorded_as_the_full_ref_and_follows_the_branch() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let local =
        f.create(Some("proj"), Some("base main"), Some(json!({ "mode": "worktree", "base": "main" }))).await.unwrap();
    let remote = f
        .create(Some("proj"), Some("base origin"), Some(json!({ "mode": "worktree", "base": "origin/main" })))
        .await
        .unwrap();
    for s in [&local, &remote] {
        f.wait_wt_state(&s.id, WorktreeState::Active).await;
        f.wait_status(&s.id, SessionStatus::Idle).await;
    }
    let (lw, rw) = (f.get(&local.id).await.worktree.unwrap(), f.get(&remote.id).await.worktree.unwrap());
    // 展示去掉前缀；库里是完整引用名
    assert_eq!((lw.base.as_str(), rw.base.as_str()), ("main", "origin/main"));
    assert_eq!(row_of(&f, &lw.id).await.base, "refs/heads/main");
    assert_eq!(row_of(&f, &rw.id).await.base, "refs/remotes/origin/main");
    // 标签记 `refs/tags/…`、展示标签名；其它写法（HEAD~0 / reflog 表达式）只记提交，展示前 12 位
    f.td.git(&f.repo, &["tag", "v1.0"]);
    let sha = f.td.git(&f.repo, &["rev-parse", "HEAD"]);
    for (spelling, name, want_base, want_display) in [
        ("v1.0", "tag", "refs/tags/v1.0".to_string(), "v1.0".to_string()),
        ("HEAD~0", "expr", sha.clone(), sha[..12].to_string()),
        ("main@{0}", "reflog", sha.clone(), sha[..12].to_string()),
    ] {
        let s = f
            .create(
                Some("proj"),
                Some(&format!("spelled {name}")),
                Some(json!({ "mode": "worktree", "base": spelling })),
            )
            .await
            .unwrap();
        f.wait_wt_state(&s.id, WorktreeState::Active).await;
        f.wait_status(&s.id, SessionStatus::Idle).await;
        let w = f.get(&s.id).await.worktree.unwrap();
        let row = row_of(&f, &w.id).await;
        assert_eq!((row.base, row.base_display), (want_base, want_display.clone()), "{spelling}");
        assert_eq!(w.base, want_display, "{spelling}: clients get the display name");
    }
    // 主目录的 main 前进：`--base main` 的会话 behind = 1
    f.td.commit_file(&f.repo, "moved.txt", "m", "main moved");
    assert_eq!(git_status(&mut f, &local.id, false).await.behind, Some(1));
    // 同步产生合并提交：信息用展示名，不是 `Merge branch 'refs/heads/main'`
    f.td.commit_file(&f.dir_of(&local), "mine.txt", "m", "my own commit");
    let r = sync(&mut f, &local.id).await.unwrap();
    assert_eq!((r.outcome, r.commits), (SyncOutcome::Merged, 1));
    assert_eq!(git_status(&mut f, &local.id, false).await.behind, Some(0));
    let subject = f.td.git(&f.dir_of(&local), &["log", "-1", "--format=%s"]);
    assert_eq!(subject, format!("Merge main into {}", lw.branch));
    assert!(!subject.contains("refs/"), "{subject}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn records_with_the_old_short_base_spellings_keep_working_and_are_normalized_when_the_daemon_starts() {
    let mut f = Fx::new().await;
    let a = f
        .create(Some("proj"), Some("legacy short"), Some(json!({ "mode": "worktree", "base": "main" })))
        .await
        .unwrap();
    let b =
        f.create(Some("proj"), Some("legacy head"), Some(json!({ "mode": "worktree", "base": "main" }))).await.unwrap();
    for s in [&a, &b] {
        f.wait_wt_state(&s.id, WorktreeState::Active).await;
        f.wait_status(&s.id, SessionStatus::Idle).await;
    }
    let (aw, bw) = (f.get(&a.id).await.worktree.unwrap(), f.get(&b.id).await.worktree.unwrap());
    // 旧版本记下的是用户写的原样
    let mut ra = row_of(&f, &aw.id).await;
    ra.base = "main".into();
    ra.base_display = String::new(); // 迁移 0006 之前的记录没有展示名
    f.td.ctx.store.upsert_worktree(&ra).await.unwrap();
    let mut rb = row_of(&f, &bw.id).await;
    rb.base = "HEAD".into();
    rb.base_display = String::new();
    f.td.ctx.store.upsert_worktree(&rb).await.unwrap();
    restart(&mut f).await;
    // 启动时能规范的就规范了；展示名跟着（没有 `base_display` 的旧记录按 base 推：引用去前缀、提交取前 12 位）
    let (ra, rb) = (row_of(&f, &aw.id).await, row_of(&f, &bw.id).await);
    assert_eq!((ra.base.as_str(), ra.base_display.as_str()), ("refs/heads/main", "main"));
    assert_eq!(rb.base, bw.base_sha, "HEAD-like spellings become the commit");
    assert_eq!(rb.base_display, bw.base_sha[..12]);
    // 照常可用：短名字跟着分支走；HEAD 类写法不会在 worktree 里解析成它自己的 HEAD
    f.td.commit_file(&f.repo, "moved.txt", "m", "main moved");
    assert_eq!(git_status(&mut f, &a.id, false).await.behind, Some(1));
    f.td.commit_file(&f.dir_of(&b), "mine.txt", "m", "committed in the worktree");
    let st = git_status(&mut f, &b.id, false).await;
    assert_eq!((st.ahead, st.behind), (Some(1), Some(0)));
    f.td.shutdown().await;
}

// =====================================================================================================================
// 同步 / 归档处置 / 删 worktree 期间项目不能改名
// =====================================================================================================================

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_project_cannot_be_renamed_while_a_session_runs_a_git_operation() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let s = f
        .create(Some("proj"), Some("sync target"), Some(json!({ "mode": "worktree", "base": "origin/main" })))
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    // 没有 turn、没有 agent 进程：这样项目只有「git 忙标记」一个理由是忙的
    let _: Empty = f.call(methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    assert!(
        !f.td.ctx.sessions.project_busy("proj").await,
        "an idle session without a process does not keep the project busy"
    );
    // 远端跟踪的 base 先 fetch：让 fetch 卡 2 秒（PATH 里的假 git）
    f.td.slow_git(&[("*fetch*", 2.0)]);
    let (rpc, sid, handle) = (f.td.rpc(), s.id.clone(), f.td.client("syncer").handle.clone());
    let syncing = tokio::spawn(async move {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Num(5),
            method: methods::SESSION_SYNC_BASE.into(),
            params: json!({ "session_id": sid }),
        };
        rpc.handle_request(&handle, req).await
    });
    wait_until(10, "the fetch to start", || !f.td.git_hits().is_empty()).await;
    assert!(f.td.ctx.sessions.project_busy("proj").await);
    let e = f
        .call::<RenamedProject>(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
    assert!(f.repo.is_dir(), "the project directory was not moved");
    assert!(matches!(syncing.await.unwrap(), RpcMessage::Response(r) if r.error.is_none()));
    // 同步结束，标记清掉：可以改名了
    f.td.slow_git(&[]);
    assert!(!f.td.ctx.sessions.project_busy("proj").await);
    let _: RenamedProject =
        f.call(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" })).await.unwrap();
    f.td.shutdown().await;
}

// =====================================================================================================================
// 重建进行中删除会话
// =====================================================================================================================

/// 一个有提交的 worktree 会话，目录被删了、状态 missing；下一次 send 会重建（hook 让重建停 1.5 秒）。返回 (会话, 分支名)。
async fn rebuilding_session(f: &mut Fx, prompt: &str) -> (SessionSummary, String) {
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let s = f.ready(prompt).await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&dir, "valuable.txt", "commits that exist only in this branch", "valuable work");
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    f.send(&s.id, "first try").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    f.wait_status(&s.id, SessionStatus::Error).await;
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(1500, Ordering::SeqCst);
    f.send(&s.id, "rebuild now").await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await; // 重建已经开始（`git worktree add` 在跑 / 之后的停顿里）
    let branch = s.worktree.clone().unwrap().branch;
    (s, branch)
}

#[tokio::test]
async fn deleting_a_session_while_its_worktree_is_being_rebuilt_follows_the_delete_request_and_never_drops_the_branch()
{
    // 默认处置（auto）+ 不带 delete_branch：分支有自己的提交，必须还在（以前一律 `git branch -D`）
    let mut f = Fx::new().await;
    let (s, branch) = rebuilding_session(&mut f, "rebuild auto").await;
    delete(&mut f, &s.id, None, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await; // 重建（停顿 1.5 秒）做完，按删除请求收尾
    assert!(branch_exists(&f, &branch), "the branch with commits was deleted by the delete");
    assert!(worktrees(&mut f).await.iter().all(|w| w.session_id.is_none()), "the session is gone");
    f.td.shutdown().await;

    // keep：重建好的 worktree 留下来，成为「没有会话的 worktree」
    let mut f = Fx::new().await;
    let (s, branch) = rebuilding_session(&mut f, "rebuild keep").await;
    let dir = f.dir_of(&s);
    delete(&mut f, &s.id, Some("keep"), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(branch_exists(&f, &branch));
    assert!(dir.join("valuable.txt").is_file(), "the rebuilt worktree was kept");
    let list = worktrees(&mut f).await;
    let w = list.iter().find(|w| w.branch == branch).expect("the kept worktree is listed");
    assert_eq!((w.session_id.as_deref(), w.state), (None, WorktreeState::Kept));
    f.td.shutdown().await;

    // 明确要求 delete_branch：照做
    let mut f = Fx::new().await;
    let (s, branch) = rebuilding_session(&mut f, "rebuild branch").await;
    delete(&mut f, &s.id, Some("remove"), true).await.unwrap();
    wait_until(15, "the branch to be deleted as requested", || !branch_exists(&f, &branch)).await;
    f.td.shutdown().await;
}

// =====================================================================================================================
// `session.diff` 的数行数预算
// =====================================================================================================================

fn write_many(dir: &Path, n: usize, bytes_each: usize) {
    std::fs::create_dir_all(dir.join("many")).unwrap();
    let body = format!("{}\n", "x".repeat(bytes_each - 1));
    for i in 0..n {
        std::fs::write(dir.join(format!("many/f{i:04}.txt")), &body).unwrap();
    }
}

#[tokio::test]
async fn counting_lines_of_untracked_files_stays_within_a_byte_budget() {
    let mut f = Fx::new().await;
    let s = f.ready("byte budget").await;
    write_many(&f.dir_of(&s), 300, 1024);
    f.td.ctx.worktrees.hooks.diff_count_max_bytes.store(20 * 1024, Ordering::SeqCst);
    let before = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst);
    let d = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!(d.files.len(), 300, "every file is still listed");
    let counted = d.files.iter().filter(|x| x.additions > 0).count();
    let reads = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst) - before;
    assert!((1..=25).contains(&counted), "only about the budget was counted, not all 300: {counted}");
    assert!(reads <= 25, "the files past the budget were read anyway: {reads}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn counting_lines_of_untracked_files_stops_at_the_time_budget() {
    let mut f = Fx::new().await;
    let s = f.ready("time budget").await;
    write_many(&f.dir_of(&s), 900, 64);
    f.td.ctx.worktrees.hooks.diff_count_budget_ms.store(1, Ordering::SeqCst);
    let before = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst);
    let d = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!(d.files.len(), 900);
    let reads = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst) - before;
    assert!(reads < 900, "a 1 ms budget cannot have read every file: {reads}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_list_is_merged_sorted_and_cut_to_a_thousand_before_any_file_is_read() {
    let mut f = Fx::new().await;
    let s = f.ready("thousand files").await;
    write_many(&f.dir_of(&s), 1100, 8);
    let before = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst);
    let d = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!((d.files.len(), d.truncated), (1000, true));
    let reads = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst) - before;
    assert_eq!(reads, 1000, "the 100 files cut from the list must not be read");
    // 列表是按路径排序的前 1000 个
    assert_eq!(d.files[0].path, "many/f0000.txt");
    assert_eq!(d.files[999].path, "many/f0999.txt");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_single_file_request_checks_the_name_and_reads_only_that_file() {
    let mut f = Fx::new().await;
    let s = f.ready("single file").await;
    write_many(&f.dir_of(&s), 400, 512);
    let before = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst);
    let d = diff(&mut f, &s.id, "uncommitted", Some("many/f0123.txt")).await.unwrap();
    assert_eq!(d.files.len(), 1);
    assert!(d.files[0].patch.as_deref().unwrap().starts_with("diff --git a/many/f0123.txt"));
    assert_eq!(d.files[0].additions, 1);
    let reads = f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst) - before;
    assert!(reads <= 1, "a single-file request read {reads} files");
    // 名字不在列表里：not_found，也不读
    let e = diff(&mut f, &s.id, "uncommitted", Some("many/nope.txt")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert_eq!(f.td.ctx.worktrees.hooks.untracked_reads.load(Ordering::SeqCst) - before, reads);
    f.td.shutdown().await;
}

// =====================================================================================================================
// `.git` 指针文件的纵深防御
// =====================================================================================================================

#[tokio::test]
async fn a_changed_git_pointer_file_is_treated_like_a_swapped_worktree() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let s = f.ready("pointer file").await;
    let w = s.worktree.clone().unwrap();
    let dir = f.dir_of(&s);
    // 创建时记下的指针（项目目录的绝对路径换成了 `{project}`）
    let recorded = row_of(&f, &w.id).await.git_pointer.expect("the pointer was recorded");
    assert!(recorded.starts_with("gitdir: {project}/.git/worktrees/"), "{recorded}");
    let original = std::fs::read_to_string(dir.join(".git")).unwrap();
    // agent 把 `.git` 改成指向它自己在 cwd 里搭的 gitdir
    let fake = dir.join("fake-gitdir");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(dir.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
    let spawns = f.td.fake.spawn_count();
    f.send(&s.id, "next turn").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Missing);
    assert_eq!(f.td.fake.spawn_count(), spawns, "no agent was started in a worktree whose .git file changed");
    // daemon 自己的 git 也不在这个目录里跑
    let e = f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("worktree_missing")));
    // 不自动重建；原样还原之后（周期检查 / 下一次 send）恢复
    std::fs::write(dir.join(".git"), &original).unwrap();
    f.send(&s.id, "after restoring").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    // turn 进行中改了：周期检查收进程
    f.td.ctx.sessions.set_recheck_every(1);
    f.send(&s.id, "needs permission").await.unwrap();
    f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
    std::fs::write(dir.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_git_pointer_check_survives_a_project_rename_and_the_repair_that_follows_it() {
    let mut f = Fx::with(&[("sessions.idle_timeout", "\"1s\"")]).await;
    let s = f.ready("pointer rename").await;
    // 空闲进程退出之后才能改名
    let t = Instant::now();
    while f.td.ctx.sessions.project_busy("proj").await {
        assert!(t.elapsed() < T, "the idle agent process never exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = std::fs::read_to_string(f.dir_of(&s).join(".git")).unwrap();
    let _: RenamedProject =
        f.call(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "proj-renamed" })).await.unwrap();
    let after =
        std::fs::read_to_string(f.td.workspace.join("proj-renamed/.anycon/worktrees/pointer-rename/.git")).unwrap();
    assert_ne!(before, after, "git worktree repair rewrote the absolute path in the pointer file");
    // 换算成 `{project}` 之后仍然等于记下的：会话照常继续，不会被当成「被换掉」
    f.send(&s.id, "after the rename").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    f.td.shutdown().await;
}
