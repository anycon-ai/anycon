//! 锁顺序：命名空间锁（`Workspace::ns_lock`）在前，会话锁在后。
//!
//! `workspace.unlink` 持着命名空间锁去逐个会话锁（查「有没有运行中的会话」）；`session.promote` 以前持着会话锁去取命名空间锁
//! （`promote_scratch`），两边互相等，两把 tokio Mutex 都没有超时，之后所有占名字的操作和那个会话上的一切都挂住直到重启。
//! 这里的用例都带超时：死锁会让它们红，而不是让测试进程挂住。

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

async fn start() -> (TestDaemon, std::path::PathBuf) {
    let td = TestDaemon::start().await;
    let machine = std::fs::canonicalize(td.dir.path()).unwrap().join("machine");
    std::fs::create_dir_all(&machine).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", &roots).unwrap();
    td.ctx.config.reload().unwrap();
    (td, machine)
}

async fn scratch(td: &TestDaemon, c: &mut TestClient) -> SessionSummary {
    c.call(td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap()
}

/// 确定性的：自己持有命名空间锁，让 promote 卡在 `promote_scratch` 的取锁上。以前这时 promote 还抓着会话锁，
/// 会话上的任何调用（以及 unlink 的 busy 检查）都会被它挡住；现在 promote 在等锁时不持会话锁。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promote_waiting_for_the_namespace_lock_does_not_hold_the_session_lock() {
    let (td, _machine) = start().await;
    let mut a = td.client("a");
    let s = scratch(&td, &mut a).await;
    let guard = td.ctx.workspace.ns_lock_for_test().lock().await;
    let (td2, sid) = (td.ctx.clone(), s.id.clone());
    let rpc = td.rpc();
    let handle = a.handle.clone();
    let promote = tokio::spawn(async move {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Num(900),
            method: methods::SESSION_PROMOTE.into(),
            params: json!({ "session_id": sid, "name": "promoted" }),
        };
        let _ = &td2;
        rpc.handle_request(&handle, req).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!promote.is_finished(), "promote is waiting for the namespace lock");
    // 会话锁是空闲的：读它、查它（unlink 的 busy 检查走的就是这些）都立刻返回
    let got: Result<SessionSummary, RpcError> =
        tokio::time::timeout(Duration::from_secs(2), a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })))
            .await
            .expect("session.get must not wait for the promote that waits for the namespace lock");
    assert_eq!(got.unwrap().project, None);
    tokio::time::timeout(Duration::from_secs(2), td.ctx.sessions.project_running("anything"))
        .await
        .expect("project_running (the busy check of unlink) locks every session");
    // 升格期间 send 是 busy（不会有 turn 在要被搬走的 scratch 目录里开始）
    let e = tokio::time::timeout(
        Duration::from_secs(2),
        a.call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "m1", "content": "hi" }),
        ),
    )
    .await
    .expect("send must not hang")
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "{e:?}");
    drop(guard);
    let resp = tokio::time::timeout(T, promote).await.expect("promote finishes once the lock is free").unwrap();
    assert!(matches!(resp, RpcMessage::Response(r) if r.error.is_none()));
    let after: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(after.project.as_deref(), Some("promoted"));
    // 升格完成之后 send 恢复
    let r: SessionSendResult = a
        .call(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "m2", "content": "now in the project" }),
        )
        .await
        .unwrap();
    assert!(r.turn_id.is_some());
    td.shutdown().await;
}

/// 统计性的：多轮同时 promote 和 unlink（unlink 会逐个锁全部会话，包括正在 promote 的那个），全部在超时内完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promote_and_unlink_running_at_the_same_time_never_deadlock() {
    let (td, machine) = start().await;
    let work = async {
        let mut a = td.client("a");
        let mut b = td.client("b");
        for round in 0..40 {
            let dir = machine.join(format!("p{round}"));
            std::fs::create_dir_all(&dir).unwrap();
            let _: Project = a
                .call(&td, methods::WORKSPACE_LINK, json!({ "path": dir, "name": format!("p{round}") }))
                .await
                .unwrap();
            let s = scratch(&td, &mut a).await;
            let name = format!("promoted{round}");
            let (r1, r2) = tokio::join!(
                a.call::<Empty>(&td, methods::SESSION_PROMOTE, json!({ "session_id": s.id, "name": name })),
                b.call::<Empty>(&td, methods::WORKSPACE_UNLINK, json!({ "name": format!("p{round}") })),
            );
            r1.unwrap();
            r2.unwrap();
            let after: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
            assert_eq!(after.project.as_deref(), Some(name.as_str()), "round {round}");
        }
    };
    tokio::time::timeout(Duration::from_secs(60), work).await.expect("a deadlock between promote and unlink");
    // 之后命名空间锁和会话锁都还能用
    let mut a = td.client("a");
    let _: Project = tokio::time::timeout(T, a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "after" })))
        .await
        .expect("create after the rounds")
        .unwrap();
    td.fake.interject.store(false, Ordering::SeqCst);
    td.shutdown().await;
}

