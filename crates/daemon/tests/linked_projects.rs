//! 关联本机已有目录为项目（ADR-33）：RPC 层 + 会话的整机行为。
//! 规则本身的单元测试在 `src/workspace.rs`；这里验证它们接到了 RPC、审计、会话和重启恢复上。
//! 所有目录都在临时目录里，`workspace.link_roots` 指向临时目录，不碰真实 HOME / `~/.anycon`。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::testutil::{eventually, TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(10);

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

/// 启动 daemon，把 `link_roots` 设成它临时目录下的 `machine/`（热加载），返回 `machine/` 的 canonical 路径。
async fn start() -> (TestDaemon, PathBuf) {
    let td = TestDaemon::start().await;
    let machine = td.dir.path().join("machine");
    std::fs::create_dir_all(machine.join("proj")).unwrap();
    std::fs::create_dir_all(machine.join("second")).unwrap();
    std::fs::write(machine.join("proj/hello.txt"), "hello from proj").unwrap();
    let machine = anycon_daemon::sys::canonicalize(machine).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", &roots).unwrap();
    assert!(td.ctx.config.reload().unwrap());
    (td, machine)
}

async fn link(td: &TestDaemon, c: &mut TestClient, path: &std::path::Path, name: Option<&str>) -> Project {
    c.call(td, methods::WORKSPACE_LINK, json!({ "path": path, "name": name })).await.unwrap()
}

async fn wait_status(td: &TestDaemon, c: &mut TestClient, sid: &str, want: SessionStatus) -> bool {
    let t = std::time::Instant::now();
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if s.status == want {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn sessions_of_a_linked_project_run_in_the_linked_directory_across_restarts_and_renames() {
    let (mut td, machine) = start().await;
    let mut a = td.client("a");
    let p = link(&td, &mut a, &machine.join("proj"), None).await;
    assert_eq!((p.name.as_str(), p.kind, p.missing), ("proj", ProjectKind::Linked, false));
    assert_eq!(p.path, machine.join("proj").to_string_lossy());

    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert_eq!(s.project.as_deref(), Some("proj"));
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap()[0].cwd, machine.join("proj"), "the agent runs in the linked directory");
    assert!(!td.workspace.join("proj").exists(), "nothing is created in the workspace root");

    // 列表：kind / 活跃会话数
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    let listed = l.projects.iter().find(|p| p.name == "proj").unwrap();
    assert_eq!((listed.kind, listed.active_sessions), (ProjectKind::Linked, 1));

    // daemon 重启：会话回到 interrupted/idle 之外的状态不重要，重要的是下一次 send 仍然在关联目录里拉起
    td.restart().await;
    let mut a = td.client("a");
    let _: SessionSendResult = a
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m2", "content": "again" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, machine.join("proj"));
    // 关联在重启后还在
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(l.projects.iter().any(|p| p.name == "proj" && p.kind == ProjectKind::Linked));

    // 改名：有活着的进程时 busy（和根目录下的项目一样）；重启会话后改名成功，会话跟着改，cwd 不变
    let e = a
        .call::<RenamedProject>(&td, methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let r: RenamedProject =
        a.call(&td, methods::WORKSPACE_RENAME, json!({ "from": "proj", "to": "renamed" })).await.unwrap();
    assert_eq!(r.to, "renamed");
    assert!(machine.join("proj/hello.txt").is_file(), "the directory is not renamed");
    let got: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(got.project.as_deref(), Some("renamed"));
    let _: SessionSendResult = a
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m3", "content": "third" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, machine.join("proj"));
    td.shutdown().await;
}

#[tokio::test]
async fn link_and_unlink_are_audited_broadcast_and_unlink_protects_running_sessions() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let p = link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    assert_eq!(p.name, "pj");
    // 另一个客户端收到 workspace.changed { added }
    let n =
        b.wait_notification(T, |n| n.method == methods::WORKSPACE_CHANGED).await.expect("workspace.changed on link");
    let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
    assert_eq!(ch.added, vec!["pj".to_string()]);

    // 有运行中的会话（hold = turn 不结束）-> unlink busy，什么都没变
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hold this", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let e = a.call::<Empty>(&td, methods::WORKSPACE_UNLINK, json!({ "name": "pj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(l.projects.iter().any(|p| p.name == "pj"));
    // turn 结束、会话空闲（agent 进程还活着）-> 可以解除关联，进程被收掉
    td.fake.last_handle().release();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "pj" })).await.unwrap();
    assert!(
        eventually(T, || async { td.fake.handle(0).shutdowns.load(Ordering::SeqCst) == 1 }).await,
        "the idle agent process of the unlinked project is shut down"
    );
    assert!(machine.join("proj/hello.txt").is_file(), "files are untouched");
    let n = b
        .wait_notification(T, |n| {
            n.method == methods::WORKSPACE_CHANGED
                && serde_json::from_value::<WorkspaceChanged>(n.params.clone()).is_ok_and(|c| !c.removed.is_empty())
        })
        .await
        .expect("workspace.changed on unlink");
    let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
    assert_eq!(ch.removed, vec!["pj".to_string()]);
    // 已有会话保留；下次 send 找不到项目而失败，不会在旧目录里重新拉起
    let spawns = td.fake.spawn_count();
    let _: SessionSendResult = a
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m2", "content": "still there?" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    assert_eq!(td.fake.spawn_count(), spawns);
    // 不是关联项目 / 不存在 -> invalid
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "plain" })).await.unwrap();
    for name in ["plain", "nope"] {
        let e = a.call::<Empty>(&td, methods::WORKSPACE_UNLINK, json!({ "name": name })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{name}");
    }
    // 审计：link 记 canonical 路径，unlink 记目标
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let link = audit.entries.iter().find(|e| e.action == "workspace.link").expect("audited link");
    assert_eq!(link.target.as_deref(), Some("pj"));
    assert_eq!(link.detail["path"], json!(machine.join("proj").to_string_lossy()));
    let unlink = audit.entries.iter().find(|e| e.action == "workspace.unlink").expect("audited unlink");
    assert_eq!(unlink.target.as_deref(), Some("pj"));
    // browse 不进审计
    let _: WorkspaceBrowseResult = a.call(&td, methods::WORKSPACE_BROWSE, json!({})).await.unwrap();
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    assert!(audit.entries.iter().all(|e| !e.action.contains("browse")));
    td.shutdown().await;
}

