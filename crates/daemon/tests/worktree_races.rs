//! 每会话 worktree 的竞争与收尾：改名与 `git worktree repair`、冷缓存的等待、PR 自动归档的复查、等审批时不能归档、
//! 失败创建占着的名字、合并提交的 base 展示名。真实 `git`，不联网；慢 / 失败的 git 和慢 `gh`
//! 用 PATH 里的包装脚本制造（`TestDaemon::slow_git` / `fail_git` / `set_gh_sleep`），不依赖机器负载。
//!
//! 每个用例都对着一个具体的现象：去掉对应的修复就红。

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use common::wt::*;
use serde_json::json;
use tokio::sync::mpsc;

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

fn add_remote(f: &Fx) -> PathBuf {
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    remote
}

async fn wait_until<F: FnMut() -> bool>(secs: u64, what: &str, mut f: F) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 空闲、没有 agent 进程的 worktree 会话（项目才能改名）。
async fn idle_session_without_a_process(f: &mut Fx, prompt: &str) -> SessionSummary {
    let s = f.ready(prompt).await;
    let _: Empty = f.call(methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    s
}

/// 在后台发一条 `workspace.rename`（经 rpc 调度器，和真实客户端一样）。
fn rename_in_the_background(f: &Fx, from: &str, to: &str) -> tokio::task::JoinHandle<RpcMessage> {
    let (rpc, handle) = (f.td.rpc(), f.td.client("renamer").handle.clone());
    let params = json!({ "from": from, "to": to });
    tokio::spawn(async move {
        let req =
            RpcRequest { jsonrpc: "2.0".into(), id: RpcId::Num(7), method: methods::WORKSPACE_RENAME.into(), params };
        rpc.handle_request(&handle, req).await
    })
}

async fn missing_events(f: &mut Fx, sid: &str) -> usize {
    f.git_actions(sid).await.iter().filter(|a| **a == GitAction::WorktreeMissing).count()
}

// =====================================================================================================================
// 改名 / `git worktree repair` / 指针比较 / 处置「被换掉」
// =====================================================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_rename_never_lets_the_periodic_check_see_the_half_repaired_worktree() {
    let mut f = Fx::new().await;
    let s = idle_session_without_a_process(&mut f, "rename window").await;
    f.td.ctx.sessions.set_recheck_every(1); // 每 0.5 秒一次周期检查，窗口里一定会碰上
                                            // repair 要 2.5 秒：改名（目录已经挪了）到 repair 完成之间就是以前的窗口
    f.td.slow_git(&[("*worktree repair*", 2.5)]);
    let renaming = rename_in_the_background(&f, "proj", "renamed");
    wait_until(10, "git worktree repair to start", || !f.td.git_hits().is_empty()).await;
    // 窗口里：会话没有被标 missing，也没有 `worktree_missing` 事件；`send` 是 busy（不是 not_found）
    for _ in 0..4 {
        let w = f.get(&s.id).await.worktree.unwrap();
        assert_eq!(w.state, WorktreeState::Active, "the periodic check saw the half-repaired worktree");
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
    let e = f.send(&s.id, "during the rename").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
    assert!(matches!(renaming.await.unwrap(), RpcMessage::Response(r) if r.error.is_none()));
    // 改名完成：会话看到新名字，worktree 还是 active，历史里一条 `worktree_missing` 都没有
    f.td.slow_git(&[]);
    let after = f.get(&s.id).await;
    assert_eq!(
        (after.project.as_deref(), after.worktree.as_ref().map(|w| w.state)),
        (Some("renamed"), Some(WorktreeState::Active))
    );
    assert_eq!(missing_events(&mut f, &s.id).await, 0);
    f.send(&s.id, "after the rename").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_failed_repair_only_asks_for_git_worktree_repair_and_archiving_never_claims_the_directory_was_removed() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_recheck_every(1_000_000); // 先关掉周期检查，单独看 send / archive
    let s = idle_session_without_a_process(&mut f, "repair fails").await;
    f.td.fail_git(&["*worktree repair*"]);
    let _: RenamedProject =
        f.call(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" })).await.unwrap();
    f.td.fail_git(&[]);
    let dir = f.td.workspace.join("renamed").join(&s.worktree.as_ref().unwrap().path);
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("wip.txt"), "uncommitted work").unwrap();
    // 指针没改写：send 得到 not_found，说明是指针不对、该运行 `git worktree repair`，而不是「把路径移走」
    f.send(&s.id, "after the failed repair").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Missing);
    let msg = f
        .events(&s.id)
        .await
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            Event::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(msg.contains("git worktree repair") && !msg.contains("move that path away"), "{msg}");
    let e = f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("worktree_missing")));
    assert!(e.message.contains("git worktree repair"), "{}", e.message);
    // 归档（auto）：磁盘不动、不写 removed、不发 worktree_removed；事件说明目录没动
    archive(&mut f, &s.id, None).await.unwrap();
    let after = f.get(&s.id).await;
    assert!(after.archived);
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Missing, "not `removed`: the directory is still there");
    assert_eq!(std::fs::read_to_string(dir.join("wip.txt")).unwrap(), "uncommitted work");
    assert!(!f.git_actions(&s.id).await.contains(&GitAction::WorktreeRemoved));
    let last = f.git_events(&s.id).await.into_iter().rev().find_map(|e| match e {
        Event::Git { action: GitAction::WorktreeMissing, note, .. } => note,
        _ => None,
    });
    assert!(last.is_some_and(|n| n.contains("git worktree repair") && n.contains("not touched")));
    // 取消归档：以前换目录名重建，因为分支还检出在原目录里而失败（会话卡在归档状态）；现在照常完成，保持 missing
    unarchive(&mut f, &s.id).await.unwrap();
    let again = f.get(&s.id).await;
    assert!(!again.archived);
    let w = again.worktree.unwrap();
    assert_eq!((w.state, w.path.as_str()), (WorktreeState::Missing, s.worktree.as_ref().unwrap().path.as_str()));
    // 用户照提示手工 repair 之后恢复（周期检查 / 下一次 send）
    f.td.git(&f.td.workspace.join("renamed"), &["worktree", "repair", dir.to_str().unwrap()]);
    f.send(&s.id, "after repairing by hand").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_eq!(std::fs::read_to_string(dir.join("wip.txt")).unwrap(), "uncommitted work");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_pointer_written_as_a_relative_path_is_the_same_pointer() {
    let mut f = Fx::new().await;
    let s = idle_session_without_a_process(&mut f, "relative paths").await;
    // 创建之后才打开 git ≥ 2.48 的 `worktree.useRelativePaths`：之后任何一次 `git worktree repair` 都把指针改写成相对路径
    let cfg = f.td.dir.path().join("gitconfig");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("[worktree]\n\tuseRelativePaths = true\n");
    std::fs::write(&cfg, text).unwrap();
    let _: RenamedProject =
        f.call(methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" })).await.unwrap();
    let dir = std::fs::canonicalize(f.td.workspace.join("renamed").join(&s.worktree.as_ref().unwrap().path)).unwrap();
    let pointer = std::fs::read_to_string(dir.join(".git")).unwrap();
    assert!(pointer.starts_with("gitdir: ../"), "repair wrote a relative pointer: {pointer}");
    // 语义上还是同一个管理目录：会话照常工作，周期检查也不误报
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Active);
    f.send(&s.id, "with a relative pointer").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_eq!(missing_events(&mut f, &s.id).await, 0);
    // agent 把指针改到别处（哪怕写成相对路径）仍然拦得住
    std::fs::write(dir.join(".git"), "gitdir: ./fake-gitdir\n").unwrap();
    f.send(&s.id, "after tampering").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    assert_eq!(f.get(&s.id).await.worktree.unwrap().state, WorktreeState::Missing);
    f.td.shutdown().await;
}

