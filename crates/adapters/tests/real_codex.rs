//! 真实 `codex` CLI 相关的测试。默认 `#[ignore]`，且需要 `ANYCON_REAL_AGENT_TESTS=1`：
//!
//! ```text
//! ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters --test real_codex -- --ignored --nocapture
//! ```
//!
//! - `real_schema_*`：用本机 `codex app-server generate-json-schema` 生成的 schema（**不入库**）校验全部 fixture，以及
//!   adapter 实际发给 app-server 的每一条请求 / 答复。**不需要登录**，不联网。这是 ADR-26 里「CI 用 schema 校验」的落实。
//! - `real_app_server_handshake`：对真 app-server 跑探测、`thread/start`、`thread/inject_items`、`thread/resume` 的回退。
//!   用临时的 `CODEX_HOME`，不碰用户的 `~/.codex`；**不需要登录**（这些调用都不联网）。
//! - `real_custom_provider_*`：用 `tests/bin/fake-responses`（本地的假模型服务）加一个临时 `CODEX_HOME`，对真 app-server 跑完整的会话：
//!   自定义提供方不需要 OpenAI 登录（`account/read`）、11 MB 的补丁不会弄死会话、命令输出很密时 `cancel` 照样及时返回。
//!   不联网，也**不走用户配置的提供方**。
//! - `real_logged_in_turn`：需要 `codex login`；没登录就跳过。会通过用户配置的提供方花一个很短的调用——
//!   需要时用 `--skip real_logged_in_turn` 跳过它。

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::common::run_capture;
use anycon_adapters::CodexAdapter;
use anycon_agent_api::{
    Adapter, AdapterConfig, AgentEvent, AgentHandle, PermissionDecision, ProbeContext, ResumeHandle, SpawnRequest,
    ENV_STRIP,
};
use anycon_protocol::{AgentStatus, Event, SessionSettings, TurnDoneReason};
use common::*;
use serde_json::{json, Value};

fn enabled() -> bool {
    std::env::var("ANYCON_REAL_AGENT_TESTS").ok().as_deref() == Some("1")
}

fn real_env() -> HashMap<String, String> {
    std::env::vars().filter(|(k, _)| !ENV_STRIP.contains(&k.as_str())).collect()
}

/// 真 `codex` 的路径（PATH 里的，或 nvm / volta 等已知目录里的）。
fn real_codex() -> Option<PathBuf> {
    let env = real_env();
    anycon_adapters::common::bin::collect_candidates("codex", &env, &std::env::temp_dir())
        .into_iter()
        .next()
        .map(|(p, _)| p)
}

// ---------------------------------------------------------------- schema

struct Schemas {
    dir: tempfile::TempDir,
}

impl Schemas {
    /// 生成本机 codex 的 schema（稳定版接口，和 adapter 用的一致）。
    async fn generate(codex: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let out = run_capture(
            codex,
            &[
                "app-server".into(),
                "generate-json-schema".into(),
                "--out".into(),
                dir.path().to_string_lossy().into_owned(),
            ],
            &real_env(),
            &std::env::temp_dir(),
            Duration::from_secs(120),
        )
        .await;
        assert!(out.success(), "generate-json-schema failed: {}", out.failure_reason());
        Self { dir }
    }

    fn load(&self, rel: &str) -> Value {
        let p = self.dir.path().join(rel);
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
            .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// 校验 `instance`，返回错误列表（空 = 通过）。
    fn check(&self, rel: &str, instance: &Value) -> Vec<String> {
        let schema = self.load(rel);
        let validator = jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("{rel}: bad schema: {e}"));
        validator.iter_errors(instance).map(|e| format!("{} (at {})", e, e.instance_path())).collect()
    }

    fn assert_valid(&self, rel: &str, instance: &Value, what: &str) {
        let errs = self.check(rel, instance);
        assert!(
            errs.is_empty(),
            "{what} does not match {rel}:\n  {}\n  message: {}",
            errs.join("\n  "),
            truncate(&instance.to_string())
        );
    }
}

fn truncate(s: &str) -> String {
    if s.len() > 600 {
        format!("{}…", &s[..600])
    } else {
        s.to_string()
    }
}