#[tokio::test]
async fn rpc_errors_namespace_trash_and_missing_projects() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), None).await;
    // 范围 / 重复 / 命名空间
    let outside = td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let e = a.call::<Project>(&td, methods::WORKSPACE_LINK, json!({ "path": outside })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    let e = a
        .call::<Project>(&td, methods::WORKSPACE_LINK, json!({ "path": machine.join("proj"), "name": "x" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists);
    let e = a.call::<Project>(&td, methods::WORKSPACE_CREATE, json!({ "name": "proj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists);
    let e = a
        .call::<JobStarted>(&td, methods::WORKSPACE_CLONE, json!({ "url": "https://example.invalid/a/proj.git" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists, "the default clone name is taken by the linked project");
    // 回收站对关联项目是 invalid，目录不动
    let e = a.call::<TrashEntry>(&td, methods::WORKSPACE_TRASH, json!({ "name": "proj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    assert!(machine.join("proj/hello.txt").is_file());
    // fs.* 相对关联目录；fs.read 进审计
    let l: FsListResult = a.call(&td, methods::FS_LIST, json!({ "path": "proj" })).await.unwrap();
    assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["hello.txt"]);
    let r: FsReadResult = a.call(&td, methods::FS_READ, json!({ "path": "proj/hello.txt" })).await.unwrap();
    assert_eq!(r.text, "hello from proj");
    let e = a.call::<FsReadResult>(&td, methods::FS_READ, json!({ "path": "proj/../second" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    // 目录没了：missing，session.create -> not_found，仍可 unlink
    std::fs::remove_dir_all(machine.join("proj")).unwrap();
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    let p = l.projects.iter().find(|p| p.name == "proj").unwrap();
    assert!(p.missing && p.git.is_none() && p.modified_at_ms == 0);
    let e = a
        .call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "proj" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "proj" })).await.unwrap();
    // link_roots 为空：link 和 browse 都是 forbidden
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", "[]").unwrap();
    td.ctx.config.reload().unwrap();
    let e =
        a.call::<Project>(&td, methods::WORKSPACE_LINK, json!({ "path": machine.join("second") })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    let e = a.call::<WorkspaceBrowseResult>(&td, methods::WORKSPACE_BROWSE, json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    td.shutdown().await;
}

#[tokio::test]
async fn browse_rpc_reports_what_can_be_linked() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), None).await;
    let r: WorkspaceBrowseResult = a.call(&td, methods::WORKSPACE_BROWSE, json!({})).await.unwrap();
    assert_eq!(r.path, machine.to_string_lossy());
    assert_eq!(r.roots, vec![machine.to_string_lossy().to_string()]);
    assert_eq!(r.parent, None);
    let by: Vec<(&str, Option<&str>, bool)> =
        r.entries.iter().map(|e| (e.name.as_str(), e.project.as_deref(), e.linkable)).collect();
    assert_eq!(by, vec![("proj", Some("proj"), false), ("second", None, true)]);
    // show_hidden
    std::fs::create_dir(machine.join(".dot")).unwrap();
    let r: WorkspaceBrowseResult =
        a.call(&td, methods::WORKSPACE_BROWSE, json!({ "show_hidden": true })).await.unwrap();
    assert!(r.entries.iter().any(|e| e.name == ".dot"));
    let r: WorkspaceBrowseResult = a.call(&td, methods::WORKSPACE_BROWSE, json!({})).await.unwrap();
    assert!(!r.entries.iter().any(|e| e.name == ".dot"));
    // 越界（文件系统根：unix 的 `/`，Windows 上盘符的根）
    let fs_root = machine.ancestors().last().unwrap().to_string_lossy().to_string();
    let e =
        a.call::<WorkspaceBrowseResult>(&td, methods::WORKSPACE_BROWSE, json!({ "path": fs_root })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    td.shutdown().await;
}

fn trace_errors(evs: &[SessionEvent]) -> Vec<(String, String)> {
    evs.iter()
        .filter_map(|e| match &e.event {
            Event::Error { code, message, .. } => Some((code.clone(), message.clone())),
            _ => None,
        })
        .collect()
}

async fn errors_of(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<(String, String)> {
    let h: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    trace_errors(&h.events)
}

async fn say(td: &TestDaemon, c: &mut TestClient, sid: &str, mid: &str, text: &str) {
    let _: SessionSendResult = c
        .call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": mid, "content": text }))
        .await
        .unwrap();
}

async fn project_of(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Option<String> {
    c.call::<SessionSummary>(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap().project
}

/// 会话记着创建时项目指向哪里。解除关联后同名关联到**另一个**目录，旧会话不能在新目录里续接；
/// 同一个目录用任何名字关联回来，旧会话就能继续（会话的 project 跟着改成新名字）。
#[tokio::test]
async fn a_session_cannot_continue_in_another_directory_but_can_when_the_same_directory_is_linked_back() {
    let (td, machine) = start().await;
    std::fs::write(machine.join("second/marker.txt"), "the other directory").unwrap();
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), None).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawns = td.fake.spawn_count();
    assert_eq!(td.fake.spawns.lock().unwrap()[0].cwd, machine.join("proj"));

    // 1) 解除关联，同名关联到另一个目录：旧会话发消息失败（error 事件 not_found，带原来的目录），不会在新目录里续接
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "proj" })).await.unwrap();
    link(&td, &mut a, &machine.join("second"), Some("proj")).await;
    say(&td, &mut a, &s.id, "m2", "continue?").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    let (code, message) = errs.last().unwrap();
    assert_eq!(code, "not_found", "{errs:?}");
    assert!(message.contains(&machine.join("proj").to_string_lossy().to_string()), "{message}");
    assert!(message.contains("link that directory again"), "{message}");
    assert_eq!(td.fake.spawn_count(), spawns, "no agent was started in the other directory");
    assert_eq!(project_of(&td, &mut a, &s.id).await.as_deref(), Some("proj"), "still bound to the name");

    // 2) 同一个目录、同一个名字关联回来：可以继续，cwd 是原来的目录
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "proj" })).await.unwrap();
    link(&td, &mut a, &machine.join("proj"), Some("proj")).await;
    say(&td, &mut a, &s.id, "m3", "back again").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, machine.join("proj"));

    // 3) 同一个目录用**别的名字**关联回来：会话的 project 跟着改，也可以继续
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "proj" })).await.unwrap();
    link(&td, &mut a, &machine.join("proj"), Some("renamed-back")).await;
    assert_eq!(project_of(&td, &mut a, &s.id).await.as_deref(), Some("renamed-back"));
    say(&td, &mut a, &s.id, "m4", "under a new name").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, machine.join("proj"));
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let relink =
        audit.entries.iter().find(|e| e.action == "workspace.link" && e.target.as_deref() == Some("renamed-back"));
    assert_eq!(relink.unwrap().detail["adopted_sessions"], json!(1));

    // 4) 项目名现在指向的仍然是会话的目录时，别的目录关联进来不会抢走这个会话
    link(&td, &mut a, &machine.join("second"), Some("other-name")).await;
    assert_eq!(project_of(&td, &mut a, &s.id).await.as_deref(), Some("renamed-back"));
    td.shutdown().await;
}

