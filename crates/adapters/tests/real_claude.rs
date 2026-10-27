//! 真实 `claude` CLI 的冒烟测试。默认 `#[ignore]`，且需要 `ANYCON_REAL_AGENT_TESTS=1`：
//!
//! ```text
//! ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters -- --ignored real_ --nocapture
//! ```
//!
//! 要求本机的 `claude` 已登录。会花几次很短的 haiku 调用（约几美分以内）。在临时目录里跑，结束后清理它在
//! `~/.claude/projects/` 下留下的会话目录。

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::ClaudeCodeAdapter;
use anycon_agent_api::{
    Adapter, AdapterConfig, AgentEvent, AgentHandle, PermissionDecision, ProbeContext, ResumeHandle, SpawnRequest,
    UserInput, ENV_STRIP,
};
use anycon_protocol::{
    AgentStatus, AttachmentRef, Event, FileChangeKind, PermissionKind, SessionSettings, TaskAgentState, TaskKind,
    TaskStatus, ToolCategory, TurnDoneReason,
};
use common::*;

fn enabled() -> bool {
    std::env::var("ANYCON_REAL_AGENT_TESTS").ok().as_deref() == Some("1")
}

fn real_env() -> HashMap<String, String> {
    // 和 daemon 一样：剔除「我正在某个 agent 会话里」的标记（只在 daemon 的 env.rs 做；adapter 不再剔），
    // 否则在 Claude Code 里跑这个测试时，嵌套启动会失败
    std::env::vars().filter(|(k, _)| !ENV_STRIP.contains(&k.as_str())).collect()
}

fn encoded_project_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let real = cwd.canonicalize().ok()?;
    let name: String =
        real.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let home = std::env::var("HOME").ok()?;
    let p = std::path::Path::new(&home).join(".claude/projects").join(name);
    p.starts_with(std::path::Path::new(&home).join(".claude/projects")).then_some(p)
}

async fn spawn_real(
    adapter: &ClaudeCodeAdapter,
    info: Arc<anycon_protocol::AgentInfo>,
    cwd: &std::path::Path,
    settings: SessionSettings,
    resume: Option<ResumeHandle>,
) -> Box<dyn AgentHandle> {
    let req = SpawnRequest {
        session_id: "sess_real".into(),
        cwd: cwd.to_path_buf(),
        settings,
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        resume,
        agent_info: info,
    };
    adapter.spawn(req).await.expect("spawn real claude")
}

async fn turn(
    h: &mut Box<dyn AgentHandle>,
    prompt: &str,
    on_perm: impl FnMut(&anycon_agent_api::PendingPermission) -> PermissionDecision,
) -> Vec<AgentEvent> {
    turn_with(h, input(prompt), on_perm).await
}

async fn turn_with(
    h: &mut Box<dyn AgentHandle>,
    user_input: UserInput,
    mut on_perm: impl FnMut(&anycon_agent_api::PendingPermission) -> PermissionDecision,
) -> Vec<AgentEvent> {
    h.send(user_input).await.expect("send");
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let ev = tokio::time::timeout_at(deadline, h.events().recv())
            .await
            .expect("turn timed out")
            .expect("channel closed");
        if let AgentEvent::Permission(p) = &ev {
            let d = on_perm(p);
            let rid = p.request_id.clone();
            out.push(ev);
            h.respond_permission(&rid, d).await.expect("respond");
            continue;
        }
        let done = is_turn_done(&ev);
        out.push(ev);
        if done {
            return out;
        }
    }
}

#[tokio::test]
#[ignore = "needs the real claude CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_probe_is_ready() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let ctx = ProbeContext {
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: false,
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let info = ClaudeCodeAdapter::new().probe(&ctx).await;
    eprintln!(
        "probe: status={:?} version={:?} path={:?} models={}",
        info.status,
        info.version,
        info.path,
        info.models.len()
    );
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let (text, fields) = anycon_adapters::diagnose("claude-code", &ctx).await.unwrap();
    eprintln!("{text}");
    assert!(fields.iter().any(|f| f.label == "status" && f.value == "ready"));
}

