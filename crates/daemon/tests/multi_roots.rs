//! 多个 workspace 根（ADR-45）：RPC 层、会话和重启恢复、doctor。
//! 规则本身（优先级、回收站、关联规则……）的单元测试在 `src/workspace.rs` 和 `src/wsroots.rs`；这里验证它们接到了 RPC、审计、
//! 会话和启动流程上。额外根都在临时目录里，不碰真实的 HOME / `~/.anycon`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_daemon::config::Paths;
use anycon_daemon::testutil::TestDaemon;
use anycon_daemon::{Daemon, DaemonError, DaemonOptions};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(10);

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

/// 两个额外根（`ext1` 存在，`gone` 不存在，`ext2` 存在）所在的临时目录，和 daemon 自己的临时目录互不嵌套。
struct Disks {
    dir: tempfile::TempDir,
}

impl Disks {
    fn new() -> Disks {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ext1")).unwrap();
        std::fs::create_dir_all(dir.path().join("ext2")).unwrap();
        Disks { dir }
    }
    fn path(&self, name: &str) -> PathBuf {
        anycon_daemon::sys::canonicalize(self.dir.path()).unwrap().join(name)
    }
    /// `config set workspace.extra_roots` 的值：ext1、gone、ext2。
    fn extra_roots(&self) -> String {
        serde_json::to_string(&[self.path("ext1"), self.path("gone"), self.path("ext2")]).unwrap()
    }
}