#[tokio::test]
async fn root_folder_sessions_are_pinned_to_the_folder_and_survive_rename_and_restart() {
    let (mut td, machine) = start().await;
    let mut a = td.client("a");
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "rf" })).await.unwrap();
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "rf", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    // 改名：位置跟着改，会话照常能用
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let r: RenamedProject = a.call(&td, methods::WORKSPACE_RENAME, json!({ "from": "rf", "to": "rf2" })).await.unwrap();
    assert_eq!(r.to, "rf2");
    say(&td, &mut a, &s.id, "m2", "after rename").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, td.workspace.join("rf2"));
    // 旧版本留下的会话（没有记过位置）：重启后第一次拉起时补上
    let mut row = td.ctx.store.get_session(&s.id).await.unwrap().unwrap();
    assert!(row.project_loc.as_deref().unwrap().starts_with("folder:rf2#"), "{:?}", row.project_loc);
    row.project_loc = None;
    td.ctx.store.upsert_session(&row).await.unwrap();
    td.restart().await;
    let mut a = td.client("a");
    say(&td, &mut a, &s.id, "m3", "after restart").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(td.ctx.store.get_session(&s.id).await.unwrap().unwrap().project_loc.unwrap().starts_with("folder:rf2#"));
    // 项目进回收站后，同名关联到别的目录：这个会话不会跑进新目录
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let _: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "rf2" })).await.unwrap();
    link(&td, &mut a, &machine.join("second"), Some("rf2")).await;
    let spawns = td.fake.spawn_count();
    say(&td, &mut a, &s.id, "m4", "into the wrong directory?").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    assert!(errs.last().unwrap().1.contains("rf2"), "{errs:?}");
    assert_eq!(td.fake.spawn_count(), spawns);
    td.shutdown().await;
}

/// 检查（没有运行中的会话）之后、登记删掉之前才开始的 turn，不能在已经解除关联的目录里继续跑。
/// 这里直接走 Workspace::unlink 绕过 RPC 层的检查（= 检查通过之后 turn 才开始），再做 RPC 层 unlink 之后的收尾。
#[tokio::test]
async fn a_turn_that_starts_after_the_busy_check_is_cancelled_when_the_project_is_unlinked() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hold this", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    // RPC 层：有运行中的会话 -> busy，什么都没变
    let e = a.call::<Empty>(&td, methods::WORKSPACE_UNLINK, json!({ "name": "pj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    // 竞态：检查已经过了（busy 回调说没有），登记被删掉；turn 其实在跑
    td.ctx.workspace.unlink("pj", |_| async { false }).await.unwrap();
    td.ctx.sessions.release_project("pj").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await, "the turn was cancelled");
    let h: SessionHistoryResult =
        a.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "limit": 1000 })).await.unwrap();
    assert!(
        h.events.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Cancelled, .. })),
        "{:?}",
        h.events
    );
    assert!(
        eventually(T, || async { td.fake.handle(0).shutdowns.load(Ordering::SeqCst) == 1 }).await,
        "and the agent process of the unlinked project is gone"
    );
    td.shutdown().await;
}