fn fixture_lines(name: &str) -> Vec<Value> {
    std::fs::read_to_string(codex_fixture_path(name))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// 服务端请求的 method → 答复的 schema 文件。
fn response_schema(method: &str) -> Option<&'static str> {
    Some(match method {
        "item/commandExecution/requestApproval" => "CommandExecutionRequestApprovalResponse.json",
        "item/fileChange/requestApproval" => "FileChangeRequestApprovalResponse.json",
        "item/permissions/requestApproval" => "PermissionsRequestApprovalResponse.json",
        "item/tool/requestUserInput" => "ToolRequestUserInputResponse.json",
        "mcpServer/elicitation/request" => "McpServerElicitationRequestResponse.json",
        "item/tool/call" => "DynamicToolCallResponse.json",
        "execCommandApproval" => "ExecCommandApprovalResponse.json",
        "applyPatchApproval" => "ApplyPatchApprovalResponse.json",
        _ => return None,
    })
}

/// fake-codex 的日志里 adapter 发出的全部消息，逐条按 schema 校验。
fn validate_client_log(s: &Schemas, h: &CodexHarness, what: &str) -> usize {
    let entries = h.log_entries();
    let server_requests: HashMap<u64, String> = entries
        .iter()
        .filter(|e| e["kind"] == "server_request")
        .map(|e| (e["id"].as_u64().unwrap(), e["method"].as_str().unwrap().to_string()))
        .collect();
    let mut n = 0;
    for m in entries.iter().filter(|e| e["kind"] == "stdin").map(|e| &e["msg"]) {
        n += 1;
        match (m.get("method"), m.get("id")) {
            (Some(_), Some(_)) => s.assert_valid("ClientRequest.json", m, &format!("{what}: request {}", m["method"])),
            (Some(_), None) => {
                s.assert_valid("ClientNotification.json", m, &format!("{what}: notification {}", m["method"]))
            }
            (None, Some(id)) => {
                let method = server_requests
                    .get(&id.as_u64().unwrap())
                    .unwrap_or_else(|| panic!("{what}: answer to unknown request {id}"));
                match (response_schema(method), m.get("result")) {
                    (Some(rel), Some(result)) => s.assert_valid(rel, result, &format!("{what}: answer to {method}")),
                    // 没有对应答复 schema 的（ChatGPT token 刷新、attestation）答 JSON-RPC 错误
                    (None, _) => {
                        assert!(m.get("error").is_some(), "{what}: {method} should be answered with an error: {m}")
                    }
                    (Some(_), None) => panic!("{what}: {method} was answered with an error: {m}"),
                }
            }
            _ => panic!("{what}: unexpected message {m}"),
        }
    }
    n
}

async fn run_to_done(
    h: &mut Box<dyn AgentHandle>,
    text: &str,
    mut answer: impl FnMut(&anycon_agent_api::PendingPermission) -> PermissionDecision,
) {
    h.send(input(text)).await.expect("send");
    loop {
        let ev = next_event(h).await;
        match &ev {
            AgentEvent::Permission(p) => {
                let d = answer(p);
                h.respond_permission(&p.request_id, d).await.unwrap();
            }
            e if is_turn_done(e) => return,
            _ => {}
        }
    }
}

#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_schema_validates_every_fixture() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let Some(codex) = real_codex() else {
        eprintln!("skipped: no codex on this machine");
        return;
    };
    let s = Schemas::generate(&codex).await;
    let dir = codex_fixture_path("");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".jsonl") {
            // `unknown_*` 故意带着 schema 里没有的东西
            if stem.starts_with("unknown_") {
                continue;
            }
            for (i, msg) in fixture_lines(&name).iter().enumerate() {
                let rel = if msg.get("id").is_some() { "ServerRequest.json" } else { "ServerNotification.json" };
                s.assert_valid(rel, msg, &format!("{name}:{}", i + 1));
                checked += 1;
            }
        }
    }
    assert!(checked > 200, "only {checked} messages checked");
    // 单个 JSON 文件：app-server 的各种响应
    let one = |file: &str| {
        serde_json::from_str::<Value>(&std::fs::read_to_string(codex_fixture_path(file)).unwrap()).unwrap()
    };
    s.assert_valid("v1/InitializeResponse.json", &one("initialize_response.json"), "initialize_response");
    s.assert_valid("v2/ModelListResponse.json", &one("model_list.json"), "model_list");
    s.assert_valid("v2/ConfigReadResponse.json", &one("config_read_empty.json"), "config_read_empty");
    s.assert_valid("v2/ConfigReadResponse.json", &one("config_read_custom.json"), "config_read_custom");
    s.assert_valid("v2/ThreadStartResponse.json", &one("thread_start_response.json"), "thread_start_response");
    s.assert_valid("v2/ThreadResumeResponse.json", &one("thread_resume_response.json"), "thread_resume_response");
    for name in ["out", "custom", "apikey", "chatgpt"] {
        s.assert_valid("v2/GetAccountResponse.json", &one(&format!("account_read_{name}.json")), name);
    }
    eprintln!("validated {checked} fixture messages against the local codex schema");
}