// =====================================================================================================================
// 冷缓存的等待不占串行队列；`updated` 防抖合并
// =====================================================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_workspace_list_does_not_hold_up_the_other_requests_of_the_same_client() {
    let td = TestDaemon::start_git(&[]).await;
    td.new_repo("slowproj");
    let mut owner = td.client("owner");
    let s: SessionSummary = owner
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "needs permission", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let t = Instant::now();
    loop {
        let st: SessionSummary = owner.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
        if st.status == SessionStatus::WaitingPermission {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pending: PermissionListResult = owner.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
    let req = pending.requests[0].clone();
    let allow = req.options.iter().find(|o| o.kind == PermissionOptionKind::AllowOnce).unwrap().id.clone();
    // 冷缓存的 list 要 2 秒（`git status` 睡 2 秒，在 3 秒预算之内）
    td.slow_git(&[("*slowproj*status*", 2.0)]);
    td.ctx.workspace.clear_git_cache_for_test();
    // 真实客户端的路径：每个客户端一个串行 worker
    let (tx, rx) = mpsc::unbounded_channel::<RpcRequest>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(Instant, RpcMessage)>();
    let client = td.client("pipelined").handle.clone();
    let _worker = anycon_daemon::rpc::spawn_serial_worker(td.rpc(), client, rx, move |m| {
        let _ = out_tx.send((Instant::now(), m));
    });
    let started = Instant::now();
    let send = |id: u64, method: &str, params: serde_json::Value| {
        tx.send(RpcRequest { jsonrpc: "2.0".into(), id: RpcId::Num(id), method: method.into(), params }).unwrap();
    };
    send(1, methods::WORKSPACE_LIST, json!({}));
    send(2, methods::SESSION_LIST, json!({}));
    send(3, methods::PERMISSION_LIST, json!({}));
    send(4, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": allow }));
    let mut at: std::collections::HashMap<u64, Duration> = Default::default();
    while at.len() < 4 {
        let (when, msg) = tokio::time::timeout(Duration::from_secs(15), out_rx.recv()).await.unwrap().unwrap();
        let RpcMessage::Response(r) = msg else { continue };
        let RpcId::Num(id) = r.id else { continue };
        assert!(r.error.is_none(), "request {id}: {:?}", r.error);
        at.insert(id, when.duration_since(started));
    }
    assert!(at[&1] >= Duration::from_millis(1800), "the list really waited for git: {:?}", at[&1]);
    for id in [2, 3, 4] {
        assert!(at[&id] < Duration::from_millis(1000), "request {id} was held up by the cold list: {:?}", at[&id]);
    }
    td.shutdown().await;
}

#[tokio::test]
async fn the_updated_notifications_of_projects_that_finish_together_are_merged_into_one() {
    let td = TestDaemon::start_git(&[]).await;
    for n in ["mergea", "mergeb", "mergec"] {
        td.new_repo(n);
    }
    td.ctx.workspace.set_git_cold_budget_for_test(Duration::from_millis(300));
    td.slow_git(&[("*merge*status*", 1.2)]);
    let mut c = td.client("a");
    let l: WorkspaceListResult = c.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(l.projects.iter().all(|p| p.git.is_none() && p.git_pending), "all three are still being computed: {l:?}");
    // 三个项目差不多同时算完：200 ms 防抖，合并成一条 `workspace.changed`
    let updated_events = |notes: Vec<RpcNotification>| -> Vec<Vec<String>> {
        notes
            .into_iter()
            .filter(|n| n.method == methods::WORKSPACE_CHANGED)
            .map(|n| serde_json::from_value::<WorkspaceChanged>(n.params).unwrap().updated)
            .filter(|u| !u.is_empty())
            .collect()
    };
    let mut all: Vec<Vec<String>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while all.iter().flatten().count() < 3 && Instant::now() < deadline {
        all.extend(updated_events(c.drain_for(Duration::from_millis(300)).await));
    }
    all.extend(updated_events(c.drain_for(Duration::from_millis(600)).await));
    assert_eq!(all.len(), 1, "one notification, not one per project: {all:?}");
    let mut names = all[0].clone();
    names.sort();
    assert_eq!(names, ["mergea", "mergeb", "mergec"]);
    td.shutdown().await;
}

// =====================================================================================================================
// daemon 自己发起的归档在会话锁里复查
// =====================================================================================================================

fn merged_pr_json() -> String {
    json!({ "number": 9, "url": "https://example.test/pr/9", "title": "t", "state": "MERGED", "isDraft": false, "baseRefName": "main", "statusCheckRollup": [] }).to_string()
}

fn enable_auto_archive(f: &Fx) {
    anycon_daemon::config::set_value(&f.td.paths, "worktree.auto_archive_on_pr_close", "true").unwrap();
    f.td.ctx.config.reload().unwrap();
}

#[tokio::test]
async fn the_pr_round_skips_a_session_that_got_queued_messages_after_it_was_picked() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let s = f.ready("queued message").await;
    f.td.git(&f.dir_of(&s), &["push", "-q", "-u", "origin", "HEAD"]);
    let dir = f.dir_of(&s);
    // 目录被换掉期间发的消息被挡下（turn 以 not_found 结束），消息留在队列里；目录回来之后会话恢复成 active：
    // 没有 turn、没有进程在跑，但还有一条没发出去的消息
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let moved = dir.with_file_name("queued-message-moved");
    std::fs::rename(&dir, &moved).unwrap();
    std::os::unix::fs::symlink(f.td.dir.path(), &dir).unwrap();
    f.send(&s.id, "this message is blocked").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Error).await;
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&moved, &dir).unwrap();
    f.td.ctx.sessions.set_recheck_every(1);
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    // PR 已合并：自动归档一轮——会话有排队的消息，跳过，留到下一轮（以前会归档、消息无声地躺在队列里）
    enable_auto_archive(&f);
    f.td.set_gh(&merged_pr_json(), "", 0);
    assert_eq!(f.td.ctx.sessions.auto_archive_round().await, 0);
    assert!(f.td.gh_calls() >= 1, "the session was a candidate and gh was asked");
    let after = f.get(&s.id).await;
    assert!(!after.archived, "the daemon archived a session that has a message waiting");
    assert_eq!(after.worktree.unwrap().state, WorktreeState::Active);
    f.td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pr_round_does_not_archive_sessions_that_started_waiting_for_an_answer_while_it_was_running() {
    let mut f = Fx::new().await;
    add_remote(&f);
    f.td.ctx.sessions.set_recheck_every(1_000_000);
    let a = f.ready("first candidate").await;
    let b = f.ready("second candidate").await;
    for s in [&a, &b] {
        f.td.git(&f.dir_of(s), &["push", "-q", "-u", "origin", "HEAD"]);
    }
    enable_auto_archive(&f);
    f.td.set_gh(&merged_pr_json(), "", 0);
    f.td.set_gh_sleep(1.5); // 每次 `gh` 要 1.5 秒：选完候选之后、轮到每个会话之前有足够的时间
    f.td.ctx.sessions.spawn_auto_archive_round();
    wait_until(10, "the round to ask gh", || f.td.gh_calls() >= 1).await;
    // 候选已经选好了；这时两个会话都开始等审批（用户刚发的消息）
    for s in [&a, &b] {
        f.send(&s.id, "needs permission").await.unwrap();
    }
    for s in [&a, &b] {
        f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
    }
    // 等这一轮跑完（两个 gh 各 1.5 秒）：两个都不能被归档，等审批的 turn 也不能被取消
    wait_until(15, "the round to finish", || f.td.gh_calls() >= 2).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    for s in [&a, &b] {
        let st = f.get(&s.id).await;
        assert!(!st.archived, "{} was archived while waiting for an answer", s.id);
        assert_eq!(st.status, SessionStatus::WaitingPermission, "its turn was cancelled");
        let pending: PermissionListResult =
            f.call(methods::PERMISSION_LIST, json!({ "session_id": s.id })).await.unwrap();
        assert_eq!(pending.requests.len(), 1, "the approval card is still there");
    }
    f.td.shutdown().await;
}