/// RPC 层：关联之后目录被换成指向数据目录（设备私钥）的符号链接。
/// 以前 `fs.read` 能读到 `keys/node.key`、`session.create` 在数据目录里起了 agent；现在全部失败，列表里标 missing。
#[tokio::test]
async fn a_linked_directory_swapped_for_a_symlink_to_the_data_directory_is_unreachable() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    std::fs::create_dir_all(td.paths.home.join("keys")).unwrap();
    std::fs::write(td.paths.home.join("keys/node.key"), "dummy-value").unwrap();
    std::fs::remove_dir_all(machine.join("proj")).unwrap();
    anycon_daemon::testutil::symlink_dir(&td.paths.home, &machine.join("proj")).unwrap();

    for (method, params) in [
        (methods::FS_LIST, json!({ "path": "pj" })),
        (methods::FS_LIST, json!({ "path": "pj/keys" })),
        (methods::FS_READ, json!({ "path": "pj/keys/node.key" })),
    ] {
        let e = a.call::<Value>(&td, method, params.clone()).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound, "{method} {params}");
        assert!(!e.message.contains("dummy-value"));
    }
    let spawns = td.fake.spawn_count();
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert_eq!(td.fake.spawn_count(), spawns, "no agent was started in the data directory");
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    let p = l.projects.iter().find(|p| p.name == "pj").unwrap();
    assert!(p.missing && p.git.is_none() && p.modified_at_ms == 0, "{p:?}");
    // 解除关联仍然可以
    let _: Empty = a.call(&td, methods::WORKSPACE_UNLINK, json!({ "name": "pj" })).await.unwrap();
    td.shutdown().await;
}

/// RPC 层：默认的 `~` 范围不包含凭证 / 自启动 / 隐藏目录；浏览里 `blocked` 说明原因。
/// 假 HOME 在临时目录里（daemon 环境的 HOME），不碰真实的 home。
#[tokio::test]
async fn sensitive_and_hidden_directories_are_refused_over_rpc_and_flagged_in_browse() {
    let td = TestDaemon::start().await;
    // `TestDaemon` 的环境用的是进程自己的 HOME，所以这里只用临时目录下的绝对路径做 root，不碰真实的 home；
    // 拒绝表里 `$HOME` 之下的项（`~/.ssh`、`~/Library`…）由 workspace.rs 的单元测试在假 HOME 里覆盖，这里验证 RPC 的形状和错误码
    let machine = anycon_daemon::sys::canonicalize(td.dir.path()).unwrap().join("machine");
    std::fs::create_dir_all(machine.join("work/.cache/x")).unwrap();
    std::fs::create_dir_all(machine.join("work/proj")).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", &roots).unwrap();
    td.ctx.config.reload().unwrap();
    let mut a = td.client("a");
    let e = a
        .call::<Project>(&td, methods::WORKSPACE_LINK, json!({ "path": machine.join("work/.cache/x"), "name": "x" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    assert!(e.message.contains("hidden") && e.message.contains("link_roots"), "{}", e.message);
    let r: WorkspaceBrowseResult = a
        .call(&td, methods::WORKSPACE_BROWSE, json!({ "path": machine.join("work"), "show_hidden": true }))
        .await
        .unwrap();
    let by: Vec<(&str, bool, Option<LinkBlock>)> =
        r.entries.iter().map(|e| (e.name.as_str(), e.linkable, e.blocked)).collect();
    assert_eq!(by, vec![(".cache", false, Some(LinkBlock::Hidden)), ("proj", true, None)]);
    assert_eq!(r.blocked, None);
    // 线上形状：blocked 是小写蛇形字符串，没有时省略
    let raw: Value = a
        .call(&td, methods::WORKSPACE_BROWSE, json!({ "path": machine.join("work"), "show_hidden": true }))
        .await
        .unwrap();
    assert_eq!(raw["entries"][0]["blocked"], json!("hidden"));
    assert!(raw["entries"][1].get("blocked").is_none());
    assert!(raw.get("blocked").is_none());
    td.shutdown().await;
}

// ======================================================================= 复用进程前的复核、位置的身份、NFD 名字、拉起之后的复核

fn shutdowns(td: &TestDaemon, idx: usize) -> usize {
    td.fake.handle(idx).shutdowns.load(Ordering::SeqCst)
}

async fn swap_for_symlink(dir: &std::path::Path, target: &std::path::Path) {
    std::fs::rename(dir, dir.with_extension("real")).unwrap();
    anycon_daemon::testutil::symlink_dir(target, dir).unwrap();
}

/// 进程还活着、会话空闲，关联目录被换成指向数据目录的符号链接。
/// 以前同一个会话的下一条消息照常跑完（规则 5 只在拉起进程时检查）；现在 `begin` 复用进程之前也复核。
#[tokio::test]
async fn a_live_idle_agent_process_is_not_reused_after_its_directory_was_swapped() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let mk = |mid: &'static str| json!({ "agent": "fake", "project": "pj", "prompt": "hi", "message_id": mid });
    let s1: SessionSummary = a.call(&td, methods::SESSION_CREATE, mk("m1")).await.unwrap();
    let s2: SessionSummary = a.call(&td, methods::SESSION_CREATE, mk("m2")).await.unwrap();
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    assert_eq!((td.fake.spawn_count(), shutdowns(&td, 0), shutdowns(&td, 1)), (2, 0, 0), "both processes are alive");
    let inputs_before =
        (td.fake.handle(0).inputs.lock().unwrap().len(), td.fake.handle(1).inputs.lock().unwrap().len());

    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    say(&td, &mut a, &s1.id, "m3", "continue in the data directory?").await;
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Error).await, "the message must not run");
    let errs = errors_of(&td, &mut a, &s1.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    assert!(
        !errs.last().unwrap().1.contains(td.paths.home.to_str().unwrap()),
        "no new location in the message: {errs:?}"
    );
    assert_eq!(
        td.fake.handle(0).inputs.lock().unwrap().len(),
        inputs_before.0,
        "nothing was delivered to the live process"
    );
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await, "its process is retired");
    // 同一个项目的其它会话的进程也被收掉（不用等它们自己收到下一条消息）
    assert!(eventually(T, || async { shutdowns(&td, 1) == 1 }).await, "the other session's process is retired too");
    assert_eq!(td.fake.handle(1).inputs.lock().unwrap().len(), inputs_before.1);
    assert_eq!(td.fake.spawn_count(), 2, "and nothing was started in the data directory");
    td.shutdown().await;
}