#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_schema_validates_what_the_adapter_sends() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let Some(codex) = real_codex() else {
        eprintln!("skipped: no codex on this machine");
        return;
    };
    let s = Schemas::generate(&codex).await;
    let mut total = 0;

    // A. 探测 + 一个带审批的 turn + 改设置（模式 / 档位 / 摘要 / 指令）+ 第二个 turn
    {
        let h = CodexHarness::new().script("real_exec_approval.jsonl");
        CodexAdapter::new().probe(&h.probe_ctx()).await;
        total += validate_client_log(&s, &h, "probe");
        let _ = std::fs::remove_file(&h.log);
        let mut settings = SessionSettings {
            model: Some("gpt-6-sol".into()),
            thinking: Some("high".into()),
            mode: Some("read_only".into()),
            system_prompt_append: Some("Be terse.".into()),
            ..Default::default()
        };
        let mut handle = h.spawn(settings.clone(), None).await;
        let _ = next_event(&mut handle).await;
        run_to_done(&mut handle, "run a command", |_| PermissionDecision {
            option_id: Some("allow_session".into()),
            ..Default::default()
        })
        .await;
        // auto_review：approvalsReviewer = auto_review（schema 的枚举里合法）
        settings.mode = Some("auto_review".into());
        handle.set_settings(&settings).await.unwrap();
        run_to_done(&mut handle, "review it", |_| unreachable!()).await;
        settings.mode = Some("full_access".into());
        settings.model = Some("gpt-6-luna".into());
        settings.thinking = Some("max".into());
        settings.options.insert("fast_mode".into(), json!(true));
        settings.options.insert("model_reasoning_summary".into(), json!("detailed"));
        settings.system_prompt_append = Some("Be verbose.".into());
        handle.set_settings(&settings).await.unwrap();
        run_to_done(&mut handle, "again", |_| unreachable!()).await;
        // 关掉 fast：退回标准速度
        settings.options.remove("fast_mode");
        handle.set_settings(&settings).await.unwrap();
        run_to_done(&mut handle, "once more", |_| unreachable!()).await;
        handle.shutdown().await.unwrap();
        total += validate_client_log(&s, &h, "approval turn");
        let turns = h.requests("turn/start");
        assert!(
            turns.iter().any(|r| r["params"]["approvalsReviewer"] == "auto_review"),
            "no turn/start carried approvalsReviewer = auto_review"
        );
        assert!(h.requests("thread/start").iter().all(|r| r["params"]["approvalsReviewer"] == "user"));
    }

    // A2. 一开始就是 auto_review 的线程
    {
        let h = CodexHarness::new();
        let mut handle =
            h.spawn(SessionSettings { mode: Some("auto_review".into()), ..Default::default() }, None).await;
        let _ = next_event(&mut handle).await;
        run_to_done(&mut handle, "go", |_| unreachable!()).await;
        handle.shutdown().await.unwrap();
        assert_eq!(h.requests("thread/start")[0]["params"]["approvalsReviewer"], "auto_review");
        assert_eq!(h.requests("turn/start")[0]["params"]["approvalsReviewer"], "auto_review");
        total += validate_client_log(&s, &h, "auto_review thread");
    }

    // B. 转向与取消
    {
        let h = CodexHarness::new().script("long_text.jsonl").var("FAKE_CODEX_DELAY_MS", "30");
        let mut handle = h.spawn(SessionSettings::default(), None).await;
        let _ = next_event(&mut handle).await;
        handle.send(input("go")).await.unwrap();
        collect_until(&mut handle, |e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).await;
        handle.send(interject("steer")).await.unwrap();
        handle.cancel().await.unwrap();
        collect_until(&mut handle, is_turn_done).await;
        handle.shutdown().await.unwrap();
        total += validate_client_log(&s, &h, "steer + interrupt");
    }

    // C. 续接（带图片）
    {
        let h = CodexHarness::new();
        let resume = ResumeHandle {
            agent_session_id: "thr-from-before".into(),
            launch_args: json!({"instructions_applied": "old"}),
        };
        let mut handle = h
            .spawn(SessionSettings { system_prompt_append: Some("new".into()), ..Default::default() }, Some(resume))
            .await;
        let _ = next_event(&mut handle).await;
        let with_image = anycon_agent_api::UserInput {
            message_id: "m".into(),
            interject: false,
            content: "look".into(),
            attachments: vec![anycon_protocol::AttachmentRef {
                blob_id: "b".into(),
                name: "a.png".into(),
                mime: "image/png".into(),
                bytes: 1,
                width: None,
                height: None,
            }],
            attachment_paths: vec!["/tmp/anycon-a.png".into()],
        };
        handle.send(with_image).await.unwrap();
        collect_until(&mut handle, is_turn_done).await;
        handle.shutdown().await.unwrap();
        total += validate_client_log(&s, &h, "resume + image");
    }

    // D. 各种服务端请求的答复
    for (script, decision) in [
        ("real_permissions_request.jsonl", "allow"),
        ("real_user_input_request.jsonl", "answer"),
        ("real_patch_approval.jsonl", "deny"),
        ("unsupported_requests.jsonl", "allow"),
    ] {
        let h = CodexHarness::new().script(script);
        let mut handle = h.spawn(SessionSettings::default(), None).await;
        let _ = next_event(&mut handle).await;
        run_to_done(&mut handle, "go", |_| PermissionDecision {
            option_id: Some(decision.into()),
            answers: (decision == "answer")
                .then(|| serde_json::Map::from_iter([("which_db".to_string(), json!("Postgres"))])),
            message: None,
        })
        .await;
        handle.shutdown().await.unwrap();
        total += validate_client_log(&s, &h, script);
    }
    assert!(total > 40, "only {total} client messages checked");
    eprintln!("validated {total} client messages against the local codex schema");
}

