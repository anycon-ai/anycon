//! 探测（docs/spec/daemon.md § 5.3 / § 5.4 的 ACP 一列）与诊断（§ 5.5）。
//!
//! 步骤：
//! 1. **解析命令**：`config.agents.acp.<id>.command`，没有就用注册表（`registry.rs`）里的。命令的第一个词在 daemon 环境的 `PATH`
//!    （再加已知安装目录）里找得到才算安装；**不跑 `npx` / `uvx`**（那会在用户机器上下载并执行东西）。配置里写绝对路径的直接用。
//! 2. **版本**：`<程序> --version`，尽力而为——ACP agent 不一定支持它：能起来就算装了，输出里解析得出 semver 才给版本。
//!    完整探测里 `initialize` 返回的 `agentInfo.version` 优先。
//! 3. **完整探测**（首次、`refresh`、命令 / 版本变了、配置变了）：在 `ctx.scratch_dir` 下拉起 agent，`initialize`，再 `session/new`
//!    取模型 / 思考等级 / 模式 / 其余选项（`options.rs`），然后 `session/close`（有的话）并关掉进程。`session/new` 报认证错误
//!    （`-32000`）= `not_authenticated`，hint 列出 `authMethods` 的名字；`type = terminal` 的登录方式给出要在机器上跑的命令。
//!    **不调 `authenticate`**：登录方式可能要打开浏览器或读终端输入，daemon 做不了，用户在机器上用 agent 自己的 CLI 登录一次。
//! 4. **cheap 探测**（`probe_interval` 的周期探测）只跑第 1、2 步：路径和 `--version` 的输出都和上一次完整探测一样、上一次是 `ready`
//!    就沿用上一次的结果，不拉起 agent；不一样、或者上一次不是 `ready`（登录状态 cheap 探测看不出来），做完整探测。
//!    「上一次」存在 adapter 实例里（`ProbeMemo`）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::{AdapterError, ProbeContext};
use anycon_protocol::{AgentCapabilities, AgentInfo, AgentIssue, AgentStatus, DiagnosticField};
use serde_json::{json, Value};

use crate::common::bin::collect_candidates;
use crate::common::{
    effective_env, field, now_ms, parse_command, parse_semver, run_capture, shell_type_a, version_timeout, Budget,
    CommandOrigin, ShutdownTimings, IMAGE_MIMES,
};

use super::conn::{auth_message, map_failure, LaunchSpec, Timings};
use super::options::{self, ConfigState};
use super::registry::{effective_command, registry_entry};
use super::rpc::{RpcFailure, AUTH_REQUIRED};
use super::wire::{AgentCaps, InitializeResult};

pub const DRIVER: &str = "acp";

/// 完整探测里各步的上限（再受总预算约束）。
const HANDSHAKE_CAP: Duration = Duration::from_secs(20);
const SESSION_CAP: Duration = Duration::from_secs(40);
const CLOSE_CAP: Duration = Duration::from_secs(3);

/// 上一次完整探测时看到的路径和 `--version` 输出（cheap 探测据此判断有没有变）。
#[derive(Default)]
pub struct ProbeMemo(Mutex<Option<(String, String)>>);

impl ProbeMemo {
    fn matches(&self, path: &str, version: &str) -> bool {
        self.0.lock().expect("probe memo lock").as_ref().is_some_and(|(p, v)| p == path && v == version)
    }

    fn set(&self, path: &str, version: &str) {
        *self.0.lock().expect("probe memo lock") = Some((path.to_string(), version.to_string()));
    }
}

pub fn capabilities(caps: &AgentCaps) -> AgentCapabilities {
    AgentCapabilities {
        // `session/resume` 或 `session/load` 有一个就能在进程退出后接上原来的会话
        resume: caps.can_restore(),
        interrupt: true,
        // ACP v1 没有「往进行中的 turn 里插话」
        interject: false,
        attachments: if caps.prompt_capabilities.image {
            IMAGE_MIMES.iter().map(|m| m.to_string()).collect()
        } else {
            Vec::new()
        },
        thinking_stream: true,
        usage: true,
        structured_output: false,
        // 进程按 agent 共享，没有会话级的环境
        env: false,
        rewind: false,
    }
}