/// `workspace.list` / `fs.*` 发现关联项目 Moved 时顺手收掉这个项目所有会话的进程。
#[tokio::test]
async fn listing_a_project_whose_directory_was_swapped_retires_all_its_agent_processes() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s1: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Idle).await);
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(l.projects.iter().find(|p| p.name == "pj").unwrap().missing);
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await, "workspace.list noticed and retired the process");
    // fs.* 同理（换回来、再换一次）
    anycon_daemon::testutil::remove_link(&machine.join("proj")).unwrap();
    std::fs::rename(machine.join("proj.real"), machine.join("proj")).unwrap();
    let s2: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hi", "message_id": "m2" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let e = a.call::<FsListResult>(&td, methods::FS_LIST, json!({ "path": "pj" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert!(eventually(T, || async { shutdowns(&td, 1) == 1 }).await, "fs.list noticed and retired the process");
    td.shutdown().await;
}

/// 根目录项目的位置带上目录的身份（设备号 + inode）。
#[tokio::test]
async fn a_root_folder_session_follows_the_directory_not_just_its_name() {
    let (mut td, _machine) = start().await;
    let mut a = td.client("a");
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "rf" })).await.unwrap();
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "rf", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let loc = td.ctx.store.get_session(&s.id).await.unwrap().unwrap().project_loc.unwrap();
    assert!(loc.starts_with("folder:rf#") && loc.matches(':').count() == 2, "folder:<name>#<dev>:<ino> — {loc}");
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();

    // ① 进回收站再恢复（inode 不变）：能继续
    let trashed: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "rf" })).await.unwrap();
    let _: Project = a.call(&td, methods::WORKSPACE_RESTORE, json!({ "id": trashed.id })).await.unwrap();
    say(&td, &mut a, &s.id, "m2", "after restore").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawns = td.fake.spawn_count();
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, td.workspace.join("rf"));
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();

    // ② 进回收站，再新建同名文件夹（同一个设备、不同 inode）：旧会话不能续接
    let _: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "rf" })).await.unwrap();
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "rf" })).await.unwrap();
    say(&td, &mut a, &s.id, "m3", "into the new folder?").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    assert!(errs.last().unwrap().1.contains("different directory"), "{errs:?}");
    assert_eq!(td.fake.spawn_count(), spawns, "no agent was started in the replacement folder");
    // 新建的文件夹上的新会话不受影响
    let s2: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "rf", "prompt": "hi", "message_id": "m4" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);

    // ③ 设备号不同（整个 workspace 根搬到别的盘）：接受并重新记下；旧版本只记名字的位置也一样
    let real = td.ctx.store.get_session(&s2.id).await.unwrap().unwrap().project_loc.unwrap();
    let (name, id) = real.strip_prefix("folder:").unwrap().rsplit_once('#').unwrap();
    let (dev, ino) = id.split_once(':').unwrap();
    for stale in [
        format!("folder:{name}#{}:{}", dev.parse::<u64>().unwrap() + 1, ino.parse::<u64>().unwrap() + 99),
        format!("folder:{name}"),
    ] {
        let mut row = td.ctx.store.get_session(&s2.id).await.unwrap().unwrap();
        row.project_loc = Some(stale.clone());
        td.ctx.store.upsert_session(&row).await.unwrap();
        td.restart().await;
        let mut a = td.client("a");
        say(&td, &mut a, &s2.id, &format!("m-{}", stale.len()), "after the volume moved").await;
        assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await, "{stale}");
        assert_eq!(
            td.ctx.store.get_session(&s2.id).await.unwrap().unwrap().project_loc.unwrap(),
            real,
            "the real location is recorded again ({stale})"
        );
    }
    td.shutdown().await;
}