// ---------------------------------------------------------------- 真 app-server

#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_app_server_handshake() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    if real_codex().is_none() {
        eprintln!("skipped: no codex on this machine");
        return;
    }
    // 临时 CODEX_HOME：不碰用户的 ~/.codex，也不会有登录
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut env = real_env();
    env.insert("CODEX_HOME".into(), home.path().to_string_lossy().into_owned());
    let ctx = ProbeContext {
        env: Arc::new(env.clone()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: false,
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let adapter = CodexAdapter::new();
    let info = Arc::new(adapter.probe(&ctx).await);
    eprintln!(
        "probe: status={:?} version={:?} path={:?} models={}",
        info.status,
        info.version,
        info.path,
        info.models.len()
    );
    // 空的 CODEX_HOME = 没登录；`model/list` 不需要登录
    assert_eq!(info.status, AgentStatus::NotAuthenticated, "{:?}", info.issue);
    assert_eq!(info.issue.as_ref().unwrap().hint.as_deref(), Some("codex login"));
    assert!(!info.models.is_empty(), "model/list should work without a login");
    assert_eq!(info.models.iter().filter(|m| m.default).count(), 1);
    assert!(info
        .models
        .iter()
        .all(|m| m.thinking.as_ref().is_none_or(|t| t.options.iter().any(|o| Some(&o.id) == t.default.as_ref()))));
    let (text, fields) = anycon_adapters::diagnose("codex", &ctx).await.unwrap();
    eprintln!("{text}");
    assert!(fields.iter().any(|f| f.label == "status" && f.value == "not_authenticated"));

    // 拉起、thread/start、改指令（thread/inject_items）、关闭
    let spawn = |settings: SessionSettings, resume: Option<ResumeHandle>| {
        let req = SpawnRequest {
            session_id: "sess_real".into(),
            cwd: project.path().canonicalize().unwrap(),
            settings,
            env: Arc::new(env.clone()),
            config: AdapterConfig { enabled: true, ..Default::default() },
            resume,
            agent_info: info.clone(),
        };
        async move { CodexAdapter::new().spawn(req).await }
    };
    let first = SessionSettings {
        mode: Some("read_only".into()),
        system_prompt_append: Some("Be terse.".into()),
        ..Default::default()
    };
    let started = |ev: AgentEvent| match ev {
        AgentEvent::Started { resume } => resume,
        other => panic!("expected Started, got {other:?}"),
    };

    // 1. 拉起 → thread/start → 关闭。这个 thread 从来没有 turn，没有 rollout
    let mut h = spawn(first.clone(), None).await.expect("spawn real codex");
    let a = started(next_event(&mut h).await);
    eprintln!("thread A = {}", a.agent_session_id);
    assert!(!a.agent_session_id.is_empty());
    assert_eq!(a.launch_args["instructions_applied"], "Be terse.");
    h.shutdown().await.unwrap();
    collect_until(&mut h, is_exited).await;

    // 2. 续接 A：没有 rollout，真 app-server 报 "no rollout found"，adapter 回退成一个新 thread
    let mut h2 = spawn(first.clone(), Some(a.clone())).await.expect("resume falls back to a new thread");
    let b = started(next_event(&mut h2).await);
    eprintln!("fell back to thread B = {}", b.agent_session_id);
    assert_ne!(b.agent_session_id, a.agent_session_id);
    // 可见的提示：之前的上下文 agent 看不到了
    match next_event(&mut h2).await {
        AgentEvent::Event(Event::Error { code, recoverable, message }) => {
            assert_eq!((code.as_str(), recoverable), ("agent_context_reset", true));
            assert!(message.contains("no rollout found"), "{message}");
        }
        other => panic!("expected the context-reset notice, got {other:?}"),
    }
    h2.shutdown().await.unwrap();
    collect_until(&mut h2, is_exited).await;

    // 3. 新 thread C：改指令走 thread/inject_items（真 app-server 接受）；注入会落下 rollout，所以之后可以真的续接
    let mut h3 = spawn(first.clone(), None).await.expect("spawn real codex");
    let c = started(next_event(&mut h3).await);
    let mut second = first.clone();
    second.system_prompt_append = Some("Answer in French.".into());
    second.mode = Some("auto_review".into());
    second.options.insert("fast_mode".into(), json!(true));
    let apply = h3.set_settings(&second).await.expect("thread/inject_items against the real server");
    assert!(apply.applied_now);
    let updated = started(next_event(&mut h3).await);
    assert_eq!(updated.launch_args["instructions_applied"], "Answer in French.");
    h3.shutdown().await.unwrap();
    collect_until(&mut h3, is_exited).await;
    let mut h4 = spawn(second.clone(), Some(updated.clone())).await.expect("resume an existing thread");
    let c2 = started(next_event(&mut h4).await);
    assert_eq!(c2.agent_session_id, c.agent_session_id, "a thread with a rollout resumes under the same id");
    // 指令没变：续接时不再注入
    assert_eq!(c2.launch_args["instructions_applied"], "Answer in French.");
    h4.shutdown().await.unwrap();
    collect_until(&mut h4, is_exited).await;
}

/// 归档（可逆，不是删除）测试留下的 thread。
async fn archive_thread(codex: &Path, env: &HashMap<String, String>, thread_id: &str) {
    use anycon_adapters::codex::rpc::{classify, Incoming, RpcClient};
    use anycon_adapters::common::{JsonlChild, SpawnSpec, DEFAULT_MAX_LINE_BYTES};
    let spec = SpawnSpec {
        program: codex.to_path_buf(),
        args: vec!["app-server".into()],
        cwd: std::env::temp_dir(),
        env: env.clone(),
        max_line_bytes: DEFAULT_MAX_LINE_BYTES,
    };
    let (child, mut lines) = JsonlChild::spawn(&spec).expect("spawn codex app-server");
    let child = Arc::new(child);
    let rpc = Arc::new(RpcClient::new(child.clone()));
    let reader = {
        let rpc = rpc.clone();
        tokio::spawn(async move {
            while let Some(l) = lines.recv().await {
                if let Some(Incoming::Response { id, result }) = classify(&l) {
                    rpc.resolve(id, result);
                }
            }
            rpc.close();
        })
    };
    let t = Duration::from_secs(20);
    let _ = rpc.request("initialize", anycon_adapters::codex::probe::initialize_params(), t).await;
    rpc.notify("initialized");
    match rpc.request("thread/archive", json!({ "threadId": thread_id }), t).await {
        Ok(_) => eprintln!("archived the test thread {thread_id}"),
        Err(e) => eprintln!("could not archive the test thread {thread_id}: {e}"),
    }
    child.shutdown(anycon_adapters::common::ShutdownTimings::default()).await;
    reader.abort();
}

// ---------------------------------------------------------------- 真 app-server + 本地假模型服务

/// `tests/bin/fake-responses` 进程；析构时杀掉。
struct Stub {
    child: std::process::Child,
    port: u16,
    _plan: tempfile::NamedTempFile,
    /// 假服务收到的每个模型请求（一行一个 JSON）。
    log: tempfile::NamedTempFile,
}

impl Stub {
    fn start(plan: &Value) -> Self {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(plan.to_string().as_bytes()).unwrap();
        let log = tempfile::NamedTempFile::new().unwrap();
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/bin/fake-responses");
        let mut child = std::process::Command::new(script)
            .arg(file.path())
            .arg(log.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start fake-responses");
        let mut first = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).unwrap();
        let port: u16 = first.trim().strip_prefix("PORT=").expect("PORT=<n>").parse().unwrap();
        Self { child, port, _plan: file, log }
    }

    /// 到目前为止收到的模型请求（请求体）。
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 临时 `CODEX_HOME`，`config.toml` 里配一个指向假模型服务的自定义提供方。
struct StubHome {
    stub: Stub,
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    scratch: tempfile::TempDir,
    env: HashMap<String, String>,
}

impl StubHome {
    fn new(plan: Value) -> Self {
        let stub = Stub::start(&plan);
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "model = \"fake-model\"\nmodel_provider = \"fake\"\n\n[model_providers.fake]\nname = \"Fake\"\nbase_url = \"http://127.0.0.1:{}/v1\"\nwire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\n",
                stub.port
            ),
        )
        .unwrap();
        let mut env = real_env();
        env.insert("CODEX_HOME".into(), home.path().to_string_lossy().into_owned());
        // 本机的代理设置不能把 127.0.0.1 也绕出去
        for k in ["NO_PROXY", "no_proxy"] {
            env.insert(k.into(), "127.0.0.1,localhost".into());
        }
        Self { stub, _home: home, project: tempfile::tempdir().unwrap(), scratch: tempfile::tempdir().unwrap(), env }
    }

    fn probe_ctx(&self) -> ProbeContext {
        ProbeContext {
            env: Arc::new(self.env.clone()),
            config: AdapterConfig { enabled: true, ..Default::default() },
            deadline: Duration::from_secs(60),
            cheap: false,
            previous: None,
            scratch_dir: self.scratch.path().to_path_buf(),
        }
    }

    async fn spawn(&self, info: Arc<anycon_protocol::AgentInfo>, settings: SessionSettings) -> Box<dyn AgentHandle> {
        let req = SpawnRequest {
            session_id: "sess_stub".into(),
            cwd: self.project.path().canonicalize().unwrap(),
            settings,
            env: Arc::new(self.env.clone()),
            config: AdapterConfig { enabled: true, ..Default::default() },
            resume: None,
            agent_info: info,
        };
        CodexAdapter::new().spawn(req).await.expect("spawn real codex against the stub provider")
    }
}