#[tokio::test]
#[ignore = "needs the real claude CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_pong_permission_cancel_and_resume() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::new();
    let ctx = ProbeContext {
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: false,
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let info = Arc::new(adapter.probe(&ctx).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);

    // 用最便宜的模型、低思考等级
    let settings = SessionSettings {
        model: Some("claude-haiku-5-5".into()),
        thinking: Some("low".into()),
        mode: Some("default".into()),
        ..Default::default()
    };
    let mut h = spawn_real(&adapter, info.clone(), &cwd, settings.clone(), None).await;
    let started = match next_event(&mut h).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("expected Started, got {other:?}"),
    };
    eprintln!("session id = {}", started.agent_session_id);
    eprintln!("launch_args = {}", started.launch_args);

    // ---- turn 1：pong ----
    let evs = turn(&mut h, "reply with the single word pong", |_| panic!("unexpected permission request")).await;
    eprintln!("turn 1 events: {:?}", names(&evs));
    assert!(text_of(&evs).to_lowercase().contains("pong"), "got: {}", text_of(&evs));
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { turn_id, reason, usage }) => {
            let u = usage.as_ref().expect("usage");
            eprintln!("turn 1 done: id={turn_id} reason={reason:?} usage={u:?}");
            assert_eq!(*reason, TurnDoneReason::Completed);
            assert!(u.output_tokens.unwrap_or(0) > 0);
            assert!(u.cost_usd.unwrap_or(0.0) > 0.0);
            assert_eq!(u.model.as_deref(), Some("claude-haiku-5-5"));
        }
        _ => unreachable!(),
    }
    let thinking_seen = evs.iter().any(|e| matches!(e, AgentEvent::Event(Event::ThinkingDelta { .. })));
    eprintln!("thinking deltas in turn 1: {thinking_seen}");
    // 同一个进程里 CLI 回显我们给的 session id，不会多出第二个 Started
    assert!(!names(&evs).contains(&"STARTED".to_string()));

    // ---- 控制请求：改模式 / 改模型都能得到 CLI 的回执（不花 token）----
    // `set_settings` 收的是完整的目标设置（和 daemon 一样），从 spawn 时的设置出发改
    let mut d = settings.clone();
    d.mode = Some("acceptEdits".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now);
    d.mode = Some("default".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now);
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now);
    let mut bad = d.clone();
    bad.model = Some("definitely-not-a-model".into());
    let err = h.set_settings(&bad).await;
    eprintln!("set_model with a bogus id -> {err:?}");
    assert!(err.is_err());
    // 思考等级要重启
    d.thinking = Some("high".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.restart_required);
    // 改回来，免得后面 resume 用到 high
    d.thinking = Some("low".into());
    h.set_settings(&d).await.unwrap();

    // ---- turn 2：Write 需要权限 → can_use_tool → 允许 ----
    let mut perm = None;
    let evs = turn(
        &mut h,
        "Use the Write tool to create a file named hello.txt in the current directory containing exactly: hi",
        |p| {
            perm = Some(p.clone());
            PermissionDecision { option_id: Some("allow".into()), ..Default::default() }
        },
    )
    .await;
    eprintln!("turn 2 events: {:?}", names(&evs));
    let p = perm.expect("the real CLI asked for permission via can_use_tool");
    eprintln!("permission request: kind={:?} title={:?}", p.kind, p.title);
    assert_eq!(p.kind, PermissionKind::Tool);
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("Write", ToolCategory::FileEdit));
    assert!(
        evs.iter().any(|e| matches!(e, AgentEvent::Event(Event::FileChanged { kind: FileChangeKind::Created, .. }))),
        "{:?}",
        names(&evs)
    );
    let content = std::fs::read_to_string(cwd.join("hello.txt")).expect("hello.txt was written");
    assert_eq!(content.trim(), "hi");

    // ---- turn 3：中途取消 ----
    h.send(input(
        "Write a very long essay of at least 1500 words about the history of the bicycle, with many sections.",
    ))
    .await
    .unwrap();
    let mut first_text = false;
    let mut cancelled_at = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let cancel_evs = loop {
        let ev = tokio::time::timeout_at(deadline, h.events().recv()).await.expect("timed out").expect("closed");
        if matches!(ev, AgentEvent::Event(Event::TextDelta { .. })) && !first_text {
            first_text = true;
            cancelled_at = Some(std::time::Instant::now());
            h.cancel().await.unwrap();
        }
        let done = is_turn_done(&ev);
        if done {
            break ev;
        }
    };
    let took = cancelled_at.map(|t| t.elapsed());
    eprintln!("cancel -> {cancel_evs:?} after {took:?}");
    match cancel_evs {
        AgentEvent::Event(Event::TurnDone { reason, .. }) => assert_eq!(reason, TurnDoneReason::Cancelled),
        _ => unreachable!(),
    }
    assert!(!h.is_busy());

    // ---- 关闭，然后 resume ----
    let resume = h.resume_handle().unwrap();
    h.shutdown().await.unwrap();
    let evs = collect_until(&mut h, is_exited).await;
    eprintln!("shutdown events: {:?}", names(&evs));
    drop(h);

    let mut h2 = spawn_real(&adapter, info.clone(), &cwd, settings.clone(), Some(resume.clone())).await;
    match next_event(&mut h2).await {
        AgentEvent::Started { resume: r2 } => assert_eq!(r2.agent_session_id, resume.agent_session_id),
        other => panic!("{other:?}"),
    }
    let evs = turn(
        &mut h2,
        "What single word did I ask you to reply with in my very first message? Answer with just that word.",
        |_| panic!("unexpected permission request"),
    )
    .await;
    eprintln!("resumed turn: {:?} text={:?}", names(&evs), text_of(&evs));
    assert!(text_of(&evs).to_lowercase().contains("pong"), "resumed session lost its history: {}", text_of(&evs));
    assert!(!names(&evs).contains(&"STARTED".to_string()), "resume keeps the same session id");
    h2.shutdown().await.unwrap();
    let _ = collect_until(&mut h2, is_exited).await;

    // 清理 claude 在 ~/.claude/projects 下为这个临时目录留的会话
    if let Some(dir) = encoded_project_dir(&cwd) {
        if dir.is_dir() {
            eprintln!("cleaning up {}", dir.display());
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

// ---------------------------------------------------------------- 图片附件（ADR-35）

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// 自己编一张 `width × height` 的纯色 PNG（8 位 RGB，zlib 用不压缩的 stored block），不引入图片库。
fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut row = vec![0u8]; // 每行第一个字节是 filter 类型：0 = None
    for _ in 0..width {
        row.extend_from_slice(&rgb);
    }
    let raw = row.repeat(height as usize);
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(block) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none())); // BFINAL，BTYPE = 00（stored）
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 位、RGB、deflate、无滤波器预设、不隔行
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &z);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