async fn start(d: &Disks) -> TestDaemon {
    TestDaemon::start_with(&[("workspace.extra_roots", &d.extra_roots())], false).await
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

async fn wait_idle(td: &TestDaemon, c: &mut anycon_daemon::testutil::TestClient, sid: &str) {
    let t = std::time::Instant::now();
    loop {
        let g: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if g.status == SessionStatus::Idle {
            return;
        }
        assert!(t.elapsed() < T, "the session did not become idle: {:?}", g.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn node_info_lists_every_root_with_the_primary_marked_and_availability() {
    let d = Disks::new();
    let td = start(&d).await;
    let mut c = td.client("a");
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.workspace_root.as_deref(), Some(s(&td.workspace).as_str()));
    let roots: Vec<(String, bool, bool)> =
        info.workspace_roots.iter().map(|r| (r.path.clone(), r.primary, r.available)).collect();
    assert_eq!(
        roots,
        vec![
            (s(&td.workspace), true, true),
            (s(&d.path("ext1")), false, true),
            (s(&d.path("gone")), false, false),
            (s(&d.path("ext2")), false, true),
        ]
    );
    // 单根的节点：只有主根，同样列出来
    let td1 = TestDaemon::start().await;
    let mut c1 = td1.client("a");
    let info: NodeInfo = c1.call(&td1, methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.workspace_roots.len(), 1);
    assert!(info.workspace_roots[0].primary && info.workspace_roots[0].available);
    td1.shutdown().await;
    td.shutdown().await;
}

#[tokio::test]
async fn projects_created_in_an_extra_root_are_listed_audited_and_get_sessions_in_that_directory() {
    let d = Disks::new();
    let mut td = start(&d).await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let ext1 = d.path("ext1");

    let p: Project = a
        .call(&td, methods::WORKSPACE_CREATE, json!({ "name": "far", "git_init": false, "root": s(&ext1) }))
        .await
        .unwrap();
    assert_eq!((p.name.as_str(), p.root.as_deref()), ("far", Some(s(&ext1).as_str())));
    assert_eq!(p.path, s(&ext1.join("far")));
    assert!(ext1.join("far").is_dir() && !td.workspace.join("far").exists());
    // 默认建在主根
    let near: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "near" })).await.unwrap();
    assert_eq!(near.root.as_deref(), Some(s(&td.workspace).as_str()));
    // 另一个客户端收到 workspace.changed { added }
    let n = b.wait_notification(T, |n| n.method == methods::WORKSPACE_CHANGED).await.expect("workspace.changed");
    let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
    assert!(ch.added.contains(&"far".to_string()), "{ch:?}");

    let list: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert_eq!(list.root, s(&td.workspace));
    let mut seen: Vec<(String, Option<String>)> =
        list.projects.iter().map(|p| (p.name.clone(), p.root.clone())).collect();
    seen.sort();
    assert_eq!(seen, vec![("far".into(), Some(s(&ext1))), ("near".into(), Some(s(&td.workspace)))]);

    // root 不对：invalid（子目录、别的目录）；列出但不可用：not_found；没建任何东西
    for bad in [s(&ext1.join("far")), s(td.dir.path()), "ext1".to_string(), String::new()] {
        let e =
            a.call::<Project>(&td, methods::WORKSPACE_CREATE, json!({ "name": "bad", "root": bad })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{bad:?}");
    }
    let e = a
        .call::<Project>(&td, methods::WORKSPACE_CREATE, json!({ "name": "bad", "root": s(&d.path("gone")) }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert!(!d.path("gone").exists());
    let e = a
        .call::<JobStarted>(
            &td,
            methods::WORKSPACE_CLONE,
            json!({ "url": "https://example.invalid/x/y.git", "root": "/nope" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 名字全局唯一
    let e = a
        .call::<Project>(&td, methods::WORKSPACE_CREATE, json!({ "name": "near", "root": s(&ext1) }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists);

    // 审计带上 root
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    let e =
        audit.entries.iter().find(|e| e.action == "workspace.create" && e.target.as_deref() == Some("far")).unwrap();
    assert_eq!(e.detail["root"], json!(s(&ext1)));

    // 会话：agent 的 cwd 是额外根里的项目目录，重启之后照样（会话记着的位置是名字加目录身份，不记根）
    let sess: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "far", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    wait_idle(&td, &mut a, &sess.id).await;
    assert_eq!(td.fake.spawns.lock().unwrap()[0].cwd, ext1.join("far"));
    // fs.*：第一段是项目名，相对它所在的根
    std::fs::write(ext1.join("far/hello.txt"), "hello from ext1").unwrap();
    let r: FsReadResult = a.call(&td, methods::FS_READ, json!({ "path": "far/hello.txt" })).await.unwrap();
    assert_eq!(r.text, "hello from ext1");
    let l: FsListResult = a.call(&td, methods::FS_LIST, json!({ "path": "far" })).await.unwrap();
    assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["hello.txt"]);

    td.restart().await;
    let mut a = td.client("a");
    let list: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(list.projects.iter().any(|p| p.name == "far" && p.root.as_deref() == Some(s(&ext1).as_str())));
    let _: SessionSendResult = a
        .call(&td, methods::SESSION_SEND, json!({ "session_id": sess.id, "message_id": "m2", "content": "again" }))
        .await
        .unwrap();
    wait_idle(&td, &mut a, &sess.id).await;
    assert_eq!(td.fake.spawns.lock().unwrap().last().unwrap().cwd, ext1.join("far"));
    td.shutdown().await;
}

#[tokio::test]
async fn trash_restore_and_purge_work_per_root_through_the_rpc() {
    let d = Disks::new();
    let td = start(&d).await;
    let mut a = td.client("a");
    let (ext1, ext2) = (d.path("ext1"), d.path("ext2"));
    for (name, root) in [("p-main", None), ("p-ext1", Some(&ext1)), ("p-ext2", Some(&ext2))] {
        let params = match root {
            Some(r) => json!({ "name": name, "root": s(r) }),
            None => json!({ "name": name }),
        };
        let _: Project = a.call(&td, methods::WORKSPACE_CREATE, params).await.unwrap();
    }
    let t1: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "p-ext1" })).await.unwrap();
    let t2: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "p-ext2" })).await.unwrap();
    let t0: TrashEntry = a.call(&td, methods::WORKSPACE_TRASH, json!({ "name": "p-main" })).await.unwrap();
    assert_eq!(t1.root.as_deref(), Some(s(&ext1).as_str()));
    assert!(ext1.join(".trash").join(&t1.id).is_dir() && td.workspace.join(".trash").join(&t0.id).is_dir());
    let list: WorkspaceTrashListResult = a.call(&td, methods::WORKSPACE_TRASH_LIST, json!({})).await.unwrap();
    let mut seen: Vec<(String, Option<String>)> =
        list.entries.iter().map(|e| (e.original_name.clone(), e.root.clone())).collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("p-ext1".into(), Some(s(&ext1))),
            ("p-ext2".into(), Some(s(&ext2))),
            ("p-main".into(), Some(s(&td.workspace)))
        ]
    );
    // 还原回原来的根
    let back: Project = a.call(&td, methods::WORKSPACE_RESTORE, json!({ "id": t1.id })).await.unwrap();
    assert_eq!((back.name.as_str(), back.root.as_deref()), ("p-ext1", Some(s(&ext1).as_str())));
    assert!(ext1.join("p-ext1").is_dir());
    // purge 找得到任何根的条目
    let _: Empty = a.call(&td, methods::WORKSPACE_PURGE, json!({ "id": t2.id })).await.unwrap();
    assert!(!ext2.join(".trash").join(&t2.id).exists());
    let e = a.call::<Empty>(&td, methods::WORKSPACE_PURGE, json!({ "id": t2.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    td.shutdown().await;
}

#[tokio::test]
async fn doctor_has_one_entry_per_root_and_flags_the_missing_one() {
    let d = Disks::new();
    let td = start(&d).await;
    let mut a = td.client("a");
    let doctor: NodeDoctorResult = a.call(&td, methods::NODE_DOCTOR, json!({})).await.unwrap();
    let get = |id: &str| doctor.checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no check {id}"));
    assert!(get("workspace").ok);
    assert!(get(&format!("workspace:{}", s(&d.path("ext1")))).ok);
    assert!(get(&format!("workspace:{}", s(&d.path("ext2")))).ok);
    let gone = get(&format!("workspace:{}", s(&d.path("gone"))));
    assert!(!gone.ok);
    assert!(gone.detail.contains("does not exist"), "{}", gone.detail);
    assert!(gone.fix.as_deref().unwrap().contains("restart"), "{:?}", gone.fix);
    // 盘在 daemon 起来之后才挂上：doctor 说要重启（daemon 还是不带它）
    std::fs::create_dir_all(d.path("gone")).unwrap();
    let doctor: NodeDoctorResult = a.call(&td, methods::NODE_DOCTOR, json!({})).await.unwrap();
    let late = doctor.checks.iter().find(|c| c.id == format!("workspace:{}", s(&d.path("gone")))).unwrap();
    assert!(!late.ok && late.detail.contains("was missing when the daemon started"), "{}", late.detail);
    // 而且这个根还是不可用：不能在上面新建
    let e = a
        .call::<Project>(&td, methods::WORKSPACE_CREATE, json!({ "name": "x", "root": s(&d.path("gone")) }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // 重启之后可用
    let mut td = td;
    td.restart().await;
    let mut a = td.client("a");
    let doctor: NodeDoctorResult = a.call(&td, methods::NODE_DOCTOR, json!({})).await.unwrap();
    assert!(doctor.checks.iter().find(|c| c.id == format!("workspace:{}", s(&d.path("gone")))).unwrap().ok);
    let p: Project =
        a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "x", "root": s(&d.path("gone")) })).await.unwrap();
    assert_eq!(p.root.as_deref(), Some(s(&d.path("gone")).as_str()));
    td.shutdown().await;
}

/// 配置错误：daemon 拒绝启动。`roots(base, ws, home)` 给出 `workspace.extra_roots`；`base` 是临时目录，主根是 `base/ws`，
/// 数据目录是 `base/home`。
async fn start_error(roots: impl FnOnce(&Path, &Path, &Path) -> Vec<PathBuf>) -> DaemonError {
    let dir = tempfile::tempdir().unwrap();
    let base = anycon_daemon::sys::canonicalize(dir.path()).unwrap();
    let (workspace, home) = (base.join("ws"), base.join("home"));
    let paths = Paths::new(&home);
    let opts = anycon_daemon::setup::InitOptions {
        workspace: Some(s(&workspace)),
        name: Some("bad-roots".into()),
        ..Default::default()
    };
    anycon_daemon::setup::init(&paths, &opts).await.unwrap();
    let extra = roots(&base, &workspace, &home);
    for p in &extra {
        let _ = std::fs::create_dir_all(p);
    }
    let value = serde_json::to_string(&extra.iter().map(|p| s(p)).collect::<Vec<_>>()).unwrap();
    anycon_daemon::config::set_value(&paths, "workspace.extra_roots", &value).unwrap();
    let r = Daemon::start(
        paths,
        DaemonOptions {
            adapters: Some(vec![]),
            hub: false,
            socket: false,
            login_shell: Some(false),
            lock: false,
            ..Default::default()
        },
    )
    .await;
    match r {
        Ok(d) => {
            d.shutdown().await;
            panic!("the daemon started with extra_roots = {value}");
        }
        Err(e) => e,
    }
}

#[tokio::test]
async fn the_daemon_refuses_to_start_with_overlapping_or_unsafe_extra_roots() {
    // 相同、嵌套（两个方向）、和主根重叠
    let e = start_error(|b, _, _| vec![b.join("a"), b.join("a")]).await;
    assert!(matches!(e, DaemonError::Workspace(_)), "{e}");
    assert!(e.to_string().contains("workspace.extra_roots") && e.to_string().contains("same directory"), "{e}");
    let e = start_error(|b, _, _| vec![b.join("a"), b.join("a/inner")]).await;
    assert!(e.to_string().contains("is inside"), "{e}");
    let e = start_error(|b, _, _| vec![b.join("a/inner"), b.join("a")]).await;
    assert!(e.to_string().contains("contains"), "{e}");
    let e = start_error(|_, ws, _| vec![ws.join("sub")]).await;
    assert!(e.to_string().contains("is inside") && e.to_string().contains("workspace.root"), "{e}");
    // 数据目录：在里面，或者包含它
    let e = start_error(|_, _, home| vec![home.join("projects")]).await;
    assert!(e.to_string().contains("data directory"), "{e}");
    let e = start_error(|b, _, _| vec![b.to_path_buf()]).await;
    assert!(e.to_string().contains("data directory") && e.to_string().contains("contains"), "{e}");
    // 文件系统根
    #[cfg(unix)]
    {
        let e = start_error(|_, _, _| vec![PathBuf::from("/")]).await;
        assert!(e.to_string().contains("filesystem root"), "{e}");
    }
}

#[tokio::test]
async fn a_missing_extra_root_does_not_stop_the_daemon() {
    // 一个根都没有的盘：照常启动，只是这个根不可用
    let d = Disks::new();
    let td = TestDaemon::start_with(
        &[("workspace.extra_roots", &serde_json::to_string(&[d.path("not-mounted")]).unwrap())],
        false,
    )
    .await;
    let mut a = td.client("a");
    let list: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert!(list.projects.is_empty());
    let info: Value = a.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info["workspace_roots"][1]["available"], json!(false));
    td.shutdown().await;
}