#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_custom_provider_is_ready_and_survives_a_huge_patch() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    if real_codex().is_none() {
        eprintln!("skipped: no codex on this machine");
        return;
    }
    // 一个 11 MB 的新文件补丁：真实 codex 0.157.1 会产生 11 MB 的 item/started、item/completed（和 turn/diff/updated，已经 opt out），
    // 都超过 8 MiB 的行上限
    let content: String = (0..110_000).map(|i| format!("+line {i} {}\n", "x".repeat(90))).collect();
    let patch = format!("apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: big.txt\n{content}*** End Patch\nEOF\n");
    let t = StubHome::new(json!([
        [{"function_call": {"name": "exec_command", "arguments": {"cmd": patch}}}],
        [{"text": "The file is written."}]
    ]));
    let adapter = CodexAdapter::new();

    // 探测：空的 CODEX_HOME，`login status` 说没登录；自定义提供方 `requiresOpenaiAuth = false`，所以是 ready
    let info = Arc::new(adapter.probe(&t.probe_ctx()).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let (_, fields) = anycon_adapters::diagnose("codex", &t.probe_ctx()).await.unwrap();
    let field = |l: &str| fields.iter().find(|f| f.label == l).map(|f| f.value.clone()).unwrap_or_default();
    assert!(field("auth").contains("not logged in"), "{}", field("auth"));
    assert!(field("account").contains("does not need an OpenAI login"), "{}", field("account"));
    assert_eq!(field("model provider"), "fake");
    // config.toml 里的 model 成了默认模型
    assert_eq!(info.models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["fake-model"]);
    // cheap 探测：login status 没变就沿用，不再拉起 app-server（同一个 adapter 实例）
    let mut cheap = t.probe_ctx();
    cheap.cheap = true;
    cheap.previous = Some((*info).clone());
    assert_eq!(adapter.probe(&cheap).await.status, AgentStatus::Ready);

    let mut h = t.spawn(info, SessionSettings::default()).await;
    let _ = next_event(&mut h).await;
    h.send(input("write a big file")).await.unwrap();
    let evs = tokio::time::timeout(Duration::from_secs(120), collect_until(&mut h, is_turn_done))
        .await
        .expect("turn timed out");
    let events: Vec<Event> =
        evs.iter().filter_map(|e| if let AgentEvent::Event(ev) = e { Some(ev.clone()) } else { None }).collect();
    eprintln!("events: {:?}", names(&evs));
    // 进程没被杀、turn 正常结束、没有错误
    assert!(events.iter().all(|e| !matches!(e, Event::Error { .. })), "{:?}", names(&evs));
    match events.last().unwrap() {
        Event::TurnDone { reason, .. } => assert_eq!(*reason, TurnDoneReason::Completed),
        _ => unreachable!(),
    }
    // 补丁的调用 / 结果是「内容没显示」的版本
    let call_title = events
        .iter()
        .find_map(|e| if let Event::ToolCall { title, .. } = e { Some(title.clone()) } else { None })
        .expect("a tool_call for the patch");
    assert!(call_title.contains("big.txt") && call_title.contains("too large"), "{call_title}");
    assert!(events.iter().any(|e| matches!(e, Event::ToolResult { output, .. } if output.as_str().is_some_and(|o| o.starts_with("[result not shown")))));
    assert!(text_of(&evs).contains("The file is written"));
    // 文件确实写出来了
    assert!(std::fs::metadata(t.project.path().join("big.txt")).unwrap().len() > 10_000_000);
    // 同一个进程还能再来一轮
    h.send(input("again")).await.unwrap();
    let evs =
        tokio::time::timeout(Duration::from_secs(60), collect_until(&mut h, is_turn_done)).await.expect("second turn");
    assert!(matches!(evs.last().unwrap(), AgentEvent::Event(Event::TurnDone { .. })));
    h.shutdown().await.unwrap();
    collect_until(&mut h, is_exited).await;
}