/// 根目录下 NFD 名字的文件夹。以前会话里存的是 NFC，`session.list { project: <列表里的名字> }` 返回 0 个，
/// `active_sessions` 是 0，项目页看不到这些会话。
#[tokio::test]
async fn sessions_of_a_decomposed_folder_name_are_found_by_the_name_workspace_list_gives() {
    let (mut td, _machine) = start().await;
    let nfd = "Re\u{301}sume\u{301}";
    let nfc = "R\u{e9}sum\u{e9}";
    std::fs::create_dir_all(td.workspace.join(nfd)).unwrap();
    let mut a = td.client("a");
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    let listed = l.projects.iter().find(|p| p.name.starts_with('R')).expect("the folder is listed").name.clone();
    // 用 NFC 写法新建会话：存的是盘上的名字，和列表里是同一个字符串
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": nfc, "prompt": "needle in a haystack", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert_eq!(s.project.as_deref(), Some(listed.as_str()));
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let mut found = Vec::new();
    for filter in [listed.as_str(), nfc, nfd] {
        let r: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({ "project": filter })).await.unwrap();
        assert_eq!(r.sessions.len(), 1, "session.list {{ project: {filter:?} }}");
        let hits: SessionSearchResult =
            a.call(&td, methods::SESSION_SEARCH, json!({ "query": "needle", "project": filter })).await.unwrap();
        assert!(!hits.hits.is_empty(), "session.search {{ project: {filter:?} }}");
        found.push(hits.hits.len());
    }
    assert!(found.iter().all(|n| *n == found[0]), "every spelling finds the same hits: {found:?}");
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert_eq!(l.projects.iter().find(|p| p.name == listed).unwrap().active_sessions, 1);
    // 之前版本存成 NFC 的老会话：过滤和活跃数一样认
    let mut row = td.ctx.store.get_session(&s.id).await.unwrap().unwrap();
    row.project = Some(nfc.to_string());
    td.ctx.store.upsert_session(&row).await.unwrap();
    td.restart().await;
    let mut a = td.client("a");
    let r: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({ "project": listed })).await.unwrap();
    assert_eq!(r.sessions.len(), 1, "legacy rows are still found");
    assert_eq!(r.sessions[0].project.as_deref(), Some(listed.as_str()), "and renamed to the on-disk spelling at load");
    let hits: SessionSearchResult =
        a.call(&td, methods::SESSION_SEARCH, json!({ "query": "needle", "project": listed })).await.unwrap();
    assert_eq!(hits.hits.len(), found[0]);
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert_eq!(
        l.projects.iter().find(|p| p.name == listed).unwrap().active_sessions,
        1,
        "legacy rows count under the on-disk name"
    );
    // 过滤一个不存在的项目：没有结果（不是全部）
    let r: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({ "project": "nope" })).await.unwrap();
    assert!(r.sessions.is_empty());
    let hits: SessionSearchResult =
        a.call(&td, methods::SESSION_SEARCH, json!({ "query": "needle", "project": "nope" })).await.unwrap();
    assert!(hits.hits.is_empty());
    td.shutdown().await;
}

/// 拉起 agent 之后再复核一次——解析项目和进程真正启动之间目录被换掉了，这个进程不会收到消息。
#[tokio::test]
async fn a_directory_swapped_while_the_agent_process_is_starting_is_caught_after_the_spawn() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    td.fake.set_spawn_delay(Duration::from_millis(400));
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    // 项目已经解析过了（cwd 已经定下），进程还在拉起：换目录
    tokio::time::sleep(Duration::from_millis(100)).await;
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await, "the turn must not run");
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    assert_eq!(td.fake.handle(0).inputs.lock().unwrap().len(), 0, "the new process never got the message");
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await, "and it was shut down");
    td.shutdown().await;
}

// ======================================================================= turn 进行中的复核

async fn wait_permission(td: &TestDaemon, c: &mut TestClient) -> PermissionRequest {
    let t = std::time::Instant::now();
    loop {
        let l: PermissionListResult = c.call(td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            return r;
        }
        assert!(t.elapsed() < T, "no permission request appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn history_events(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let h: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    h.events
}

/// turn 停在权限请求上，目录被换成指向数据目录的符号链接，然后客户端放行。
/// 以前 turn 照常往下跑（执行了那条命令）；现在：请求按拒绝答复 adapter，turn 以 `not_found` 结束，进程收掉。
#[tokio::test]
async fn allowing_a_permission_after_the_directory_was_swapped_is_refused() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "run a permission", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let req = wait_permission(&td, &mut a).await;
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "the client is told the request could not be allowed: {e:?}");
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    // 答复 adapter 的是拒绝选项，不是 allow；进程被收掉；turn 以 error 收尾，之后没有任何工具执行
    let decisions = td.fake.handle(0).decisions.lock().unwrap().clone();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].1.option_id.as_deref(), Some("deny"), "{decisions:?}");
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await, "the process is retired");
    let evs = history_events(&td, &mut a, &s.id).await;
    assert!(
        evs.iter().any(|e| matches!(&e.event, Event::PermissionResolved { option_id, by_device, .. }
            if option_id.as_deref() == Some("deny") && by_device == "daemon")),
        "{evs:?}"
    );
    assert!(evs.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Error, .. })));
    assert!(!evs.iter().any(|e| matches!(e.event, Event::ToolResult { .. })), "the tool must not have run");
    // 再答复一次：请求已经被消耗
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyResolved);
    td.shutdown().await;
}

/// 对照：目录没变时，放行照常。
#[tokio::test]
async fn allowing_a_permission_in_an_unchanged_directory_still_works() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "run a permission", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let req = wait_permission(&td, &mut a).await;
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.handle(0).decisions.lock().unwrap()[0].1.option_id.as_deref(), Some("allow"));
    td.shutdown().await;
}