/// 不花额度的自检：编出来的 PNG 结构正确（签名、每个 chunk 的 CRC、zlib 的 Adler-32、像素都是指定的颜色）。
#[test]
fn solid_png_encoder_produces_a_well_formed_png() {
    let png = solid_png(32, 32, [255, 0, 0]);
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut pos = 8;
    let mut kinds = Vec::new();
    let mut idat = Vec::new();
    while pos < png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = png[pos + 4..pos + 8].to_vec();
        let data = &png[pos + 8..pos + 8 + len];
        let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
        assert_eq!(crc, crc32(&png[pos + 4..pos + 8 + len]), "bad CRC in {:?}", String::from_utf8_lossy(&kind));
        if kind == b"IHDR" {
            assert_eq!(u32::from_be_bytes(data[0..4].try_into().unwrap()), 32);
            assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 32);
            assert_eq!(&data[8..], &[8, 2, 0, 0, 0]);
        }
        if kind == b"IDAT" {
            idat.extend_from_slice(data);
        }
        kinds.push(String::from_utf8(kind).unwrap());
        pos += 12 + len;
    }
    assert_eq!(pos, png.len());
    assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"]);

    // 手工拆 zlib：头 + stored block + Adler-32
    assert_eq!(&idat[..2], &[0x78, 0x01]);
    let mut raw = Vec::new();
    let mut p = 2;
    loop {
        let last = idat[p] & 1 == 1;
        assert_eq!(idat[p] >> 1, 0, "stored block expected");
        let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]);
        assert_eq!(u16::from_le_bytes([idat[p + 3], idat[p + 4]]), !len);
        raw.extend_from_slice(&idat[p + 5..p + 5 + len as usize]);
        p += 5 + len as usize;
        if last {
            break;
        }
    }
    assert_eq!(u32::from_be_bytes(idat[p..p + 4].try_into().unwrap()), adler32(&raw));
    assert_eq!(p + 4, idat.len());
    assert_eq!(raw.len(), (32 * 3 + 1) * 32);
    for row in raw.chunks(32 * 3 + 1) {
        assert_eq!(row[0], 0);
        assert!(row[1..].chunks(3).all(|px| px == [255, 0, 0]));
    }
    // 大图要拆成多个 stored block
    assert!(solid_png(200, 200, [0, 0, 255]).len() > 65535);
}