#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_cancel_under_heavy_command_output_returns_promptly() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    if real_codex().is_none() {
        eprintln!("skipped: no codex on this machine");
        return;
    }
    let t = StubHome::new(json!([
        [{"function_call": {"name": "exec_command", "arguments": {"cmd": "yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "yield_time_ms": 30000}}}],
        [{"text": "unreachable"}]
    ]));
    let info = Arc::new(CodexAdapter::new().probe(&t.probe_ctx()).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let mut h = t.spawn(info, SessionSettings::default()).await;
    let _ = next_event(&mut h).await;
    h.send(input("run yes")).await.unwrap();
    collect_until(&mut h, |e| matches!(e, AgentEvent::Event(Event::ToolOutputDelta { .. }))).await;
    // 不读事件，让输出堆几秒
    tokio::time::sleep(Duration::from_secs(3)).await;
    let began = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), h.cancel())
        .await
        .expect("cancel() must not block on unread output")
        .unwrap();
    eprintln!("cancel() returned after {:?}", began.elapsed());
    let evs =
        tokio::time::timeout(Duration::from_secs(60), collect_until(&mut h, is_turn_done)).await.expect("turn_done");
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { reason, .. }) => assert_eq!(*reason, TurnDoneReason::Cancelled),
        _ => unreachable!(),
    }
    let deltas = evs.iter().filter(|e| matches!(e, AgentEvent::Event(Event::ToolOutputDelta { .. }))).count();
    eprintln!("{deltas} more output deltas were queued while nobody was reading");
    h.shutdown().await.unwrap();
    collect_until(&mut h, is_exited).await;
}