// ---------------------------------------------------------------------------------------------------------------------
// worktree（ADR-34）：git 操作不在会话锁里跑，也从不去取命名空间锁。新路径沿用 `promote` 的三步写法。

use common::wt::Fx;

/// 命名空间锁被占着（`unlink` / `create` / `rename` 持有它的时候）：worktree 的所有操作——建、查状态 / 改动、同步、归档、
/// 取消归档、删除、列表、清理——都不能等它（它们不取它）。有超时：一旦哪条路径去取了，这里红的是超时而不是挂住。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_worktree_operation_waits_for_the_namespace_lock() {
    let mut f = Fx::new().await;
    let ctx = f.td.ctx.clone();
    let guard = ctx.workspace.ns_lock_for_test().lock().await;
    let work = async {
        let s = f.ready("under the lock").await;
        let sid = s.id.clone();
        let _: SessionGitStatus = f.call(methods::SESSION_GIT_STATUS, json!({ "session_id": sid })).await.unwrap();
        let _: SessionDiffResult = f.call(methods::SESSION_DIFF, json!({ "session_id": sid })).await.unwrap();
        let _: SessionSyncBaseResult = f.call(methods::SESSION_SYNC_BASE, json!({ "session_id": sid })).await.unwrap();
        let _: WorkspaceBranchesResult =
            f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
        let _: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({})).await.unwrap();
        let _: Empty =
            f.call(methods::SESSION_ARCHIVE, json!({ "session_id": sid, "worktree": "remove" })).await.unwrap();
        let _: Empty = f.call(methods::SESSION_UNARCHIVE, json!({ "session_id": sid })).await.unwrap();
        let _: Empty =
            f.call(methods::SESSION_ARCHIVE, json!({ "session_id": sid, "worktree": "keep" })).await.unwrap();
        let list: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({})).await.unwrap();
        let _: Empty = f
            .call(methods::WORKSPACE_REMOVE_WORKTREE, json!({ "id": list.worktrees[0].id, "force": true }))
            .await
            .unwrap();
        let _: Empty = f.call(methods::SESSION_DELETE, json!({ "session_id": sid })).await.unwrap();
        // 再建一个、不等它跑完就删（创建流程也不取命名空间锁）
        let s2 = f.create_wt("second under the lock").await;
        let _: Empty =
            f.call(methods::SESSION_DELETE, json!({ "session_id": s2.id, "worktree": "remove" })).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .expect("a worktree operation tried to take the namespace lock while it was held");
    drop(guard);
    f.td.shutdown().await;
}