// =====================================================================================================================
// 等审批时不能归档
// =====================================================================================================================

#[tokio::test]
async fn archiving_while_waiting_for_an_answer_is_busy_for_every_kind_of_session() {
    let mut f = Fx::new().await;
    let wt = f.ready("worktree waits").await;
    let local = f.create(Some("proj"), Some("local waits"), None).await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    let scratch = f.create(None, Some("scratch waits"), None).await.unwrap();
    f.wait_status(&scratch.id, SessionStatus::Idle).await;
    for s in [&wt, &local, &scratch] {
        f.send(&s.id, "needs permission").await.unwrap();
        f.wait_status(&s.id, SessionStatus::WaitingPermission).await;
        // 再排一条消息：以前归档之后它会在放行后作为新 turn 跑
        f.send(&s.id, "queued behind the approval").await.unwrap();
        let e = archive(&mut f, &s.id, None).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
        let st = f.get(&s.id).await;
        assert!(!st.archived && st.status == SessionStatus::WaitingPermission);
    }
    assert!(f.dir_of(&wt).is_dir(), "the refused archive touched nothing");
    f.td.shutdown().await;
}

// =====================================================================================================================
// 失败创建占着的名字的提示；合并提交用 base 的展示名
// =====================================================================================================================

#[tokio::test]
async fn the_name_of_a_failed_creation_says_who_reserves_it() {
    let mut f = Fx::new().await;
    f.td.ctx.worktrees.hooks.fail_after_add.store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = f
        .create(Some("proj"), Some("fails here"), Some(json!({ "mode": "worktree", "branch": "wt/reuse" })))
        .await
        .unwrap();
    f.wait_wt_state(&failed.id, WorktreeState::Failed).await;
    f.td.ctx.worktrees.hooks.fail_after_add.store(false, std::sync::atomic::Ordering::SeqCst);
    // git 里并没有这个分支：不能说「已存在」，要说被谁占着、怎么办
    assert!(!f.branches().iter().any(|b| b == "wt/reuse"));
    let e = f
        .create(Some("proj"), Some("again"), Some(json!({ "mode": "worktree", "branch": "wt/reuse" })))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists, "{e:?}");
    assert!(
        e.message.contains(&failed.id) && e.message.contains("reserved") && e.message.contains("retry"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("already exists"), "{}", e.message);
    // 一个真的存在的分支仍然是「已存在」
    let e = f
        .create(Some("proj"), Some("real one"), Some(json!({ "mode": "worktree", "branch": "main" })))
        .await
        .unwrap_err();
    assert!(e.message.contains("already exists") || e.message.contains("reserved branch"), "{}", e.message);
    // 给失败的会话再发一条消息：用它占着的名字重试
    f.send(&failed.id, "retry with the same name").await.unwrap();
    let s = f.wait_wt_state(&failed.id, WorktreeState::Active).await;
    assert_eq!(s.worktree.unwrap().branch, "wt/reuse");
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_merge_made_by_a_sync_is_named_after_the_display_name_of_the_base() {
    let mut f = Fx::new().await;
    let s =
        f.create(Some("proj"), Some("merge named"), Some(json!({ "mode": "worktree", "base": "main" }))).await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let w = f.get(&s.id).await.worktree.unwrap();
    assert_eq!(w.base, "main");
    f.td.commit_file(&f.dir_of(&s), "mine.txt", "m", "mine");
    f.td.commit_file(&f.repo, "theirs.txt", "t", "theirs");
    let r: SessionSyncBaseResult = f.call(methods::SESSION_SYNC_BASE, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(r.outcome, SyncOutcome::Merged);
    let subject = f.td.git(&f.dir_of(&s), &["log", "-1", "--format=%s"]);
    assert_eq!(subject, format!("Merge main into {}", w.branch), "no `Merge branch 'refs/heads/main'`");
    f.td.shutdown().await;
}