/// 被 `turn/steer` 接受的消息，随后 turn 被 interrupt，会不会丢？（真实 codex 0.157.1 上会丢，所以 `cancel` 之后的插话要退回队列。）
/// 这条测试把「会丢」固定下来：哪天 codex 改了，它会失败，提醒我们重新考虑 adapter 里「`cancel` 之后的插话 = `TurnEnded`」的取舍。
#[tokio::test]
#[ignore = "needs the real codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_a_steered_message_is_lost_when_the_turn_is_then_interrupted() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    if real_codex().is_none() {
        eprintln!("skipped: no codex on this machine");
        return;
    }
    // 第一个模型请求在途（假服务睡 4 秒才回），期间插话、再打断；然后再来一个 turn，看那条插话有没有进历史
    let t = StubHome::new(json!([
        [{"sleep": 4}, {"text": "first answer"}],
        [{"text": "second answer"}]
    ]));
    let info = Arc::new(CodexAdapter::new().probe(&t.probe_ctx()).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let mut h = t.spawn(info, SessionSettings::default()).await;
    let _ = next_event(&mut h).await;
    h.send(input("first")).await.unwrap();
    // 等第一个模型请求到了假服务，才算「在途」
    let began = std::time::Instant::now();
    while t.stub.requests().is_empty() {
        assert!(began.elapsed() < Duration::from_secs(30), "the first model request never arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 真 codex 接受了这条 steer（响应 `{turnId}`）……
    h.send(interject("STEERED-MSG-123")).await.expect("codex accepts the steer while a model request is in flight");
    h.cancel().await.unwrap();
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut h, is_turn_done))
        .await
        .expect("the interrupted turn must finish");
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { reason, .. }) => assert_eq!(*reason, TurnDoneReason::Cancelled),
        _ => unreachable!(),
    }
    // ……取消之后的插话 adapter 不再发 steer，而是 TurnEnded（daemon 会把它退回队列）
    assert!(matches!(h.send(interject("too late")).await, Err(anycon_agent_api::AdapterError::TurnEnded)));
    // 下一个 turn
    h.send(input("second turn")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), collect_until(&mut h, is_turn_done)).await.expect("second turn");
    let bodies: Vec<String> = t.stub.requests().iter().map(Value::to_string).collect();
    eprintln!("{} model requests", bodies.len());
    assert!(bodies.last().is_some_and(|b| b.contains("second turn")), "the next turn reached the model");
    assert!(
        bodies.iter().all(|b| !b.contains("STEERED-MSG-123")),
        "codex now keeps a steered message across an interrupt; revisit the cancel-pending rule in codex/handle.rs"
    );
    h.shutdown().await.unwrap();
    collect_until(&mut h, is_exited).await;
}

