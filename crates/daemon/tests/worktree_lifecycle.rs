//! worktree 会话的归档 / 取消归档 / 删除、目录复核（missing）、worktree 列表与清理、项目改名 / 回收站、PR 自动归档
//! （ADR-34，daemon.md § 10.1 / § 7.8）。真实 `git`，不联网。

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

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

fn branch_exists(f: &Fx, branch: &str) -> bool {
    f.branches().iter().any(|b| b == branch)
}

fn git_note(evs: &[Event], action: GitAction) -> Option<Option<String>> {
    evs.iter().find_map(|e| match e {
        Event::Git { action: a, note, .. } if *a == action => Some(note.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------- 归档

#[tokio::test]
async fn archive_auto_removes_a_clean_worktree_and_its_branch_when_it_has_no_commits_of_its_own() {
    let mut f = Fx::new().await;
    let s = f.ready("clean one").await;
    let dir = f.dir_of(&s);
    let mut watcher = f.td.client("watcher");
    // 缺省处置是 auto：只带 session_id 的旧请求照常
    archive(&mut f, &s.id, None).await.unwrap();
    assert!(!dir.exists());
    assert!(!branch_exists(&f, "anycon/clean-one"));
    assert_eq!(f.registered_worktrees().len(), 1);
    let after = f.get(&s.id).await;
    assert!(after.archived);
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Removed);
    // 进程在 worktree 动之前收掉了
    assert_eq!(f.td.fake.last_handle().shutdowns.load(Ordering::SeqCst), 1);
    // 事件 + 带完整 SessionWorktree 的 session.changed
    let evs: Vec<Event> = f.events(&s.id).await.into_iter().map(|e| e.event).collect();
    let note = git_note(&evs, GitAction::WorktreeRemoved).expect("worktree_removed");
    assert_eq!(note.as_deref(), Some("branch_deleted"));
    let notes = watcher.drain_for(Duration::from_millis(200)).await;
    let last = notes
        .iter()
        .filter(|n| n.method == methods::SESSION_CHANGED)
        .filter_map(|n| serde_json::from_value::<SessionChanged>(n.params.clone()).ok())
        .rfind(|c| c.session_id == s.id)
        .unwrap();
    assert!(last.archived);
    assert_eq!(last.worktree.unwrap().state, WorktreeState::Removed);
    // 审计
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    let a = audit.entries.iter().find(|e| e.action == "worktree.remove").expect("worktree.remove");
    assert_eq!(
        (a.detail["branch"].as_str(), a.detail["branch_deleted"].as_bool()),
        (Some("anycon/clean-one"), Some(true))
    );
    // 已经归档：再归档是空操作
    archive(&mut f, &s.id, None).await.unwrap();
    f.td.shutdown().await;
}

#[tokio::test]
async fn archive_auto_keeps_the_branch_when_it_has_commits_or_an_upstream() {
    let mut f = Fx::new().await;
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let a = f.ready("has commits").await;
    f.td.commit_file(&f.dir_of(&a), "mine.txt", "m", "my work");
    let b = f.ready("pushed branch").await;
    f.td.git(&f.dir_of(&b), &["push", "-q", "-u", "origin", "HEAD"]);
    for s in [&a, &b] {
        archive(&mut f, &s.id, Some("auto")).await.unwrap();
        assert!(!f.dir_of(s).exists());
        assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Removed);
    }
    assert!(branch_exists(&f, "anycon/has-commits"), "commits of its own");
    assert!(branch_exists(&f, "anycon/pushed-branch"), "it has an upstream");
    let evs: Vec<Event> = f.events(&a.id).await.into_iter().map(|e| e.event).collect();
    assert_eq!(git_note(&evs, GitAction::WorktreeRemoved).unwrap().as_deref(), Some("branch_kept"));
    f.td.shutdown().await;
}

#[tokio::test]
async fn archive_auto_keeps_a_worktree_with_uncommitted_changes() {
    let mut f = Fx::new().await;
    let s = f.ready("dirty one").await;
    let dir = f.dir_of(&s);
    std::fs::write(dir.join("wip.txt"), "unsaved work").unwrap();
    archive(&mut f, &s.id, Some("auto")).await.unwrap();
    let after = f.get(&s.id).await;
    assert!(after.archived);
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Kept);
    assert_eq!(std::fs::read_to_string(dir.join("wip.txt")).unwrap(), "unsaved work", "untouched");
    assert!(branch_exists(&f, "anycon/dirty-one"));
    let evs: Vec<Event> = f.events(&s.id).await.into_iter().map(|e| e.event).collect();
    assert!(git_note(&evs, GitAction::WorktreeKept).unwrap().unwrap().contains("uncommitted"));
    // 一个只改了已跟踪文件的也算
    let t = f.ready("tracked edit").await;
    std::fs::write(f.dir_of(&t).join("README.md"), "changed").unwrap();
    archive(&mut f, &t.id, None).await.unwrap();
    assert_eq!(f.get(&t.id).await.worktree.unwrap().state, WorktreeState::Kept);
    f.td.shutdown().await;
}

#[tokio::test]
async fn archive_keep_and_remove_do_what_they_say() {
    let mut f = Fx::new().await;
    let keep = f.ready("keep it").await;
    let remove = f.ready("remove it").await;
    std::fs::write(f.dir_of(&remove).join("wip.txt"), "discarded").unwrap();
    f.td.commit_file(&f.dir_of(&remove), "c.txt", "c", "a commit");
    std::fs::write(f.dir_of(&remove).join("wip2.txt"), "also discarded").unwrap();
    archive(&mut f, &keep.id, Some("keep")).await.unwrap();
    assert!(f.dir_of(&keep).is_dir(), "clean, but the user asked to keep it");
    assert_eq!(f.get(&keep.id).await.worktree.unwrap().state, WorktreeState::Kept);
    assert!(
        git_note(&f.git_events(&keep.id).await, GitAction::WorktreeKept).is_none(),
        "no event for an explicit keep"
    );
    archive(&mut f, &remove.id, Some("remove")).await.unwrap();
    assert!(!f.dir_of(&remove).exists(), "uncommitted changes are discarded");
    assert!(branch_exists(&f, "anycon/remove-it"), "the branch stays unless the session is deleted with delete_branch");
    assert_eq!(f.get(&remove.id).await.worktree.unwrap().state, WorktreeState::Removed);
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    assert!(audit.entries.iter().any(|e| e.action == "worktree.remove" && e.detail["force"] == true));
    f.td.shutdown().await;
}

#[tokio::test]
async fn archive_refuses_while_a_turn_runs_or_waits_for_an_answer() {
    let mut f = Fx::new().await;
    let s = f.ready("hello there").await;
    f.send(&s.id, "hold this turn").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Running).await;
    let e = archive(&mut f, &s.id, None).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    assert!(f.dir_of(&s).is_dir(), "a refused archive touches nothing");
    f.td.fake.last_handle().release();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    // 等审批时同样 busy（先答复；以前这里会取消 turn、把请求按拒绝处理、排队的消息无声丢掉）：什么都不动
    f.send(&s.id, "needs permission").await.unwrap();
    f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
    let pending: PermissionListResult = f.call(methods::PERMISSION_LIST, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(pending.requests.len(), 1);
    let e = archive(&mut f, &s.id, None).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
    let after = f.get(&s.id).await;
    assert!(!after.archived && after.status == SessionStatus::WaitingPermission);
    assert!(f.dir_of(&s).is_dir(), "a refused archive touches nothing");
    let pending: PermissionListResult = f.call(methods::PERMISSION_LIST, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(pending.requests.len(), 1, "the request is still there to be answered");
    // 答复（拒绝）之后 turn 收尾，就可以归档了
    let deny =
        pending.requests[0].options.iter().find(|o| o.kind == PermissionOptionKind::RejectOnce).unwrap().id.clone();
    f.call::<Empty>(
        methods::PERMISSION_RESPOND,
        json!({ "request_id": pending.requests[0].request_id, "option_id": deny }),
    )
    .await
    .unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    archive(&mut f, &s.id, None).await.unwrap();
    assert!(f.get(&s.id).await.archived);
    assert!(!f.dir_of(&s).exists());
    f.td.shutdown().await;
}

#[tokio::test]
async fn archiving_a_session_without_a_worktree_is_unchanged() {
    let mut f = Fx::new().await;
    let s = f.create(Some("proj"), Some("plain local"), None).await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    archive(&mut f, &s.id, Some("remove")).await.unwrap();
    let after = f.get(&s.id).await;
    assert!(after.archived && after.worktree.is_none());
    assert_eq!(f.td.fake.last_handle().shutdowns.load(Ordering::SeqCst), 0, "the agent process is left alone");
    unarchive(&mut f, &s.id).await.unwrap();
    assert!(!f.get(&s.id).await.archived);
    f.td.shutdown().await;
}

#[tokio::test]
async fn archive_with_the_directory_gone_or_replaced_never_touches_what_is_there_now() {
    let mut f = Fx::new().await;
    let gone = f.ready("already gone").await;
    let swapped = f.ready("swapped dir").await;
    let linked = f.ready("symlinked dir").await;
    std::fs::remove_dir_all(f.dir_of(&gone)).unwrap();
    // 被换成另一个真目录（里面有东西）
    let sd = f.dir_of(&swapped);
    f.td.git(&f.repo, &["worktree", "remove", "--force", sd.to_str().unwrap()]);
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(sd.join("not-ours.txt"), "precious").unwrap();
    // 被换成指向别处的符号链接
    let ld = f.dir_of(&linked);
    let precious = f.td.dir.path().join("precious");
    std::fs::create_dir_all(&precious).unwrap();
    std::fs::write(precious.join("keep.txt"), "keep me").unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", ld.to_str().unwrap()]);
    anycon_daemon::testutil::symlink_dir(&precious, &ld).unwrap();
    // 目录不在了：照常 `removed`；被换成别的东西的：磁盘一概不动，**也不能说它被删了**——记录保持 `missing`，事件说明目录没动
    archive(&mut f, &gone.id, Some("remove")).await.unwrap();
    assert_eq!(f.get(&gone.id).await.worktree.unwrap().state, WorktreeState::Removed);
    for s in [&swapped, &linked] {
        archive(&mut f, &s.id, Some("remove")).await.unwrap();
        let after = f.get(&s.id).await;
        assert!(after.archived, "the archive itself went through");
        assert_eq!(after.worktree.unwrap().state, WorktreeState::Missing);
        let actions = f.git_actions(&s.id).await;
        assert!(!actions.contains(&GitAction::WorktreeRemoved), "{actions:?}");
        let last_missing = f.git_events(&s.id).await.into_iter().rev().find_map(|e| match e {
            Event::Git { action: GitAction::WorktreeMissing, note, .. } => Some(note),
            _ => None,
        });
        assert!(
            last_missing.flatten().is_some_and(|n| n.contains("not touched")),
            "the last event says the directory was left alone"
        );
    }
    assert_eq!(std::fs::read_to_string(sd.join("not-ours.txt")).unwrap(), "precious");
    assert_eq!(std::fs::read_to_string(precious.join("keep.txt")).unwrap(), "keep me");
    assert!(std::fs::symlink_metadata(&ld).unwrap().file_type().is_symlink(), "the link itself is not removed either");
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 取消归档

#[tokio::test]
async fn unarchive_rebuilds_a_removed_worktree_from_its_branch_and_sessions_work_again() {
    let mut f = Fx::new().await;
    let s = f.ready("come back").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&dir, "kept-commit.txt", "from before", "a commit on the branch");
    archive(&mut f, &s.id, Some("remove")).await.unwrap();
    assert!(!dir.exists() && branch_exists(&f, "anycon/come-back"));
    unarchive(&mut f, &s.id).await.unwrap();
    let after = f.get(&s.id).await;
    assert!(!after.archived);
    let wt = after.worktree.clone().unwrap();
    assert_eq!((wt.state, wt.path.as_str()), (WorktreeState::Active, ".anycon/worktrees/come-back"));
    assert_eq!(std::fs::read_to_string(dir.join("kept-commit.txt")).unwrap(), "from before");
    assert!(f.git_actions(&s.id).await.contains(&GitAction::WorktreeRestored));
    // 重建之后可以继续工作：下一个 turn 在重建的目录里
    f.send(&s.id, "continue working").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert_eq!(std::fs::canonicalize(&spawns.last().unwrap().cwd).unwrap(), std::fs::canonicalize(&dir).unwrap());
    f.td.shutdown().await;
}

#[tokio::test]
async fn unarchive_recreates_a_branch_that_was_deleted_with_the_worktree() {
    let mut f = Fx::new().await;
    let s = f.ready("no commits").await;
    let base_sha = s.worktree.clone().unwrap().base_sha;
    archive(&mut f, &s.id, None).await.unwrap();
    assert!(!branch_exists(&f, "anycon/no-commits"), "auto deleted the branch: nothing of its own on it");
    // base 在这期间前进了：重建仍然从创建时的提交开分支
    f.td.commit_file(&f.repo, "later.txt", "l", "main moved on");
    unarchive(&mut f, &s.id).await.unwrap();
    let dir = f.dir_of(&s);
    assert!(branch_exists(&f, "anycon/no-commits"));
    assert_eq!(f.td.git(&dir, &["rev-parse", "HEAD"]), base_sha);
    assert!(!dir.join("later.txt").exists());
    f.td.shutdown().await;
}

#[tokio::test]
async fn unarchive_of_a_kept_worktree_just_reactivates_it_and_an_occupied_path_gets_another_name() {
    let mut f = Fx::new().await;
    let kept = f.ready("kept around").await;
    std::fs::write(f.dir_of(&kept).join("wip.txt"), "wip").unwrap();
    archive(&mut f, &kept.id, None).await.unwrap();
    assert_eq!(f.get(&kept.id).await.worktree.unwrap().state, WorktreeState::Kept);
    let before_actions = f.git_actions(&kept.id).await.len();
    unarchive(&mut f, &kept.id).await.unwrap();
    let after = f.get(&kept.id).await;
    assert_eq!(after.worktree.clone().unwrap().state, WorktreeState::Active);
    assert_eq!(
        std::fs::read_to_string(f.dir_of(&kept).join("wip.txt")).unwrap(),
        "wip",
        "the same directory, still there"
    );
    assert_eq!(f.git_actions(&kept.id).await.len(), before_actions, "nothing was rebuilt");
    // 归档（remove）之后路径被别的东西占了：换一个目录名并更新记录
    let s = f.ready("occupied").await;
    archive(&mut f, &s.id, Some("remove")).await.unwrap();
    let old = f.repo.join(".anycon/worktrees/occupied");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("squatter.txt"), "mine").unwrap();
    unarchive(&mut f, &s.id).await.unwrap();
    let wt = f.get(&s.id).await.worktree.unwrap();
    assert_eq!((wt.state, wt.path.as_str()), (WorktreeState::Active, ".anycon/worktrees/occupied-2"));
    assert!(f.repo.join(&wt.path).join("README.md").is_file());
    assert_eq!(std::fs::read_to_string(old.join("squatter.txt")).unwrap(), "mine", "what was there is untouched");
    // 之后的 turn 在新目录里
    f.send(&s.id, "go on").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert!(spawns.last().unwrap().cwd.ends_with(".anycon/worktrees/occupied-2"));
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_failed_rebuild_on_unarchive_keeps_the_session_archived() {
    let mut f = Fx::new().await;
    let s = f.ready("rebuild fails").await;
    archive(&mut f, &s.id, Some("remove")).await.unwrap();
    // `.anycon` 被换成指向别处的符号链接：不能经它建
    std::fs::remove_dir_all(f.repo.join(".anycon")).unwrap();
    let elsewhere = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    anycon_daemon::testutil::symlink_dir(&elsewhere, &f.repo.join(".anycon")).unwrap();
    let e = unarchive(&mut f, &s.id).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Internal);
    assert!(e.message.contains("symbolic link"), "{}", e.message);
    let after = f.get(&s.id).await;
    assert!(after.archived, "stays archived");
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Removed);
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    // 修好之后再来
    std::fs::remove_file(f.repo.join(".anycon")).unwrap();
    unarchive(&mut f, &s.id).await.unwrap();
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 删除

#[tokio::test]
async fn delete_disposes_the_worktree_like_archive_and_forgets_it_when_nothing_is_left() {
    let mut f = Fx::new().await;
    let clean = f.ready("delete clean").await;
    let dir = f.dir_of(&clean);
    delete(&mut f, &clean.id, None, false).await.unwrap();
    assert!(!dir.exists() && !branch_exists(&f, "anycon/delete-clean"));
    assert!(worktrees(&mut f).await.is_empty(), "worktree and branch are gone, so is the record");
    let e = f.call::<SessionSummary>(methods::SESSION_GET, json!({ "session_id": clean.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // 没带参数的旧请求
    let old = f.ready("old style delete").await;
    f.call::<Empty>(methods::SESSION_DELETE, json!({ "session_id": old.id })).await.unwrap();
    assert!(!f.dir_of(&old).exists());
    f.td.shutdown().await;
}

#[tokio::test]
async fn delete_keeps_what_has_value_as_a_worktree_without_a_session() {
    let mut f = Fx::new().await;
    // 有未提交改动：保留目录
    let dirty = f.ready("delete dirty").await;
    std::fs::write(f.dir_of(&dirty).join("wip.txt"), "wip").unwrap();
    delete(&mut f, &dirty.id, None, false).await.unwrap();
    // 分支上有自己的提交：目录删，分支留，记录留（state removed）
    let commits = f.ready("delete commits").await;
    f.td.commit_file(&f.dir_of(&commits), "c.txt", "c", "valuable");
    delete(&mut f, &commits.id, None, false).await.unwrap();
    // keep
    let keep = f.ready("delete keep").await;
    delete(&mut f, &keep.id, Some("keep"), false).await.unwrap();
    let list = worktrees(&mut f).await;
    let by = |b: &str| list.iter().find(|w| w.branch == b).unwrap_or_else(|| panic!("{b}: {list:?}")).clone();
    let (d, c, k) = (by("anycon/delete-dirty"), by("anycon/delete-commits"), by("anycon/delete-keep"));
    assert_eq!((d.state, d.session_id.clone(), d.uncommitted), (WorktreeState::Kept, None, 1));
    assert_eq!((c.state, c.session_id.clone(), c.ahead), (WorktreeState::Removed, None, 1));
    assert_eq!((k.state, k.session_id.clone()), (WorktreeState::Kept, None));
    assert!(f.repo.join(&d.path).join("wip.txt").is_file() && f.repo.join(&k.path).is_dir());
    assert!(!f.repo.join(&c.path).exists() && branch_exists(&f, "anycon/delete-commits"));
    f.td.shutdown().await;
}

#[tokio::test]
async fn delete_with_delete_branch_removes_the_branch_even_with_commits() {
    let mut f = Fx::new().await;
    let s = f.ready("drop everything").await;
    f.td.commit_file(&f.dir_of(&s), "c.txt", "c", "unmerged work");
    std::fs::write(f.dir_of(&s).join("wip.txt"), "wip").unwrap();
    delete(&mut f, &s.id, Some("remove"), true).await.unwrap();
    assert!(!f.dir_of(&s).exists());
    assert!(!branch_exists(&f, "anycon/drop-everything"));
    assert!(worktrees(&mut f).await.is_empty());
    // delete_branch 在 worktree 没被删掉时不生效
    let kept = f.ready("kept with flag").await;
    std::fs::write(f.dir_of(&kept).join("wip.txt"), "wip").unwrap();
    delete(&mut f, &kept.id, Some("auto"), true).await.unwrap();
    assert!(f.dir_of(&kept).is_dir() && branch_exists(&f, "anycon/kept-with-flag"));
    let k = &worktrees(&mut f).await[0];
    assert_eq!(k.state, WorktreeState::Kept);
    f.td.shutdown().await;
}

#[tokio::test]
async fn deleting_a_session_while_its_worktree_is_being_created_cleans_everything_up() {
    let mut f = Fx::new().await;
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(800, Ordering::SeqCst);
    let s = f.create_wt("delete during creation").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    delete(&mut f, &s.id, None, false).await.unwrap();
    // 建完发现会话没了：目录、分支、记录都清掉，agent 没有被拉起（清理是异步的，等它做完）
    let t = std::time::Instant::now();
    loop {
        let gone = !f.repo.join(".anycon/worktrees/delete-during-creation").exists()
            && !f.branches().iter().any(|b| b.starts_with("anycon/"))
            && worktrees(&mut f).await.is_empty();
        if gone {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "the half-made worktree was never cleaned up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(f.td.fake.spawn_count(), 0);
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 目录复核

#[tokio::test]
async fn a_deleted_worktree_directory_is_noticed_and_rebuilt_on_the_next_send() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000); // 先关掉周期检查，单独验证 send 时的复核
    let s = f.ready("rebuild on send").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&dir, "committed.txt", "kept in the branch", "survives");
    std::fs::write(dir.join("uncommitted.txt"), "lost with the directory").unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    assert!(!dir.exists());
    // 发现这个的 send：turn 以 not_found（worktree_missing）结束，状态 missing，发 worktree_missing
    f.send(&s.id, "first try").await.unwrap();
    let after = f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    f.wait_status(&after.id, SessionStatus::Error).await;
    let evs = f.events(&s.id).await;
    let err = evs.iter().rev().find_map(|e| match &e.event {
        Event::Error { code, message, .. } => Some((code.clone(), message.clone())),
        _ => None,
    });
    let (c, m) = err.unwrap();
    assert_eq!(c, "not_found");
    assert!(m.contains("recreate it from branch anycon/rebuild-on-send"), "{m}");
    assert!(f.git_actions(&s.id).await.contains(&GitAction::WorktreeMissing));
    // 下一次 send：路径仍然不存在，prune 之后按分支重建，发 worktree_restored，再继续这个 turn
    f.send(&s.id, "second try").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert!(dir.join("committed.txt").is_file(), "rebuilt from the branch");
    assert!(!dir.join("uncommitted.txt").exists());
    assert_eq!(f.git_actions(&s.id).await.iter().filter(|a| **a == GitAction::WorktreeRestored).count(), 1);
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert_eq!(std::fs::canonicalize(&spawns.last().unwrap().cwd).unwrap(), std::fs::canonicalize(&dir).unwrap());
    // 新目录是新身份：再次复核通过
    f.send(&s.id, "third try").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_periodic_check_notices_a_missing_directory_and_closes_the_idle_process() {
    let mut f = Fx::new().await;
    let s = f.ready("periodic one").await;
    let handle = f.td.fake.last_handle();
    let dir = f.dir_of(&s);
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    // 不发消息：约 2 秒内被发现、进程收掉
    let after = f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    assert_ne!(after.status, SessionStatus::Error, "an idle session stays as it was");
    let t = std::time::Instant::now();
    while handle.shutdowns.load(Ordering::SeqCst) == 0 {
        assert!(t.elapsed() < T, "the idle agent process was never closed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 下一次 send 直接重建（状态已经是 missing，路径不存在）
    f.send(&s.id, "after the periodic check").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert!(dir.join("README.md").is_file());
    assert_eq!(f.td.fake.spawn_count(), 2, "a new process in the rebuilt directory");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_replaced_worktree_directory_is_never_rebuilt_over_and_never_followed() {
    let mut f = Fx::new().await;
    let s = f.ready("replaced dir").await;
    let dir = f.dir_of(&s);
    // 换成指向别处的符号链接
    let elsewhere = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("secret.txt"), "outside").unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    anycon_daemon::testutil::symlink_dir(&elsewhere, &dir).unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    for _ in 0..2 {
        f.send(&s.id, "try anyway").await.unwrap();
        f.wait_status(&s.id, SessionStatus::Error).await;
        let evs = f.events(&s.id).await;
        let m = evs.iter().rev().find_map(|e| match &e.event {
            Event::Error { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(m.unwrap().contains("move that path away"), "the message says what to do");
    }
    assert_eq!(f.td.fake.spawn_count(), 1, "no agent was started in or through the replaced path");
    assert!(std::fs::symlink_metadata(&dir).unwrap().file_type().is_symlink(), "not rebuilt over");
    assert_eq!(std::fs::read_to_string(elsewhere.join("secret.txt")).unwrap(), "outside");
    // 被换成另一个真目录也一样
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("other.txt"), "someone else's directory").unwrap();
    f.send(&s.id, "and now?").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.td.fake.spawn_count(), 1);
    assert_eq!(std::fs::read_to_string(dir.join("other.txt")).unwrap(), "someone else's directory");
    // 把那个路径移走之后再发消息：按分支重建
    std::fs::remove_dir_all(&dir).unwrap();
    f.send(&s.id, "now it is free").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert!(dir.join("README.md").is_file());
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_swapped_worktree_blocks_permission_grants_and_interjections() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let s = f.ready("perm session").await;
    let dir = f.dir_of(&s);
    f.send(&s.id, "needs permission").await.unwrap();
    f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
    let pending: PermissionListResult = f.call(methods::PERMISSION_LIST, json!({ "session_id": s.id })).await.unwrap();
    let req = pending.requests[0].clone();
    // 目录在审批期间被换成指向别处的符号链接
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
    // agent 收到的是拒绝；turn 以 not_found 结束，状态 missing
    let decisions = f.td.fake.last_handle().decisions.lock().unwrap().clone();
    assert_eq!(decisions.len(), 1);
    assert_ne!(decisions[0].1.option_id.as_deref(), Some(allow.as_str()));
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Missing);
    // 请求被消耗了：再答复是 already_resolved
    let e = f
        .call::<Empty>(methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": allow }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyResolved);
    f.td.shutdown().await;
}

#[tokio::test]
async fn interjecting_into_a_swapped_worktree_is_refused_and_the_message_waits() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    f.td.fake.interject.store(true, Ordering::SeqCst);
    f.td.ctx.agents.refresh_all().await;
    let s = f.ready("interject target").await;
    let dir = f.dir_of(&s);
    f.send(&s.id, "hold the turn").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Running).await;
    let inputs_before = f.td.fake.last_handle().inputs.lock().unwrap().len();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    let r = f.send(&s.id, "late instruction").await.unwrap();
    assert!(r.queued, "not delivered to the agent");
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(
        f.td.fake.last_handle().inputs.lock().unwrap().len(),
        inputs_before,
        "the interjection never reached the agent"
    );
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Missing);
    // 消息没丢：重建之后和下一条一起发出
    f.send(&s.id, "next one").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let texts = f.td.fake.last_handle().input_texts().join("\n");
    assert!(texts.contains("late instruction") && texts.contains("next one"), "{texts}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_live_idle_process_is_not_reused_after_the_directory_was_swapped() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let s = f.ready("reuse check").await;
    let dir = f.dir_of(&s);
    let elsewhere = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    anycon_daemon::testutil::symlink_dir(&elsewhere, &dir).unwrap();
    let inputs_before = f.td.fake.last_handle().inputs.lock().unwrap().len();
    f.send(&s.id, "must not reach the old process").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.td.fake.last_handle().inputs.lock().unwrap().len(), inputs_before);
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 列表与清理

#[tokio::test]
async fn the_worktree_list_computes_state_changes_commits_size_and_session_titles() {
    let mut f = Fx::new().await;
    let a = f.ready("list me").await;
    let b = f.ready("and me").await;
    f.td.commit_file(&f.dir_of(&a), "c.txt", "c", "one commit");
    std::fs::write(f.dir_of(&a).join("dirty.txt"), "d").unwrap();
    let list = worktrees(&mut f).await;
    assert_eq!(list.len(), 2);
    let wa = list.iter().find(|w| w.id == a.worktree.as_ref().unwrap().id).unwrap();
    assert_eq!((wa.state, wa.ahead, wa.uncommitted), (WorktreeState::Active, 1, 1));
    assert_eq!(wa.session_id.as_deref(), Some(a.id.as_str()));
    assert!(wa.session_title.as_deref().is_some_and(|t| t.contains("list me")), "{:?}", wa.session_title);
    assert!(wa.bytes.is_some_and(|n| n > 0));
    assert_eq!((wa.project.as_str(), wa.path.as_str()), ("proj", ".anycon/worktrees/list-me"));
    // 目录不在了的 active 项按 missing 报
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    f.td.git(&f.repo, &["worktree", "remove", "--force", f.dir_of(&b).to_str().unwrap()]);
    let list = worktrees(&mut f).await;
    let wb = list.iter().find(|w| w.id == b.worktree.as_ref().unwrap().id).unwrap();
    assert_eq!(wb.state, WorktreeState::Missing);
    assert_eq!((wb.uncommitted, wb.bytes), (0, None));
    // 按项目过滤
    let r: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({ "project": "proj" })).await.unwrap();
    assert_eq!(r.worktrees.len(), 2);
    let r: WorkspaceWorktreesResult =
        f.call(methods::WORKSPACE_WORKTREES, json!({ "project": "other" })).await.unwrap();
    assert!(r.worktrees.is_empty());
    f.td.shutdown().await;
}

#[tokio::test]
async fn remove_worktree_is_busy_for_live_sessions_and_needs_force_for_uncommitted_changes() {
    let mut f = Fx::new().await;
    let s = f.ready("to be removed").await;
    let id = s.worktree.clone().unwrap().id;
    let rm = |force: bool, delete_branch: bool| json!({ "id": id, "force": force, "delete_branch": delete_branch });
    // 会话没归档
    assert_eq!(
        code(&f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, rm(true, false)).await.unwrap_err()),
        ErrorCode::Busy
    );
    std::fs::write(f.dir_of(&s).join("wip.txt"), "wip").unwrap();
    archive(&mut f, &s.id, Some("keep")).await.unwrap();
    // 有未提交改动而没 force
    let e = f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, rm(false, false)).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("uncommitted_changes")));
    assert!(f.dir_of(&s).is_dir());
    // force：目录删了，分支还在（没要求删）；会话还在，记录置 removed
    f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, rm(true, false)).await.unwrap();
    assert!(!f.dir_of(&s).exists() && branch_exists(&f, "anycon/to-be-removed"));
    let after = f.get(&s.id).await;
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Removed);
    let l = worktrees(&mut f).await;
    assert_eq!(l[0].state, WorktreeState::Removed);
    // 再删一次：只删分支，并且会话已经归档着，记录依然是 removed
    f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, rm(false, true)).await.unwrap();
    assert!(!branch_exists(&f, "anycon/to-be-removed"));
    // 不存在的 id
    let e = f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": "wt_nope" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    assert!(audit.entries.iter().filter(|e| e.action == "worktree.remove").count() >= 2);
    f.td.shutdown().await;
}

