//! 本机通道（Unix socket / Windows 命名管道）端到端：换行分隔的 JSON-RPC，和内层协议同一套方法。

use std::time::Duration;

use anycon_daemon::local::ClientStream;
use anycon_daemon::testutil::TestDaemon;
use anycon_daemon::{Daemon, DaemonError, DaemonOptions};
use anycon_protocol::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

const T: Duration = Duration::from_secs(10);

struct Sock {
    r: BufReader<ReadHalf<ClientStream>>,
    w: WriteHalf<ClientStream>,
    next: u64,
    notes: Vec<RpcNotification>,
}

impl Sock {
    async fn connect(td: &TestDaemon) -> Sock {
        let s = anycon_daemon::local::connect(&td.paths.sock()).await.unwrap();
        let (r, w) = tokio::io::split(s);
        Sock { r: BufReader::new(r), w, next: 0, notes: vec![] }
    }

    async fn send_raw(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    async fn read_msg(&mut self) -> RpcMessage {
        let mut line = String::new();
        tokio::time::timeout(T, self.r.read_line(&mut line)).await.expect("timed out reading a line").unwrap();
        assert!(!line.is_empty(), "socket closed");
        RpcMessage::decode(line.trim().as_bytes()).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }

    /// 发请求并等对应响应；途中收到的通知存起来。
    async fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next += 1;
        let id = self.next;
        let req = RpcMessage::request(id, method, params);
        self.send_raw(&String::from_utf8(req.encode()).unwrap()).await;
        loop {
            match self.read_msg().await {
                RpcMessage::Response(r) if r.id == RpcId::Num(id) => {
                    return r.error.map(Err).unwrap_or_else(|| Ok(r.result.unwrap_or(Value::Null)))
                }
                RpcMessage::Notification(n) => self.notes.push(n),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    async fn call<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, RpcError> {
        self.call_raw(method, params).await.map(|v| serde_json::from_value(v).unwrap())
    }

    async fn next_note(&mut self, pred: impl Fn(&RpcNotification) -> bool) -> RpcNotification {
        if let Some(i) = self.notes.iter().position(&pred) {
            return self.notes.remove(i);
        }
        loop {
            match self.read_msg().await {
                RpcMessage::Notification(n) if pred(&n) => return n,
                RpcMessage::Notification(n) => self.notes.push(n),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn node_info_agent_list_and_end_to_end_session_over_the_socket() {
    let td = TestDaemon::start_with(&[], true).await;
    // 命名管道不是文件，它的 DACL 在 `local` 模块的单元测试里
    if cfg!(unix) {
        anycon_daemon::testutil::assert_mode(&td.paths.sock(), 0o600);
    }
    let mut s = Sock::connect(&td).await;

    let info: NodeInfo = s.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.name, "test-node");
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(info.workspace_root.as_deref(), Some(td.workspace.to_str().unwrap()));
    assert!(info.capabilities.workspace && info.capabilities.search && info.capabilities.audit);
    assert!(info.node_id.starts_with("unpaired-"), "no hub identity yet: {}", info.node_id);

    let list: AgentListResult = s.call(methods::AGENT_LIST, Value::Null).await.unwrap();
    assert_eq!(list.agents.len(), 1);
    let a = &list.agents[0];
    assert_eq!((a.id.as_str(), a.status), ("fake", AgentStatus::Ready));
    assert_eq!(a.models.len(), 2);
    assert!(a.models[0].thinking.is_some() && a.modes.iter().any(|m| m.risk == ModeRisk::Dangerous));
    let again: AgentListResult = s.call(methods::AGENT_LIST, json!({ "if_none_match": list.etag })).await.unwrap();
    assert!(again.not_modified && again.agents.is_empty());
    let refreshed: AgentListResult = s.call(methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    assert_eq!(refreshed.etag, list.etag, "an unchanged re-probe keeps the etag");
    let diag: AgentDiagnoseResult = s.call(methods::AGENT_DIAGNOSE, json!({ "id": "fake" })).await.unwrap();
    assert!(diag.text.contains("Fake Agent") && !diag.fields.is_empty());

    // 创建会话 + 发消息 + 收事件 + 权限答复
    let created: SessionSummary = s
        .call(
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "run a permission check", "message_id": "m-1" }),
        )
        .await
        .unwrap();
    let _: Empty = s.call(methods::SESSION_WATCH, json!({ "session_id": created.id })).await.unwrap();
    let req: PermissionRequest =
        serde_json::from_value(s.next_note(|n| n.method == methods::PERMISSION_REQUEST).await.params).unwrap();
    assert_eq!(req.session_id, created.id);
    let _: Empty = s
        .call(methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    // 流式事件：turn_done 之前能看到 tool_result
    let mut seen = Vec::new();
    loop {
        let n = s.next_note(|n| n.method == methods::SESSION_EVENT).await;
        let ev: SessionEvent = serde_json::from_value(n.params).unwrap();
        let done = matches!(ev.event, Event::TurnDone { .. });
        seen.push(ev);
        if done {
            break;
        }
    }
    assert!(seen.iter().any(|e| matches!(e.event, Event::ToolResult { .. })));
    assert!(seen.windows(2).all(|w| w[0].seq < w[1].seq));
    let g: SessionSummary = s.call(methods::SESSION_GET, json!({ "session_id": created.id })).await.unwrap();
    assert_eq!(g.usage.turns, 1);

    // 其它 node.* 方法
    let stats: NodeStats = s.call(methods::NODE_STATS, json!({})).await.unwrap();
    assert!(!stats.hub_connected && stats.db_bytes > 0 && stats.disk_free_bytes > 0);
    assert!(stats.sessions.idle + stats.sessions.running >= 1);
    let devices: NodeDevicesResult = s.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert!(devices.devices.is_empty());
    let audit: NodeAuditResult = s.call(methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    let actions: Vec<&str> = audit.entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"session.create") && actions.contains(&"permission.respond"), "{actions:?}");
    assert!(audit.entries.iter().all(|e| e.device_id == "local" && e.device_name == "local-cli"));
    assert!(
        !serde_json::to_string(&audit).unwrap().contains("run a permission check"),
        "message content never reaches the audit log"
    );
    let doctor: NodeDoctorResult = s.call(methods::NODE_DOCTOR, json!({})).await.unwrap();
    let ids: Vec<&str> = doctor.checks.iter().map(|c| c.id.as_str()).collect();
    for want in [
        "config",
        "keys",
        "hub",
        "device_token",
        "daemon",
        "workspace",
        "agent:fake",
        "login_shell_path",
        "service",
        "clock",
        "blobs",
    ] {
        assert!(ids.contains(&want), "doctor is missing {want}: {ids:?}");
    }
    assert!(doctor.checks.iter().find(|c| c.id == "daemon").unwrap().ok);
    // 附件：个数、字节数和剩余磁盘都在 detail 里
    let blobs = doctor.checks.iter().find(|c| c.id == "blobs").unwrap();
    assert!(
        blobs.ok && blobs.detail.starts_with("0 attachment(s)") && blobs.detail.contains("free on the data volume")
    );
    assert!(doctor.checks.iter().find(|c| c.id == "agent:fake").unwrap().ok);
    let _: Empty = s
        .call(methods::NODE_SET_AGENT_DEFAULTS, json!({ "agent": "fake", "model": "fake-small", "mode": "plan" }))
        .await
        .unwrap();
    let new_session: SessionSummary = s.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    assert_eq!(
        (new_session.settings.model.as_deref(), new_session.settings.mode.as_deref()),
        (Some("fake-small"), Some("plan"))
    );

    // local.* 只在本机 socket 上有
    let st: Value = s.call("local.status", json!({})).await.unwrap();
    assert_eq!(st["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(st["hub"]["connected"], json!(false));

    // 协议层错误
    let e = s.call_raw("no.such.method", json!({})).await.unwrap_err();
    assert_eq!(e.code, -32601);
    s.send_raw("this is not json").await;
    match s.read_msg().await {
        RpcMessage::Response(r) => assert_eq!(r.error.unwrap().code, -32700),
        other => panic!("{other:?}"),
    }
    let e = s.call_raw(methods::SESSION_GET, json!({ "wrong": 1 })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    // 通知（无 id）被忽略，连接继续可用
    s.send_raw(r#"{"jsonrpc":"2.0","method":"node.info"}"#).await;
    assert!(s.call::<NodeInfo>(methods::NODE_INFO, json!({})).await.is_ok());
    td.shutdown().await;
}

#[tokio::test]
async fn workspace_and_fs_methods_over_the_socket() {
    let td = TestDaemon::start_with(&[], true).await;
    let mut s = Sock::connect(&td).await;
    let p: Project = s.call(methods::WORKSPACE_CREATE, json!({ "name": "alpha", "git_init": true })).await.unwrap();
    assert_eq!(p.name, "alpha");
    // 事件：所有客户端收到 workspace.changed
    let ch = s.next_note(|n| n.method == methods::WORKSPACE_CHANGED).await;
    assert_eq!(serde_json::from_value::<WorkspaceChanged>(ch.params).unwrap().added, vec!["alpha".to_string()]);
    let e = s.call::<Project>(methods::WORKSPACE_CREATE, json!({ "name": "alpha" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Exists));
    for bad in ["../x", ".trash", "a/b", ""] {
        let e = s.call::<Project>(methods::WORKSPACE_CREATE, json!({ "name": bad })).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid), "{bad:?}");
    }
    std::fs::write(td.workspace.join("alpha/readme.md"), "# hi").unwrap();
    let fl: FsListResult = s.call(methods::FS_LIST, json!({ "path": "alpha" })).await.unwrap();
    assert!(fl.entries.iter().any(|e| e.name == "readme.md" && !e.is_dir));
    let fr: FsReadResult = s.call(methods::FS_READ, json!({ "path": "alpha/readme.md" })).await.unwrap();
    assert_eq!(fr.text, "# hi");
    for bad in ["../outside", "/etc/passwd", "alpha/../../x"] {
        let e = s.call::<FsReadResult>(methods::FS_READ, json!({ "path": bad })).await.unwrap_err();
        assert!(matches!(e.stable_code(), Some(ErrorCode::Forbidden)), "{bad}: {e:?}");
    }
    let r: RenamedProject = s.call(methods::WORKSPACE_RENAME, json!({ "from": "alpha", "to": "beta" })).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("alpha", "beta"));
    let entry: TrashEntry = s.call(methods::WORKSPACE_TRASH, json!({ "name": "beta" })).await.unwrap();
    let tl: WorkspaceTrashListResult = s.call(methods::WORKSPACE_TRASH_LIST, json!({})).await.unwrap();
    assert_eq!(tl.entries.len(), 1);
    let restored: Project = s.call(methods::WORKSPACE_RESTORE, json!({ "id": entry.id })).await.unwrap();
    assert_eq!(restored.name, "beta");
    let _: TrashEntry = s.call(methods::WORKSPACE_TRASH, json!({ "name": "beta" })).await.unwrap();
    let tl: WorkspaceTrashListResult = s.call(methods::WORKSPACE_TRASH_LIST, json!({})).await.unwrap();
    let _: Empty = s.call(methods::WORKSPACE_PURGE, json!({ "id": tl.entries[0].id })).await.unwrap();
    assert!(s
        .call::<WorkspaceTrashListResult>(methods::WORKSPACE_TRASH_LIST, json!({}))
        .await
        .unwrap()
        .entries
        .is_empty());
    // clone：非法 URL 立即拒绝；连不上的地址以 job.done(ok=false) 结束且推过进度
    let e = s.call::<JobStarted>(methods::WORKSPACE_CLONE, json!({ "url": "file:///etc" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    let job: JobStarted = s
        .call(methods::WORKSPACE_CLONE, json!({ "url": "ssh://git@127.0.0.1:1/x/y.git", "name": "cl" }))
        .await
        .unwrap();
    let done = s.next_note(|n| n.method == methods::JOB_DONE).await;
    let done: JobDone = serde_json::from_value(done.params).unwrap();
    assert_eq!((done.job_id.as_str(), done.ok), (job.job_id.as_str(), false));
    assert!(!td.workspace.join("cl").exists());
    let e = s.call::<Empty>(methods::JOB_CANCEL, json!({ "job_id": job.job_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    // 审计覆盖了工作区操作
    let audit: NodeAuditResult = s.call(methods::NODE_AUDIT, json!({ "limit": 200 })).await.unwrap();
    let actions: std::collections::HashSet<&str> = audit.entries.iter().map(|e| e.action.as_str()).collect();
    for a in
        ["workspace.create", "workspace.rename", "workspace.trash", "workspace.restore", "workspace.clone", "fs.read"]
    {
        assert!(actions.contains(a), "audit is missing {a}: {actions:?}");
    }
    td.shutdown().await;
}

#[tokio::test]
async fn config_file_edits_hot_reload_without_restart() {
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"0\"")], true).await;
    let mut s = Sock::connect(&td).await;
    let created: SessionSummary =
        s.call(methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": "m" })).await.unwrap();
    let t = std::time::Instant::now();
    while s.call::<SessionSummary>(methods::SESSION_GET, json!({ "session_id": created.id })).await.unwrap().status
        != SessionStatus::Idle
    {
        assert!(t.elapsed() < T);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst), 0);
    // 只改文件（模拟 `anycon config set`），由 watcher 热加载
    anycon_daemon::config::set_value(&td.paths, "sessions.idle_timeout", "\"1s\"").unwrap();
    let t = std::time::Instant::now();
    while td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(t.elapsed() < Duration::from_secs(8), "idle_timeout change should hot-reload");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 非法值：保留旧配置
    std::fs::write(td.paths.config_toml(), "[sessions]\nmax_concurrent = \"x\"\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(td.ctx.config.get().idle_timeout(), Duration::from_secs(1));
    let st: Value = s.call("local.config_reload", json!({})).await.unwrap_or(Value::Null);
    let _ = st;
    td.shutdown().await;
}

#[tokio::test]
async fn single_instance_lock_and_not_initialized() {
    let td = TestDaemon::start_with(&[], true).await;
    let second = Daemon::start(
        td.paths.clone(),
        DaemonOptions { hub: false, socket: false, login_shell: Some(false), ..Default::default() },
    )
    .await;
    match second {
        // Windows 的文件锁是强制的，另一个打开者读不到锁文件里的 pid
        Err(DaemonError::AlreadyRunning(pid)) if cfg!(windows) => assert_eq!(pid, ""),
        Err(DaemonError::AlreadyRunning(pid)) => assert_eq!(pid, std::process::id().to_string()),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("second instance must not start"),
    }
    td.shutdown().await;
    let d = tempfile::tempdir().unwrap();
    let r = Daemon::start(
        anycon_daemon::config::Paths::new(d.path().join("nothing")),
        DaemonOptions { hub: false, socket: false, ..Default::default() },
    )
    .await;
    assert!(matches!(r, Err(ref e) if e.is_not_initialized()), "{:?}", r.err());
}

#[tokio::test]
async fn several_socket_clients_share_one_daemon() {
    let td = TestDaemon::start_with(&[], true).await;
    let mut a = Sock::connect(&td).await;
    let mut b = Sock::connect(&td).await;
    let created: SessionSummary = a.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    // b 没有 watch，但收到列表级通知
    let ch = b.next_note(|n| n.method == methods::SESSION_CHANGED).await;
    assert_eq!(ch.params["session_id"], json!(created.id));
    let _: Empty = a.call(methods::SESSION_WATCH, json!({ "session_id": created.id })).await.unwrap();
    let _: SessionSendResult = b
        .call(methods::SESSION_SEND, json!({ "session_id": created.id, "message_id": "x", "content": "from b" }))
        .await
        .unwrap();
    let ev =
        a.next_note(|n| n.method == methods::SESSION_EVENT && n.params["event"]["type"] == json!("user_message")).await;
    assert_eq!(ev.params["event"]["by_device"], json!("local"));
    td.shutdown().await;
}

#[tokio::test]
async fn requests_of_one_client_are_answered_in_the_order_they_arrived() {
    let td = TestDaemon::start_with(&[], true).await;
    let mut s = Sock::connect(&td).await;
    // 一次写出一批请求（不等响应）：create(带 prompt) -> list -> send -> history … 每个都依赖前一个的结果
    let create_id = 1u64;
    let mut lines = Vec::new();
    lines.push(RpcMessage::request(
        create_id,
        methods::SESSION_CREATE,
        json!({ "agent": "fake", "prompt": "hello", "message_id": "m-1" }),
    ));
    lines.push(RpcMessage::request(2, methods::SESSION_LIST, json!({})));
    for i in 0..20u64 {
        lines.push(RpcMessage::request(10 + i, methods::NODE_INFO, json!({})));
    }
    let mut batch = String::new();
    for l in &lines {
        batch.push_str(&String::from_utf8(l.encode()).unwrap());
        batch.push('\n');
    }
    s.w.write_all(batch.as_bytes()).await.unwrap();
    let mut order = Vec::new();
    let mut listed = None;
    while order.len() < lines.len() {
        match s.read_msg().await {
            RpcMessage::Response(r) => {
                if let RpcId::Num(id) = r.id {
                    if id == 2 {
                        listed = Some(serde_json::from_value::<SessionListResult>(r.result.unwrap()).unwrap());
                    }
                    order.push(id);
                }
            }
            RpcMessage::Notification(_) => {}
            other => panic!("{other:?}"),
        }
    }
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "responses come back in request order");
    assert_eq!(listed.unwrap().sessions.len(), 1, "the list sees the session created by the request before it");
    td.shutdown().await;
}

#[tokio::test]
async fn an_oversized_request_line_closes_the_connection_without_buffering_it_all() {
    let td = TestDaemon::start_with(&[], true).await;
    let mut s = Sock::connect(&td).await;
    // 17 MiB 没有换行：超过 16 MiB 上限的那一刻就断开
    let chunk = vec![b'a'; 1024 * 1024];
    let mut closed = false;
    for _ in 0..18 {
        if s.w.write_all(&chunk).await.is_err() {
            closed = true;
            break;
        }
    }
    if !closed {
        let mut line = String::new();
        let n = tokio::time::timeout(T, s.r.read_line(&mut line)).await.expect("the daemon reacts").unwrap_or(0);
        assert_eq!(n, 0, "connection closed");
    }
    // 别的连接不受影响
    let mut ok = Sock::connect(&td).await;
    let _: NodeInfo = ok.call(methods::NODE_INFO, json!({})).await.unwrap();
    td.shutdown().await;
}