#[tokio::test]
#[ignore = "needs a logged-in codex CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_logged_in_turn() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    if real_codex().is_none() {
        eprintln!("skipped: no codex on this machine");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let env = real_env();
    let mut ctx = ProbeContext {
        env: Arc::new(env.clone()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: true, // 只看认证，不拉起 app-server
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let adapter = CodexAdapter::new();
    if adapter.probe(&ctx).await.status != AgentStatus::Ready {
        eprintln!("skipped: codex is not logged in on this machine (run `codex login`)");
        return;
    }
    ctx.cheap = false;
    let info = Arc::new(adapter.probe(&ctx).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    // 最便宜的组合：默认模型里思考等级最低的
    let model = info.models.iter().find(|m| m.default).or(info.models.first()).expect("a model");
    let low = model.thinking.as_ref().and_then(|t| t.options.first().map(|o| o.id.clone()));
    let settings = SessionSettings {
        model: Some(model.id.clone()),
        thinking: low,
        mode: Some("read_only".into()),
        ..Default::default()
    };
    let req = SpawnRequest {
        session_id: "sess_real".into(),
        cwd,
        settings,
        env: Arc::new(env),
        config: AdapterConfig { enabled: true, ..Default::default() },
        resume: None,
        agent_info: info,
    };
    let codex = real_codex().expect("codex");
    let env_for_cleanup = real_env();
    let mut h = adapter.spawn(req).await.expect("spawn real codex");
    let thread_id = match next_event(&mut h).await {
        AgentEvent::Started { resume } => resume.agent_session_id,
        other => panic!("expected Started, got {other:?}"),
    };
    h.send(input("reply with the single word pong")).await.unwrap();
    let evs = collect_until(&mut h, is_turn_done).await;
    eprintln!("events: {:?}", names(&evs));
    h.shutdown().await.unwrap();
    collect_until(&mut h, is_exited).await;
    // 先清理再断言：这个 turn 在用户的 ~/.codex 里留下了一个 thread，归档掉
    archive_thread(&codex, &env_for_cleanup, &thread_id).await;
    assert!(text_of(&evs).to_lowercase().contains("pong"), "got: {}", text_of(&evs));
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { reason, usage, .. }) => {
            assert_eq!(*reason, TurnDoneReason::Completed);
            let u = usage.as_ref().expect("usage");
            eprintln!("usage: {u:?}");
            assert!(u.output_tokens.unwrap_or(0) > 0);
        }
        _ => unreachable!(),
    }
}