#[tokio::test]
#[ignore = "needs the real claude CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_image_attachment_is_understood() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    // 图片文件放在项目目录之外（像 daemon 的 blobs 目录），adapter 只管读它
    let files = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let png = solid_png(32, 32, [255, 0, 0]);
    let image_path = files.path().join("red.png");
    std::fs::write(&image_path, &png).unwrap();

    let adapter = ClaudeCodeAdapter::new();
    let ctx = ProbeContext {
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: false,
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let info = Arc::new(adapter.probe(&ctx).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert!(info.capabilities.attachments.contains(&"image/png".to_string()));

    let settings = SessionSettings {
        model: Some("claude-haiku-5-5".into()),
        thinking: Some("low".into()),
        mode: Some("default".into()),
        ..Default::default()
    };
    let mut h = spawn_real(&adapter, info.clone(), &cwd, settings, None).await;
    let _ = next_event(&mut h).await; // Started

    let user_input = UserInput {
        content: "What color is this image? Answer with one English word.".into(),
        attachments: vec![AttachmentRef {
            blob_id: "0123456789abcdef0123456789abcdef".into(),
            name: "red.png".into(),
            mime: "image/png".into(),
            bytes: png.len() as u64,
            width: Some(32),
            height: Some(32),
        }],
        attachment_paths: vec![image_path],
        ..input("")
    };
    let evs = turn_with(&mut h, user_input, |_| panic!("unexpected permission request")).await;
    eprintln!("image turn events: {:?} text={:?}", names(&evs), text_of(&evs));
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { reason, usage, .. }) => {
            eprintln!("image turn done: reason={reason:?} usage={usage:?}");
            assert_eq!(*reason, TurnDoneReason::Completed, "{:?}", names(&evs));
        }
        _ => unreachable!(),
    }
    assert!(text_of(&evs).to_lowercase().contains("red"), "the model did not see the image: {}", text_of(&evs));
    h.shutdown().await.unwrap();
    let _ = collect_until(&mut h, is_exited).await;

    // 清理 claude 在 ~/.claude/projects 下为这个临时目录留的会话
    if let Some(dir) = encoded_project_dir(&cwd) {
        if dir.is_dir() {
            eprintln!("cleaning up {}", dir.display());
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

// ---------------------------------------------------------------- workflow / MCP elicitation

/// 一个事件序列里最后一次出现的 `Task` 快照（按 id 取最新）。
fn last_task<'a>(evs: &'a [AgentEvent], id: &str) -> Option<&'a Event> {
    evs.iter().rev().find_map(|e| match e {
        AgentEvent::Event(ev @ Event::Task { id: i, .. }) if i == id => Some(ev),
        _ => None,
    })
}