/// 一个候选的探活结果，诊断用。
pub struct CandidateLine {
    pub path: PathBuf,
    pub origin: CommandOrigin,
    /// `ok: …` / `failed: …` / `not probed`。
    pub outcome: String,
}

pub struct ProbeReport {
    pub info: AgentInfo,
    /// 生效的命令（配置或注册表），以及它从哪来。
    pub command: Option<(String, &'static str)>,
    pub candidates: Vec<CandidateLine>,
    pub version_output: Option<String>,
    pub init: Option<InitializeResult>,
    pub notes: Vec<String>,
}

fn base_info(ctx: &ProbeContext, id: &str, label: &str) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        label: label.to_string(),
        driver: DRIVER.to_string(),
        enabled: ctx.config.enabled,
        status: AgentStatus::Ready,
        version: None,
        path: None,
        issue: None,
        models: vec![],
        allow_custom_model: false,
        modes: vec![],
        options: vec![],
        capabilities: AgentCapabilities::default(),
        probed_at_ms: now_ms(),
    }
}

fn fail(info: &mut AgentInfo, status: AgentStatus, code: &str, message: String, hint: Option<String>) {
    info.status = status;
    info.issue = Some(AgentIssue { code: code.to_string(), message, hint });
}

/// 没装时的提示：注册表里的安装方式，自己加的 agent 让用户检查命令。
fn install_hint(id: &str) -> String {
    match registry_entry(id) {
        Some(e) => e.install_hint.to_string(),
        None => format!("install the agent, or fix `agents.acp.{id}.command`"),
    }
}