#[tokio::test]
async fn remove_worktree_of_a_deleted_session_drops_the_record_and_only_touches_recorded_directories() {
    let mut f = Fx::new().await;
    let s = f.ready("orphan").await;
    let id = s.worktree.clone().unwrap().id;
    std::fs::write(f.dir_of(&s).join("wip.txt"), "wip").unwrap();
    delete(&mut f, &s.id, Some("keep"), false).await.unwrap();
    assert_eq!(worktrees(&mut f).await[0].state, WorktreeState::Kept);
    // 记录的目录被换成了别的目录：不碰磁盘，只删记录
    let dir = f.repo.join(".anycon/worktrees/orphan");
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("precious.txt"), "not anycon's").unwrap();
    f.call::<Empty>(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": id, "force": true, "delete_branch": true }))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("precious.txt")).unwrap(), "not anycon's");
    assert!(!branch_exists(&f, "anycon/orphan"));
    assert!(worktrees(&mut f).await.is_empty(), "the session is gone, so the record goes too");
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 项目改名 / 回收站

#[tokio::test]
async fn renaming_the_project_repairs_the_worktrees() {
    let mut f = Fx::with(&[("sessions.idle_timeout", "\"1s\"")]).await;
    let s = f.ready("survive rename").await;
    // 空闲进程退出之后才能改名
    let t = std::time::Instant::now();
    while f.td.ctx.sessions.project_busy("proj").await {
        assert!(t.elapsed() < T, "the idle agent process never exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _: RenamedProject =
        f.call(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" })).await.unwrap();
    let new_repo = std::fs::canonicalize(f.td.workspace.join("renamed")).unwrap();
    let wt_dir = new_repo.join(".anycon/worktrees/survive-rename");
    // git 在 worktree 里能用（没修的话 `.git` 文件还指着旧路径）
    assert_eq!(f.td.git(&wt_dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "anycon/survive-rename");
    assert!(f
        .td
        .git(&new_repo, &["worktree", "list", "--porcelain"])
        .contains("/renamed/.anycon/worktrees/survive-rename"));
    let after = f.get(&s.id).await;
    assert_eq!(after.project.as_deref(), Some("renamed"));
    assert_eq!(after.worktree.unwrap().path, ".anycon/worktrees/survive-rename");
    let list = worktrees(&mut f).await;
    assert_eq!(list[0].project, "renamed");
    // 会话照常继续：cwd 在改名后的项目里
    f.send(&s.id, "still works").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert_eq!(std::fs::canonicalize(&spawns.last().unwrap().cwd).unwrap(), wt_dir);
    let st: SessionGitStatus = f.call(methods::SESSION_GIT_STATUS, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(st.branch.as_deref(), Some("anycon/survive-rename"));
    f.td.shutdown().await;
}

#[tokio::test]
async fn trashing_a_project_with_unarchived_worktree_sessions_is_busy_and_restore_repairs() {
    let mut f = Fx::with(&[("sessions.idle_timeout", "\"1s\"")]).await;
    let s = f.ready("trash guard").await;
    let t = std::time::Instant::now();
    while f.td.ctx.sessions.project_busy("proj").await {
        assert!(t.elapsed() < T);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 进程已经退出，但会话没归档、有 worktree：不能进回收站
    let e = f.call::<TrashEntry>(methods::WORKSPACE_TRASH, json!({ "name": "proj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    assert!(f.repo.join(".anycon").is_dir());
    // 归档（保留 worktree）之后可以；worktree 跟着项目进回收站，恢复之后修复
    archive(&mut f, &s.id, Some("keep")).await.unwrap();
    let entry: TrashEntry = f.call(methods::WORKSPACE_TRASH, json!({ "name": "proj" })).await.unwrap();
    assert!(!f.repo.exists());
    let _: Project = f.call(methods::WORKSPACE_RESTORE, json!({ "id": entry.id })).await.unwrap();
    let wt_dir = f.repo.join(".anycon/worktrees/trash-guard");
    assert_eq!(f.td.git(&wt_dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "anycon/trash-guard");
    unarchive(&mut f, &s.id).await.unwrap();
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- PR 自动归档

#[cfg(unix)]
fn merged_json() -> String {
    json!({ "number": 9, "url": "https://example.test/pr/9", "title": "t", "state": "MERGED", "isDraft": false, "baseRefName": "main", "statusCheckRollup": [] }).to_string()
}

#[cfg(unix)]
fn state_json(state: &str) -> String {
    merged_json().replace("MERGED", state)
}

#[cfg(unix)]
fn add_remote(f: &Fx) {
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
}

#[cfg(unix)]
#[tokio::test]
async fn closed_prs_archive_their_sessions_only_when_enabled_and_only_eligible_ones() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let pushed = f.ready("pushed branch").await;
    f.td.git(&f.dir_of(&pushed), &["push", "-q", "-u", "origin", "HEAD"]);
    let unpushed = f.ready("local only branch").await;
    let running = f.ready("running one").await;
    f.td.git(&f.dir_of(&running), &["push", "-q", "-u", "origin", "HEAD"]);
    f.send(&running.id, "hold this turn").await.unwrap();
    f.wait_status(&running.id, SessionStatus::Running).await;
    f.td.set_gh(&state_json("MERGED"), "", 0);
    // 默认关
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    assert_eq!(f.td.gh_calls(), 0);
    anycon_daemon::config::set_value(&f.td.paths, "worktree.auto_archive_on_pr_close", "true").unwrap();
    f.td.ctx.config.reload().unwrap();
    // OPEN：不动
    f.td.set_gh(&state_json("OPEN"), "", 0);
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    assert!(f.td.gh_calls() >= 1);
    assert!(!f.get(&pushed.id).await.archived);
    // 没有 PR：不动
    f.td.set_gh("", "no pull requests found for branch", 1);
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    // MERGED：只有没归档、没有 turn、有上游的那个
    f.td.set_gh(&state_json("MERGED"), "", 0);
    let calls_before = f.td.gh_calls();
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 1);
    assert_eq!(
        f.td.gh_calls(),
        calls_before + 1,
        "only the session with an upstream and no running turn is asked about"
    );
    let after = f.get(&pushed.id).await;
    assert!(after.archived);
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Removed, "archived with auto: clean worktree removed");
    assert!(!f.get(&unpushed.id).await.archived);
    assert!(!f.get(&running.id).await.archived);
    // 审计里是 daemon 自己
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    assert!(audit.entries.iter().any(|e| e.action == "worktree.remove" && e.device_id == "daemon"));
    // CLOSED 也算；有未提交改动的 auto 归档会保留 worktree
    f.td.fake.last_handle().release();
    f.wait_status(&running.id, SessionStatus::Idle).await;
    std::fs::write(f.dir_of(&running).join("wip.txt"), "wip").unwrap();
    f.td.set_gh(&state_json("CLOSED"), "", 0);
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 1);
    let r = f.get(&running.id).await;
    assert!(r.archived);
    assert_eq!(r.worktree.unwrap().state, WorktreeState::Kept);
    f.td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn the_pr_round_is_skipped_when_gh_is_not_usable_and_asks_about_at_most_twenty_sessions() {
    let mut f = Fx::new().await;
    add_remote(&f);
    anycon_daemon::config::set_value(&f.td.paths, "worktree.auto_archive_on_pr_close", "true").unwrap();
    f.td.ctx.config.reload().unwrap();
    let mut ids = Vec::new();
    for i in 0..22 {
        let s = f.ready(&format!("batch item {}", (b'a' + i as u8) as char)).await;
        f.td.git(&f.dir_of(&s), &["push", "-q", "-u", "origin", "HEAD"]);
        ids.push(s.id);
    }
    // gh 没装 / 没登录：整轮跳过（只试一次就停）
    f.td.remove_gh();
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    f.td.set_gh("", "To get started with GitHub CLI, please run:  gh auth login", 4);
    let before = f.td.gh_calls();
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    assert_eq!(f.td.gh_calls(), before, "the fake gh was removed: nothing was called"); // remove_gh 之后文件没了
    f.td.write_gh_back();
    f.td.set_gh("", "To get started with GitHub CLI, please run:  gh auth login", 4);
    let before = f.td.gh_calls();
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    assert_eq!(f.td.gh_calls(), before + 1, "not logged in: one try, then the whole round is skipped");
    // 每轮最多 20 个
    f.td.set_gh(&state_json("MERGED"), "", 0);
    let n1 = f.td.ctx.sessions.auto_archive_round().await;
    assert_eq!(n1, 20);
    let n2 = f.td.ctx.sessions.auto_archive_round().await;
    assert_eq!(n2, 2, "the next round picks up the rest");
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 关联项目（ADR-33 补充）

#[cfg(unix)]
#[tokio::test]
async fn worktrees_work_in_a_linked_project_and_follow_it_when_it_is_linked_again_under_another_name() {
    let mut f = Fx::new().await;
    let machine = std::fs::canonicalize(f.td.dir.path()).unwrap().join("machine");
    let linked = machine.join("elsewhere-repo");
    std::fs::create_dir_all(&linked).unwrap();
    f.td.git(&linked, &["init", "-q", "-b", "main"]);
    f.td.commit_file(&linked, "README.md", "# linked\n", "initial");
    f.td.commit_file(&linked, "keep.txt", "untouched\n", "second");
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&f.td.paths, "workspace.link_roots", &roots).unwrap();
    f.td.ctx.config.reload().unwrap();
    let _: Project =
        f.call(methods::WORKSPACE_LINK, json!({ "path": linked.to_string_lossy(), "name": "ln" })).await.unwrap();
    let s = f.create(Some("ln"), Some("linked worktree"), Some(json!({ "mode": "worktree" }))).await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    // 目录在关联目录里的 .anycon/worktrees 下；info/exclude 写了一行；别的文件都没动
    let wt_dir = linked.join(".anycon/worktrees/linked-worktree");
    assert!(wt_dir.join("keep.txt").is_file());
    let exclude = std::fs::read_to_string(linked.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|l| l.trim() == "/.anycon/"), "{exclude}");
    assert_eq!(f.td.git(&linked, &["status", "--porcelain"]), "");
    // 归档（auto，干净且没有自己的提交）：worktree 和分支删掉，关联目录里其余文件原样
    archive(&mut f, &s.id, None).await.unwrap();
    assert!(!wt_dir.exists());
    assert_eq!(std::fs::read_to_string(linked.join("keep.txt")).unwrap(), "untouched\n");
    assert_eq!(std::fs::read_to_string(linked.join("README.md")).unwrap(), "# linked\n");
    // 解除关联，换个名字关联回来：会话跟着改名，worktree 的登记行也是
    let _: Empty = f.call(methods::WORKSPACE_UNLINK, json!({ "name": "ln" })).await.unwrap();
    let _: Project =
        f.call(methods::WORKSPACE_LINK, json!({ "path": linked.to_string_lossy(), "name": "ln2" })).await.unwrap();
    assert_eq!(f.get(&s.id).await.project.as_deref(), Some("ln2"));
    let r: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({ "project": "ln2" })).await.unwrap();
    assert_eq!(r.worktrees.len(), 1, "the record follows the session");
    assert_eq!(r.worktrees[0].project, "ln2");
    // 取消归档：按分支重建，仍在关联目录里
    unarchive(&mut f, &s.id).await.unwrap();
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    assert!(wt_dir.join("keep.txt").is_file());
    f.td.shutdown().await;
}

// ---------------------------------------------------------------------------------------- 重建也复制 .worktreeinclude

/// 被忽略的本地文件（`.env`、`node_modules/`）+ 一个指向项目外的符号链接，都列在 `.worktreeinclude` 里。
fn setup_includes(f: &Fx) {
    f.td.commit_file(&f.repo, ".gitignore", ".env\nnode_modules/\nlink.txt\n", "ignore");
    f.td.commit_file(&f.repo, ".worktreeinclude", ".env\nnode_modules/\nlink.txt\n", "include");
    std::fs::write(f.repo.join(".env"), "TOKEN=dummy-value\n").unwrap();
    std::fs::create_dir_all(f.repo.join("node_modules/pkg")).unwrap();
    std::fs::write(f.repo.join("node_modules/pkg/index.js"), "module.exports = 1\n").unwrap();
    let outside = f.td.dir.path().join("outside.txt");
    std::fs::write(&outside, "do not copy").unwrap();
    anycon_daemon::testutil::symlink_file(&outside, &f.repo.join("link.txt")).unwrap();
}

fn assert_included(dir: &std::path::Path) {
    assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), "TOKEN=dummy-value\n");
    assert!(dir.join("node_modules/pkg/index.js").is_file());
    assert!(std::fs::symlink_metadata(dir.join("link.txt")).is_err(), "symbolic links are skipped when rebuilding too");
}

fn restored_event_of(evs: &[Event]) -> (Option<u32>, Option<String>) {
    evs.iter()
        .rev()
        .find_map(|e| match e {
            Event::Git { action: GitAction::WorktreeRestored, count, note, .. } => Some((*count, note.clone())),
            _ => None,
        })
        .expect("a worktree_restored event")
}

#[tokio::test]
async fn unarchive_copies_the_worktreeinclude_files_into_the_rebuilt_worktree() {
    let mut f = Fx::new().await;
    setup_includes(&f);
    let s = f.ready("rebuild with includes").await;
    let dir = f.dir_of(&s);
    assert_included(&dir);
    // auto 归档：被忽略的文件不算未提交，干净的 worktree 连 .env 一起删掉
    archive(&mut f, &s.id, None).await.unwrap();
    assert!(!dir.exists());
    unarchive(&mut f, &s.id).await.unwrap();
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    assert_included(&dir);
    let evs: Vec<Event> = f.events(&s.id).await.into_iter().map(|e| e.event).collect();
    assert_eq!(restored_event_of(&evs), (Some(2), None), "the number of copied files is on the event");
    // 超限：和创建时一样停下，note 说明没复制完
    archive(&mut f, &s.id, None).await.unwrap();
    anycon_daemon::config::set_value(&f.td.paths, "worktree.include_max_files", "1").unwrap();
    f.td.ctx.config.reload().unwrap();
    unarchive(&mut f, &s.id).await.unwrap();
    let evs: Vec<Event> = f.events(&s.id).await.into_iter().map(|e| e.event).collect();
    let (count, note) = restored_event_of(&evs);
    assert_eq!(count, Some(1));
    assert!(note.is_some_and(|n| n.contains("stopped copying")));
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_worktree_rebuilt_on_the_next_send_gets_the_worktreeinclude_files_too() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    setup_includes(&f);
    let s = f.ready("rebuild on send with includes").await;
    let dir = f.dir_of(&s);
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    assert!(!dir.exists());
    // 发现目录不见了的那次 send 以 not_found 结束；下一次 send 重建
    f.send(&s.id, "first try").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Missing).await;
    f.wait_status(&s.id, SessionStatus::Error).await;
    f.send(&s.id, "second try").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_included(&dir);
    let evs: Vec<Event> = f.events(&s.id).await.into_iter().map(|e| e.event).collect();
    assert_eq!(restored_event_of(&evs).0, Some(2));
    f.td.shutdown().await;
}