/// git 在跑的时候会话锁是空闲的：建 worktree、同步 base、归档 / 删除时处置 worktree 期间，`session.get` / `session.list` /
/// `project_running`（`unlink` 的 busy 检查，持着命名空间锁逐个锁会话）都立刻返回。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_lock_is_free_while_git_runs() {
    let mut f = Fx::new().await;
    async fn probe(f: &mut Fx, sid: &str, what: &str) {
        let r = tokio::time::timeout(Duration::from_secs(2), async {
            let _: SessionSummary = f.call(methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
            let _: SessionListResult = f.call(methods::SESSION_LIST, json!({})).await.unwrap();
            f.td.ctx.sessions.project_running("proj").await;
            f.td.ctx.sessions.project_busy("proj").await;
        })
        .await;
        assert!(r.is_ok(), "the session lock was held while {what}");
    }
    // 建 worktree 期间（add 之后停 1.5 秒，这时 git 刚跑完、daemon 在建后处理里）
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(1500, Ordering::SeqCst);
    let s = f.create_wt("probe creation").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    probe(&mut f, &s.id, "the worktree was being created").await;
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(0, Ordering::SeqCst);
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    // 同步期间
    f.td.ctx.worktrees.hooks.sync_delay_ms.store(1500, Ordering::SeqCst);
    f.td.commit_file(&f.repo, "x.txt", "x", "base moved");
    let (rpc, sid, handle) = (f.td.rpc(), s.id.clone(), f.td.client("syncer").handle.clone());
    let sync = tokio::spawn(async move {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Num(5),
            method: methods::SESSION_SYNC_BASE.into(),
            params: json!({ "session_id": sid }),
        };
        rpc.handle_request(&handle, req).await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    probe(&mut f, &s.id, "the base was being merged").await;
    assert!(matches!(sync.await.unwrap(), RpcMessage::Response(r) if r.error.is_none()));
    f.td.ctx.worktrees.hooks.sync_delay_ms.store(0, Ordering::SeqCst);
    f.td.shutdown().await;
}

/// 归档、worktree 和冷缓存相关的路径同样不取命名空间锁，也不在会话锁里等 git / gh：已归档会话的 `send` / `sync_base`、
/// `remove_worktree` 的 turn / 进程检查、`project_busy` 看 git 忙标记、冷缓存时等 git 元数据的 `workspace.list`、
/// PR 自动归档的一轮（含它里面的 `archive`）。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_worktree_and_cold_cache_paths_do_not_wait_for_the_namespace_lock() {
    let mut f = Fx::new().await;
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    // 准备（不持锁）：一个已归档的会话；一个「已归档的标记 + 进程还活着」的会话；一个 PR 已合并的会话
    let archived = f.ready("archived one").await;
    let _: Empty = f.call(methods::SESSION_ARCHIVE, json!({ "session_id": archived.id })).await.unwrap();
    let owner = f.ready("busy owner").await;
    f.td.ctx.sessions.set_archived(&owner.id, true).await.unwrap();
    let merged = f.ready("merged pr").await;
    f.td.git(&f.dir_of(&merged), &["push", "-q", "-u", "origin", "HEAD"]);
    anycon_daemon::config::set_value(&f.td.paths, "worktree.auto_archive_on_pr_close", "true").unwrap();
    f.td.ctx.config.reload().unwrap();
    f.td.set_gh(
        &json!({ "number": 9, "url": "https://example.test/pr/9", "title": "t", "state": "MERGED", "isDraft": false, "baseRefName": "main", "statusCheckRollup": [] }).to_string(),
        "",
        0,
    );
    f.td.slow_git(&[("*status*", 0.3)]);
    f.td.ctx.workspace.clear_git_cache_for_test();
    // 这条用例要断言的是「冷缓存的 list 在等 git 时不持锁、`session.get` 不被它挡住」，不是「list 在 3 秒内等到 git」：
    // macOS 上刚写出的脚本第一次执行偶尔要停顿约 3 秒，默认的冷缓存预算会被它吃掉，所以放宽预算
    f.td.ctx.workspace.set_git_cold_budget_for_test(Duration::from_secs(20));
    let ctx = f.td.ctx.clone();
    let guard = ctx.workspace.ns_lock_for_test().lock().await;
    let work = async {
        let e = f.send(&archived.id, "hello?").await.unwrap_err();
        assert_eq!(common::wt::reason(&e), Some("archived"));
        let e = f
            .call::<SessionSyncBaseResult>(methods::SESSION_SYNC_BASE, json!({ "session_id": archived.id }))
            .await
            .unwrap_err();
        assert_eq!(common::wt::reason(&e), Some("archived"));
        let e = f
            .call::<Empty>(
                methods::WORKSPACE_REMOVE_WORKTREE,
                json!({ "id": owner.worktree.as_ref().unwrap().id, "force": true }),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Busy);
        ctx.sessions.project_busy("proj").await;
        // 冷缓存的 list 在等 git 的时候不持任何锁：会话照常读得到
        let mut other = f.td.client("other");
        let (td, sid) = (&f.td, merged.id.clone());
        let (list, got) =
            tokio::join!(f.c.call::<WorkspaceListResult>(&f.td, methods::WORKSPACE_LIST, json!({})), async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                tokio::time::timeout(
                    Duration::from_millis(1500),
                    other.call::<SessionSummary>(td, methods::SESSION_GET, json!({ "session_id": sid })),
                )
                .await
                .expect("session.get must not wait for the list that waits for git")
            });
        assert!(list.unwrap().projects.iter().any(|p| p.name == "proj" && p.git.is_some()));
        got.unwrap();
        // PR 一轮（`gh` 查到 MERGED → archive → 处置 worktree）
        assert_eq!(ctx.sessions.auto_archive_round().await, 1);
    };
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .expect("a path tried to take the namespace lock while it was held");
    drop(guard);
    f.td.shutdown().await;
}

/// `slow_git` / `fail_git` 把 `bin/git` 换成刚写出的包装脚本。macOS 上「第一次执行一个刚写出的脚本」偶尔要停顿约 3 秒，
/// 所以夹具在交出去之前就同步执行过它一次（脚本第一次被执行时留下 `git.warm`）：测试里 daemon 的 git 调用不再带着这个停顿
/// 去赶 3 秒的冷缓存预算。
#[cfg(unix)]
#[tokio::test]
async fn the_git_wrapper_has_already_run_once_when_the_fixture_is_handed_over() {
    let td = TestDaemon::start_git(&[]).await;
    let marker = td.dir.path().join("bin/git.warm");
    assert!(!marker.exists(), "the plain symlink to git has never been the wrapper");
    td.slow_git(&[]);
    assert!(marker.exists(), "the wrapper was executed once while the fixture was being built");
    td.shutdown().await;
}