/// 插话之前复核：目录变了就不送出，消息退回队列，turn 以 `not_found` 结束、进程收掉；目录恢复之后队列里的消息照常发出。
#[tokio::test]
async fn an_interjection_after_the_directory_was_swapped_is_not_delivered_and_goes_back_to_the_queue() {
    let (td, machine) = start().await;
    td.fake.interject.store(true, Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hold this", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let r: SessionSendResult = a
        .call(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "m2", "content": "second thoughts" }),
        )
        .await
        .unwrap();
    assert!(r.queued && r.turn_id.is_none(), "queued, not delivered: {r:?}");
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    assert_eq!(td.fake.handle(0).inputs.lock().unwrap().len(), 1, "only the first prompt ever reached the process");
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await);
    // 目录恢复：队列里的消息和新消息一起作为下一个 turn 发出（不丢）
    anycon_daemon::testutil::remove_link(&machine.join("proj")).unwrap();
    std::fs::rename(machine.join("proj.real"), machine.join("proj")).unwrap();
    say(&td, &mut a, &s.id, "m3", "third thought").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let last = td.fake.handle(1).inputs.lock().unwrap().last().unwrap().clone();
    assert!(last.content.contains("second thoughts") && last.content.contains("third thought"), "{}", last.content);
    assert!(!last.interject);
    td.shutdown().await;
}

/// 不答复、只等待：共享的周期检查（约 2 秒一次）在 5 秒内发现并收掉。
#[tokio::test]
async fn the_periodic_check_catches_a_swapped_directory_while_a_turn_is_just_waiting() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hold this", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    // 另一个项目上也在跑一个 turn，目录没动：不受影响
    std::fs::create_dir_all(machine.join("second")).unwrap();
    link(&td, &mut a, &machine.join("second"), Some("other")).await;
    let s2: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "other", "prompt": "hold this too", "message_id": "m2" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Running).await);
    let swapped_at = std::time::Instant::now();
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let t = std::time::Instant::now();
    loop {
        let st: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
        if st.status == SessionStatus::Error {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "not caught within 5 s (status {:?})", st.status);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(swapped_at.elapsed() < Duration::from_secs(5));
    let errs = errors_of(&td, &mut a, &s.id).await;
    assert_eq!(errs.last().unwrap().0, "not_found", "{errs:?}");
    let evs = history_events(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Error, .. })));
    assert!(eventually(T, || async { shutdowns(&td, 0) == 1 }).await, "the process is retired");
    assert_eq!(shutdowns(&td, 1), 0, "an untouched project keeps running");
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Running).await);
    td.fake.handle(1).release();
    td.shutdown().await;
}

// ======================================================================= `not_found` 的原因、空闲进程、被挡下的消息、替换根目录文件夹的提示

fn reason_of(e: &RpcError) -> Option<String> {
    e.data.as_ref().and_then(|d| d.reason.clone())
}

/// 目录变了导致的 `permission.respond` 的 `not_found` 带 `data.reason = "project_dir_changed"`，别的 `not_found` 不带，
/// 客户端据此区分「项目目录变了、本轮已中止」和「这个请求不存在」。
#[tokio::test]
async fn a_not_found_from_permission_respond_says_why_only_when_the_directory_changed() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "run a permission", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let req = wait_permission(&td, &mut a).await;
    // 未知的请求：not_found，没有 reason
    let e = a
        .call::<Empty>(
            &td,
            methods::PERMISSION_RESPOND,
            json!({ "request_id": "no-such-request", "option_id": "allow" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert_eq!(reason_of(&e), None, "{e:?}");
    // 目录变了之后放行：not_found + reason
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert_eq!(reason_of(&e).as_deref(), Some("project_dir_changed"), "{e:?}");
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    td.shutdown().await;
}

/// turn 之间目录被换掉，空闲但进程还活着的会话也要收进程（周期检查，约 2 秒一次）。
/// 以前周期检查只看有 turn 在跑的会话，空闲进程要等下一条消息、`workspace.list` 或 `idle_timeout`（默认 20 分钟）；
/// agent 在 turn 结束后仍在跑的后台任务会继续在被换掉的目录里执行。
#[tokio::test]
async fn the_periodic_check_also_closes_an_idle_agent_process_in_a_swapped_directory() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    link(&td, &mut a, &machine.join("proj"), Some("pj")).await;
    link(&td, &mut a, &machine.join("second"), Some("other")).await;
    let mk = |project: &'static str, mid: &'static str| json!({ "agent": "fake", "project": project, "prompt": "hi", "message_id": mid });
    let s1: SessionSummary = a.call(&td, methods::SESSION_CREATE, mk("pj", "m1")).await.unwrap();
    let s2: SessionSummary = a.call(&td, methods::SESSION_CREATE, mk("other", "m2")).await.unwrap();
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    assert_eq!((shutdowns(&td, 0), shutdowns(&td, 1)), (0, 0), "both idle processes are alive");

    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    // 不发消息、不调 workspace.list / fs.*：只靠周期检查，5 秒内收掉
    let t = std::time::Instant::now();
    while shutdowns(&td, 0) == 0 {
        assert!(t.elapsed() < Duration::from_secs(5), "the idle process was not closed within 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 会话本身保持空闲；目录没动的项目不受影响
    let st: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s1.id })).await.unwrap();
    assert_eq!(st.status, SessionStatus::Idle);
    assert_eq!(shutdowns(&td, 1), 0, "an untouched project keeps its process");
    // 下一条消息得到 not_found，没有新进程
    let spawns = td.fake.spawn_count();
    say(&td, &mut a, &s1.id, "m3", "are you still there?").await;
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Error).await);
    assert_eq!(errors_of(&td, &mut a, &s1.id).await.last().unwrap().0, "not_found");
    assert_eq!(td.fake.spawn_count(), spawns);
    td.shutdown().await;
}