fn tool_result_texts(evs: &[AgentEvent]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match e {
            AgentEvent::Event(Event::ToolResult { output, .. }) => Some(output.to_string()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs the real claude CLI and ANYCON_REAL_AGENT_TESTS=1"]
async fn real_workflow_and_mcp_elicitation() {
    if !enabled() {
        eprintln!("skipped: set ANYCON_REAL_AGENT_TESTS=1");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    // 项目里的 `.mcp.json` 注册一个只用标准库的 MCP server（它的工具会反过来向用户要输入）；`-p` 下不需要额外批准
    let server = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/bin/elicit-mcp-server");
    std::fs::write(
        cwd.join(".mcp.json"),
        serde_json::json!({"mcpServers": {"example-elicit": {"command": "python3", "args": [server]}}}).to_string(),
    )
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::new();
    let ctx = ProbeContext {
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(60),
        cheap: false,
        previous: None,
        scratch_dir: scratch.path().to_path_buf(),
    };
    let info = Arc::new(adapter.probe(&ctx).await);
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    let settings = SessionSettings {
        model: Some("claude-haiku-5-5".into()),
        thinking: Some("low".into()),
        mode: Some("default".into()),
        ..Default::default()
    };
    let mut h = spawn_real(&adapter, info.clone(), &cwd, settings, None).await;
    let _ = next_event(&mut h).await; // Started

    // ---- turn 1：Workflow。走 can_use_tool，在后台跑；CLI 在它结束后自己再开一轮，事件一直送到那一轮结束 ----
    let mut titles = Vec::new();
    let mut evs = turn(
        &mut h,
        "Use a workflow to do this: run exactly two trivial agents in parallel. The first agent just replies with the single word alpha, \
         the second agent just replies with the single word beta. Then tell me both words. Keep everything minimal.",
        |p| {
            titles.push((p.kind, p.title.clone()));
            PermissionDecision { option_id: Some("allow".into()), ..Default::default() }
        },
    )
    .await;
    eprintln!("workflow turn: permissions={titles:?} events={:?}", names(&evs));
    assert!(titles.iter().any(|(k, t)| *k == PermissionKind::Tool && t.starts_with("Run workflow:")), "{titles:?}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let finished = evs.iter().any(|e| {
            matches!(e, AgentEvent::Event(Event::Task { kind: TaskKind::Workflow, status, .. }) if status.is_terminal())
        });
        // CLI 自己开的那一轮要收完（含它的 TurnDone），否则下一个 turn 会把残留的 TurnDone 当成自己的结束
        let own_turn_done = evs
            .iter()
            .position(|e| matches!(e, AgentEvent::TurnStarted))
            .is_some_and(|i| evs[i..].iter().any(is_turn_done));
        if finished && own_turn_done {
            break;
        }
        let ev = tokio::time::timeout_at(deadline, h.events().recv())
            .await
            .expect("the workflow did not finish")
            .expect("channel closed");
        evs.push(ev);
    }
    let task_id = evs
        .iter()
        .find_map(|e| match e {
            AgentEvent::Event(Event::Task { id, kind: TaskKind::Workflow, .. }) => Some(id.clone()),
            _ => None,
        })
        .expect("a workflow task snapshot");
    match last_task(&evs, &task_id).unwrap() {
        Event::Task { status, title, agents, phases, usage, tool_id, .. } => {
            eprintln!("final workflow snapshot: title={title:?} status={status:?} phases={phases:?} usage={usage:?} agents={agents:?}");
            assert_eq!(*status, TaskStatus::Completed);
            assert!(!title.is_empty() && tool_id.is_some());
            assert_eq!(agents.len(), 2, "{agents:?}");
            assert!(agents.iter().all(|a| a.state == TaskAgentState::Done && a.tokens.unwrap_or(0) > 0), "{agents:?}");
            assert!(usage.and_then(|u| u.total_tokens).unwrap_or(0) > 0);
        }
        _ => unreachable!(),
    }
    // 两轮（用户的 turn + CLI 自己开的那一轮）都带着上下文用量
    let dones: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Event(Event::TurnDone { usage: Some(u), .. }) => Some(u.clone()),
            _ => None,
        })
        .collect();
    eprintln!("workflow turns' usage: {dones:?}");
    assert!(dones.iter().all(|u| u.context_window.unwrap_or(0) > 0 && u.context_tokens.unwrap_or(0) > 0), "{dones:?}");

    // ---- turn 2：表单模式的 elicitation，提交 ----
    let mut asked = Vec::new();
    let evs = turn(
        &mut h,
        "Call the ask_details tool of the example-elicit MCP server, then tell me exactly what result it returned. Nothing else.",
        |p| {
            asked.push(p.clone());
            if p.kind != PermissionKind::Question {
                return PermissionDecision { option_id: Some("allow".into()), ..Default::default() };
            }
            let mut answers = serde_json::Map::new();
            for (k, v) in [("name", "Ada"), ("age", "36"), ("subscribe", "true"), ("color", "green")] {
                answers.insert(k.into(), serde_json::json!(v));
            }
            PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), ..Default::default() }
        },
    )
    .await;
    eprintln!(
        "form turn: events={:?} asked={:?}",
        names(&evs),
        asked.iter().map(|p| (p.kind, &p.title)).collect::<Vec<_>>()
    );
    let form = asked.iter().find(|p| p.kind == PermissionKind::Question).expect("an elicitation form");
    assert_eq!(form.title, "example-elicit: Please enter your details");
    assert_eq!(form.detail.questions.len(), 4);
    // MCP server 把它收到的答复放进了工具结果：类型是转换过的（age 是数字、subscribe 是布尔）
    let results = tool_result_texts(&evs).join("\n");
    assert!(results.contains(r#"\"action\": \"accept\""#) && results.contains(r#"\"age\": 36"#), "{results}");
    assert!(results.contains(r#"\"subscribe\": true"#) && results.contains(r#"\"color\": \"green\""#), "{results}");

    // ---- turn 3：URL 模式，确认 ----
    let mut asked = Vec::new();
    let evs = turn(
        &mut h,
        "Call the ask_signin tool of the example-elicit MCP server, then tell me exactly what result it returned. Nothing else.",
        |p| {
            asked.push(p.clone());
            let id = if p.kind == PermissionKind::Other { "accept" } else { "allow" };
            PermissionDecision { option_id: Some(id.into()), ..Default::default() }
        },
    )
    .await;
    eprintln!("url turn: events={:?}", names(&evs));
    let link = asked.iter().find(|p| p.kind == PermissionKind::Other).expect("a URL elicitation");
    assert_eq!(link.detail.url.as_deref(), Some("https://example.com/signin?state=abc123"));
    assert!(tool_result_texts(&evs).join("\n").contains(r#"\"action\": \"accept\""#));

    // ---- turn 4：表单模式，拒绝 ----
    let evs = turn(
        &mut h,
        "Call the ask_details tool of the example-elicit MCP server, then tell me exactly what result it returned. Nothing else.",
        |p| {
            let id = if p.kind == PermissionKind::Question { "decline" } else { "allow" };
            PermissionDecision { option_id: Some(id.into()), ..Default::default() }
        },
    )
    .await;
    eprintln!("decline turn: events={:?}", names(&evs));
    assert!(tool_result_texts(&evs).join("\n").contains(r#"\"action\": \"decline\""#));

    h.shutdown().await.unwrap();
    let _ = collect_until(&mut h, is_exited).await;
    if let Some(dir) = encoded_project_dir(&cwd) {
        if dir.is_dir() {
            eprintln!("cleaning up {}", dir.display());
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