/// 登录提示。`terminal` 类型的登录方式要在机器上跑一次命令（配置的命令 + 它的 `args`）；其余的让用户用 agent 自己的 CLI 登录。
fn login_hint(command: &str, init: &InitializeResult) -> String {
    if let Some(m) = init.auth_methods.iter().find(|m| m.is_terminal()) {
        let env: String = {
            let mut pairs: Vec<String> = m.env.iter().map(|(k, v)| format!("{k}={}", shell_words::quote(v))).collect();
            pairs.sort();
            if pairs.is_empty() {
                String::new()
            } else {
                format!("{} ", pairs.join(" "))
            }
        };
        let args: Vec<String> = m.args.iter().map(|a| shell_words::quote(a).into_owned()).collect();
        format!(
            "run `{env}{command}{}{}` in a terminal on this machine to log in",
            if args.is_empty() { "" } else { " " },
            args.join(" ")
        )
    } else {
        "log in once on this machine with the agent's own CLI, then refresh".to_string()
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// 完整探测。`probe_all`：`--version` 把所有候选都跑一遍（`doctor` 用）。
pub async fn run_probe(ctx: &ProbeContext, id: &str, label: &str, probe_all: bool, memo: &ProbeMemo) -> ProbeReport {
    let budget = Budget::new(ctx.deadline);
    let display = registry_entry(id).map(|e| e.label).unwrap_or(label).to_string();
    let mut report = ProbeReport {
        info: base_info(ctx, id, &display),
        command: None,
        candidates: Vec::new(),
        version_output: None,
        init: None,
        notes: Vec::new(),
    };

    if !ctx.config.enabled {
        fail(
            &mut report.info,
            AgentStatus::Disabled,
            "disabled",
            format!("{display} is disabled in the config"),
            Some(format!("set agents.acp.{id}.enabled = true")),
        );
        return report;
    }
    let env = effective_env(ctx);

    // 1. 解析命令
    let Some(command) = effective_command(id, &ctx.config) else {
        fail(
            &mut report.info,
            AgentStatus::Error,
            "no_command",
            format!("no command is configured for the ACP agent {id}"),
            Some(format!("anycon config set agents.acp.{id}.command \"<your-agent> acp\"")),
        );
        return report;
    };
    let source =
        if ctx.config.command.as_deref().is_some_and(|c| !c.trim().is_empty()) { "config" } else { "registry" };
    report.command = Some((command.clone(), source));
    let spec = match parse_command(&command) {
        Ok(s) => s,
        Err(e) => {
            fail(
                &mut report.info,
                AgentStatus::Error,
                "invalid_command",
                format!("invalid command in config: {e}"),
                Some(format!("fix agents.acp.{id}.command")),
            );
            return report;
        }
    };
    let candidates: Vec<(PathBuf, CommandOrigin)> = if spec.is_path() {
        // 相对路径会按子进程的 cwd 解析，可能落到项目目录里；配置里的路径必须是绝对的
        if !Path::new(&spec.program).is_absolute() {
            fail(
                &mut report.info,
                AgentStatus::Error,
                "invalid_command",
                format!("invalid command in config: the path must be absolute (got {})", spec.program),
                Some(format!("fix agents.acp.{id}.command")),
            );
            return report;
        }
        let p = PathBuf::from(&spec.program);
        if p.is_file() {
            vec![(p, CommandOrigin::Config)]
        } else {
            Vec::new()
        }
    } else {
        collect_candidates(&spec.program, &env, &ctx.scratch_dir)
    };
    if candidates.is_empty() {
        fail(
            &mut report.info,
            AgentStatus::NotInstalled,
            "binary_not_found",
            format!("the `{}` command was not found", spec.program),
            Some(install_hint(id)),
        );
        return report;
    }

    // 2. `--version`（尽力而为）：第一个能起来的候选
    let vt = version_timeout(&env);
    let mut chosen: Option<(PathBuf, String)> = None;
    let mut start_failures: Vec<String> = Vec::new();
    for (path, origin) in &candidates {
        if chosen.is_some() && !probe_all {
            report.candidates.push(CandidateLine { path: path.clone(), origin: *origin, outcome: "not probed".into() });
            continue;
        }
        let out = run_capture(path, &["--version".to_string()], &env, &ctx.scratch_dir, budget.cap(vt)).await;
        if out.spawn_error.is_some() {
            start_failures.push(format!("{}: {}", path.display(), out.failure_reason()));
            report.candidates.push(CandidateLine {
                path: path.clone(),
                origin: *origin,
                outcome: format!("failed: {}", out.failure_reason()),
            });
            continue;
        }
        let text = if out.stdout.trim().is_empty() { out.stderr.trim() } else { out.stdout.trim() };
        let version_line = if out.success() { first_line(text) } else { String::new() };
        report.candidates.push(CandidateLine {
            path: path.clone(),
            origin: *origin,
            outcome: if out.success() {
                format!("ok: {version_line}")
            } else {
                // 起得来、`--version` 不认（很多 ACP agent 就是这样）：照样算装了
                format!("started, but --version failed ({})", out.failure_reason())
            },
        });
        if chosen.is_none() {
            chosen = Some((path.clone(), version_line));
        }
    }
    let Some((path, version_line)) = chosen else {
        fail(
            &mut report.info,
            AgentStatus::Error,
            "binary_failed",
            format!("found {} but it cannot be started: {}", spec.program, start_failures.join("; ")),
            Some(format!("check that `{}` works in a terminal", spec.program)),
        );
        return report;
    };
    report.info.path = Some(path.to_string_lossy().into_owned());
    report.info.version = parse_semver(&version_line).map(|v| v.to_string());
    report.version_output = Some(version_line.clone()).filter(|v| !v.is_empty());
    let path_str = path.to_string_lossy().into_owned();

    // cheap：路径和 `--version` 都没变、上一次是 ready，就沿用上一次的结果
    if ctx.cheap {
        if let Some(prev) = ctx.previous.as_ref().filter(|p| p.status == AgentStatus::Ready) {
            if memo.matches(&path_str, &version_line) {
                let mut info = prev.clone();
                info.probed_at_ms = now_ms();
                info.enabled = true;
                report.info = info;
                report
                    .notes
                    .push("cheap probe: the binary and its version are unchanged, reusing the last result".into());
                return report;
            }
        }
        report
            .notes
            .push("cheap probe found a change (or the last result was not ready): running the full probe".into());
    }

    // 3. 完整探测：拉起 agent，`initialize` + `session/new`
    let launch = LaunchSpec { program: path.clone(), args: spec.args.clone(), env: Arc::new(env.clone()) };
    let handshake = budget.cap(HANDSHAKE_CAP);
    if handshake.is_zero() {
        fail(
            &mut report.info,
            AgentStatus::Error,
            "probe_timeout",
            "probe timed out before the agent was started; unfinished steps: handshake, models".to_string(),
            None,
        );
        return report;
    }
    let probe_timings = Timings {
        handshake,
        session: budget.cap(SESSION_CAP),
        request: budget.cap(SESSION_CAP),
        close: CLOSE_CAP,
        shutdown: ShutdownTimings { after_stdin_close: Duration::from_millis(300), grace: Duration::from_secs(2) },
    };
    let conn = match super::conn::Connection::start(&launch, &display, launch.fingerprint(), &probe_timings).await {
        Ok(c) => c,
        Err(AdapterError::UnsupportedVersion(m)) => {
            fail(&mut report.info, AgentStatus::UnsupportedVersion, "protocol_version", m, None);
            return report;
        }
        Err(e) => {
            fail(
                &mut report.info,
                AgentStatus::Error,
                "handshake_failed",
                e.to_string(),
                Some(format!(
                    "check that `{command}` starts an ACP server on stdio (it should wait for messages on stdin)"
                )),
            );
            return report;
        }
    };
    report.init = Some(conn.init.clone());
    let caps = conn.caps().clone();

    let new = conn
        .request(
            "session/new",
            json!({ "cwd": ctx.scratch_dir.to_string_lossy(), "mcpServers": [] }),
            budget.cap(SESSION_CAP),
        )
        .await;
    let mut state = ConfigState::default();
    match new {
        Ok(v) => {
            state = ConfigState::from_response(&v);
            report.notes.push(format!(
                "session/new: {} configOptions, {} legacy modes",
                state.options.len(),
                state.modes.len()
            ));
            // 探测用的会话不留
            if let (true, Some(sid)) = (caps.can_close(), v.get("sessionId").and_then(Value::as_str)) {
                let _ = conn.request("session/close", json!({ "sessionId": sid }), budget.cap(CLOSE_CAP)).await;
            }
        }
        Err(RpcFailure::Rpc(e)) if e.code == AUTH_REQUIRED => {
            fail(
                &mut report.info,
                AgentStatus::NotAuthenticated,
                "login_required",
                auth_message(&display, &conn.init, &e),
                Some(login_hint(&command, &conn.init)),
            );
            // 登录之前拿不到模型列表：沿用上一次的
            if let Some(prev) = &ctx.previous {
                report.info.models = prev.models.clone();
                report.info.modes = prev.modes.clone();
                report.info.options = prev.options.clone();
            }
        }
        Err(e) => {
            let msg = match &e {
                RpcFailure::Closed => {
                    let tail = crate::common::truncate_bytes(conn.stderr_tail().trim(), 300);
                    if tail.is_empty() {
                        "the agent exited during session/new".to_string()
                    } else {
                        format!("the agent exited during session/new: {tail}")
                    }
                }
                other => map_failure("session/new", other.clone()).to_string(),
            };
            fail(
                &mut report.info,
                AgentStatus::Error,
                "session_new_failed",
                msg,
                Some(format!("run `{command}` in a terminal to see why it fails")),
            );
        }
    }
    conn.shutdown(probe_timings.shutdown).await;

    // 身份：`agentInfo.version` 优先于 `--version`；自己加的 agent 的名字用 `agentInfo`
    if let Some(ai) = &report.init.as_ref().and_then(|i| i.agent_info.clone()) {
        if !ai.version.trim().is_empty() {
            report.info.version = Some(ai.version.trim().to_string());
        }
        if registry_entry(id).is_none() {
            if let Some(n) = ai.display() {
                report.info.label = n.to_string();
            }
        }
    }
    report.info.capabilities = capabilities(&caps);
    if report.info.status == AgentStatus::Ready {
        report.info.models = options::models(&state);
        report.info.modes = options::modes(&state);
        report.info.options = options::options(&state);
        memo.set(&path_str, &version_line);
    }
    report
}

/// `anycon doctor` / `agent.diagnose` 的字段。
pub async fn diagnose_fields(ctx: &ProbeContext, id: &str, label: &str, memo: &ProbeMemo) -> Vec<DiagnosticField> {
    let mut full = ctx.clone();
    full.cheap = false;
    let report = run_probe(&full, id, label, true, memo).await;
    let env = effective_env(ctx);
    let info = &report.info;
    let mut f = Vec::new();
    match &report.command {
        Some((c, source)) => {
            f.push(field("command", c.clone()));
            f.push(field("command source", *source));
        }
        None => f.push(field("command", "(none configured)")),
    }
    f.push(field("daemon PATH", env.get("PATH").cloned().unwrap_or_default()));
    f.push(field("daemon shell", env.get("SHELL").cloned().unwrap_or_else(|| "(unknown)".into())));
    let first_word = report
        .command
        .as_ref()
        .and_then(|(c, _)| parse_command(c).ok())
        .map(|s| s.program)
        .unwrap_or_else(|| id.to_string());
    f.push(field("which -a / candidates", {
        let lines: Vec<String> = report
            .candidates
            .iter()
            .map(|c| format!("{} [{}] {}", c.path.display(), c.origin.as_str(), c.outcome))
            .collect();
        if lines.is_empty() {
            "(none)".to_string()
        } else {
            lines.join("\n")
        }
    }));
    f.push(field("login shell type -a", shell_type_a(&env, &first_word, &ctx.scratch_dir).await));
    f.push(field("resolved path", info.path.clone().unwrap_or_else(|| "(not found)".into())));
    f.push(field("--version", report.version_output.clone().unwrap_or_else(|| "(none)".into())));
    if let Some(init) = &report.init {
        let agent = init
            .agent_info
            .as_ref()
            .map(|a| format!("{} {}", a.display().unwrap_or("(unnamed)"), a.version).trim().to_string())
            .unwrap_or_else(|| "(not reported)".into());
        f.push(field("agent (initialize)", agent));
        f.push(field(
            "ACP protocol version",
            init.protocol_version.map(|v| v.to_string()).unwrap_or_else(|| "(not reported)".into()),
        ));
        let c = &init.agent_capabilities;
        f.push(field(
            "agent capabilities",
            format!(
                "loadSession={} resume={} close={} image={} audio={} embeddedContext={}",
                c.load_session,
                c.can_resume(),
                c.can_close(),
                c.prompt_capabilities.image,
                c.prompt_capabilities.audio,
                c.prompt_capabilities.embedded_context
            ),
        ));
        f.push(field(
            "login methods",
            if init.auth_methods.is_empty() {
                "(none)".to_string()
            } else {
                init.auth_methods
                    .iter()
                    .map(|m| {
                        format!(
                            "{} ({}){}",
                            m.name,
                            if m.is_terminal() { "terminal" } else { "agent" },
                            m.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ));
    }
    f.push(field("models", info.models.len().to_string()));
    f.push(field("modes", info.modes.len().to_string()));
    f.push(field("options", info.options.len().to_string()));
    f.push(field("resumable", info.capabilities.resume.to_string()));
    f.push(field(
        "attachments",
        if info.capabilities.attachments.is_empty() {
            "none".to_string()
        } else {
            info.capabilities.attachments.join(", ")
        },
    ));
    f.push(field("probe notes", if report.notes.is_empty() { "(none)".to_string() } else { report.notes.join("\n") }));
    f.push(field("status", format!("{:?}", info.status).to_lowercase()));
    if let Some(i) = &info.issue {
        f.push(field("issue", format!("{}: {}", i.code, i.message)));
        if let Some(h) = &i.hint {
            f.push(field("hint", h.clone()));
        }
    }
    f
}