/// 让会话停在「插话被目录检查挡下、消息退回队列、会话 error」的状态：返回（会话，机器目录）。
async fn blocked_interjection_fixture(
    td: &TestDaemon,
    a: &mut TestClient,
    machine: &std::path::Path,
) -> SessionSummary {
    td.fake.interject.store(true, Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    link(td, a, &machine.join("proj"), Some("pj")).await;
    let s: SessionSummary = a
        .call(
            td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "pj", "prompt": "hold this", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(td, a, &s.id, SessionStatus::Running).await);
    swap_for_symlink(&machine.join("proj"), &td.paths.home).await;
    let r: SessionSendResult = a
        .call(
            td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "m2", "content": "second thoughts" }),
        )
        .await
        .unwrap();
    assert!(r.queued && r.turn_id.is_none(), "{r:?}");
    assert!(wait_status(td, a, &s.id, SessionStatus::Error).await);
    assert!(eventually(T, || async { shutdowns(td, 0) == 1 }).await);
    s
}

fn restore_dir(machine: &std::path::Path) {
    anycon_daemon::testutil::remove_link(&machine.join("proj")).unwrap();
    std::fs::rename(machine.join("proj.real"), machine.join("proj")).unwrap();
}

/// 被挡下的插话 -> 目录还没恢复时再发一条 -> 恢复之后再发。以前第二条的 turn 拉起前解析项目失败，
/// `backlog.clear()` 把第一条（退回的插话）和第二条一起清掉，两条都没有送达也没有任何事件说明。现在消息留在队列里，
/// 恢复之后三条在同一个 turn 里按顺序各出现一次。
#[tokio::test]
async fn messages_blocked_while_the_directory_is_gone_wait_and_go_out_in_order_once_it_is_back() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    let s = blocked_interjection_fixture(&td, &mut a, &machine).await;
    let errors_before = errors_of(&td, &mut a, &s.id).await.len();
    // 目录还没恢复：再发一条，同样 not_found，但消息不丢
    say(&td, &mut a, &s.id, "m3", "third thought").await;
    let t = std::time::Instant::now();
    while errors_of(&td, &mut a, &s.id).await.len() <= errors_before {
        assert!(t.elapsed() < T, "the second attempt never failed");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    assert_eq!(td.fake.spawn_count(), 1, "nothing was started in the data directory");
    // 恢复目录，再发一条：三条一起、按顺序、各一次
    restore_dir(&machine);
    say(&td, &mut a, &s.id, "m4", "fourth thought").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let inputs = td.fake.handle(1).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1, "one turn carries all of them");
    let c = &inputs[0].content;
    for needle in ["second thoughts", "third thought", "fourth thought"] {
        assert_eq!(c.matches(needle).count(), 1, "{needle} appears exactly once in {c:?}");
    }
    let pos = |n: &str| c.find(n).unwrap();
    assert!(pos("second thoughts") < pos("third thought") && pos("third thought") < pos("fourth thought"), "{c:?}");
    td.shutdown().await;
}

/// `session.cancel` 清空还在等目录的消息，之后的消息不带着它们。
#[tokio::test]
async fn cancel_discards_the_messages_that_were_waiting_for_the_directory() {
    let (td, machine) = start().await;
    let mut a = td.client("a");
    let s = blocked_interjection_fixture(&td, &mut a, &machine).await;
    let _: Empty = a.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    restore_dir(&machine);
    say(&td, &mut a, &s.id, "m3", "a fresh start").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let inputs = td.fake.handle(1).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1);
    assert!(inputs[0].content.contains("a fresh start"), "{}", inputs[0].content);
    assert!(!inputs[0].content.contains("second thoughts"), "cancelled: {}", inputs[0].content);
    td.shutdown().await;
}

/// 根目录文件夹被换成新 inode（从备份恢复、删掉后重新 clone）后，提示先说「如果是有意替换的，新建一个会话；旧会话的历史仍然可以查看」，
/// 再说「否则把原来的文件夹放回来」。以前第一个建议是「把原来的文件夹放回来」，最常见的触发方式里原来的文件夹已经不存在。
#[tokio::test]
async fn replacing_a_root_folder_tells_the_user_to_start_a_new_session_first() {
    let (td, _machine) = start().await;
    let mut a = td.client("a");
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "rf" })).await.unwrap();
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "rf", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let _: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "rf" })).await.unwrap();
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "rf" })).await.unwrap();
    say(&td, &mut a, &s.id, "m2", "into the replacement?").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let errs = errors_of(&td, &mut a, &s.id).await;
    let (code_s, msg) = errs.last().unwrap();
    assert_eq!(code_s, "not_found");
    let at = |needle: &str| msg.find(needle).unwrap_or_else(|| panic!("{needle:?} missing in {msg:?}"));
    assert!(msg.contains("different directory"), "{msg}");
    assert!(at("on purpose") < at("start a new session"), "{msg}");
    assert!(at("start a new session") < at("history can still be viewed"), "{msg}");
    assert!(at("history can still be viewed") < at("restore the original folder"), "{msg}");
    td.shutdown().await;
}
