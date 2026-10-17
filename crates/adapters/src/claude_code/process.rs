//! 拉起 `claude` 的参数与环境（docs/spec/daemon.md § 6.3 / § 7.4）。
//!
//! 固定部分：`claude -p --output-format stream-json --input-format stream-json --verbose --include-partial-messages
//! --permission-prompt-tool stdio`。**`--permission-prompt-tool stdio` 必须带**：不带的话 CLI（2.1.x）在需要确认时直接
//! 自动拒绝（`permission_denied`），根本不会发 `can_use_tool` 控制请求。
//!
//! 可变部分来自会话设置，且总是显式传：模型 / 思考 / 权限模式缺省时取配置里的默认值，再取 `AgentInfo` 里标 default 的，
//! 这样 UI 上显示的「当前值」就是 CLI 实际用的值，不会悄悄继承用户 `~/.claude/settings.json` 里更宽松的设置。
//!
//! **argv 安全**：每个带值的选项都写成单个 argv 元素 `--flag=value`，所以无论值长什么样（以 `-` 开头、含空格…）
//! 都不可能被 CLI 解析成另一个选项。值本身也在 [`LaunchOptions::resolve`] / [`validate_settings_values`] 里校验：
//! 模型 / 思考 / 会话 id 限字符集和长度，`--append-system-prompt` 限长且不含控制字符，预算必须是有限正数。
//! 校验同时挂在 `Adapter::validate_settings` 上，daemon 在 `session.create` / `set_settings` 时就能拒绝。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anycon_agent_api::{AdapterConfig, AdapterError, Result, ResumeHandle};
use anycon_protocol::{AgentInfo, SessionSettings};
use serde_json::{json, Value};

use super::models::LEVELS_5X;
use super::probe::{self, DEFAULT_COMMAND};

pub const MODE_BYPASS: &str = "bypassPermissions";
/// 关思考：不传 `--effort`，改用环境变量。
pub const THINKING_OFF: &str = "off";

/// 每个 `claude` 进程固定带的参数（带值的选项一律 `--flag=value`）。
pub fn base_args() -> Vec<String> {
    [
        "-p",
        "--output-format=stream-json",
        "--input-format=stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool=stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 模型 id 最长。放得下 Bedrock 的 inference profile ARN。
pub const MAX_MODEL_LEN: usize = 200;
/// 思考等级（`--effort`）最长。
pub const MAX_EFFORT_LEN: usize = 32;
/// 会话 id（`--session-id` / `--resume`）最长。
pub const MAX_SESSION_ID_LEN: usize = 128;
/// `--append-system-prompt` 的值最大字节数（单个 argv 元素的上限是 128 KiB，留足余量）。
pub const MAX_SYSTEM_PROMPT_APPEND_BYTES: usize = 32 * 1024;

/// 首字符必须是字母或数字（所以不会以 `-` 开头），其余字符是字母 / 数字 / `extra` 里的符号。
fn token_ok(s: &str, max_len: usize, extra: &str) -> bool {
    !s.is_empty()
        && s.len() <= max_len
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

fn bad_value(field: &str, what: &str) -> AdapterError {
    AdapterError::InvalidSettings { field: field.to_string(), reason: what.to_string() }
}

/// 模型 id：字母数字开头，之后允许 `. _ : / @ [ ] + -`（别名、`name[1m]`、Bedrock ARN、Vertex `name@date` 都在内）。
pub fn check_model(model: &str) -> Result<()> {
    if token_ok(model, MAX_MODEL_LEN, "._:/@[]+-") {
        Ok(())
    } else {
        Err(bad_value(
            "model",
            &format!(
                "model ids must start with a letter or digit, use only letters, digits and . _ : / @ [ ] + -, and be at most {MAX_MODEL_LEN} characters"
            ),
        ))
    }
}

/// 思考等级：字母数字开头，之后允许 `_ -`，最长 [`MAX_EFFORT_LEN`]。
pub fn check_effort(thinking: &str) -> Result<()> {
    if token_ok(thinking, MAX_EFFORT_LEN, "_-") {
        Ok(())
    } else {
        Err(bad_value("thinking", "invalid thinking level"))
    }
}

/// 会话 id：字母数字开头，之后允许 `_ -`，最长 [`MAX_SESSION_ID_LEN`]。
pub fn check_session_id(field: &str, id: &str) -> Result<()> {
    if token_ok(id, MAX_SESSION_ID_LEN, "_-") {
        Ok(())
    } else {
        Err(bad_value(field, "invalid agent session id"))
    }
}

/// `system_prompt_append`：最多 [`MAX_SYSTEM_PROMPT_APPEND_BYTES`] 字节，不含 NUL 等控制字符（换行 / 制表符可以）。
pub fn check_system_prompt_append(text: &str) -> Result<()> {
    if text.len() > MAX_SYSTEM_PROMPT_APPEND_BYTES {
        return Err(bad_value(
            "system_prompt_append",
            &format!("too long ({} bytes; the limit is {MAX_SYSTEM_PROMPT_APPEND_BYTES})", text.len()),
        ));
    }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(bad_value("system_prompt_append", "control characters are not allowed"));
    }
    Ok(())
}

/// 预算：有限的正数（CLI 本身也要求 > 0）。
pub fn check_budget(usd: f64) -> Result<()> {
    if usd.is_finite() && usd > 0.0 {
        Ok(())
    } else {
        Err(bad_value("max_budget_usd", "must be a finite number greater than 0"))
    }
}

/// 只看设置本身的值检查（不需要 `AgentInfo`）。`Adapter::validate_settings` 和 [`LaunchOptions::resolve`] 都会调，
/// 所以 daemon 在 `session.create` / `session.set_settings` 时就能以 `invalid_settings` 拒绝。
pub fn validate_settings_values(settings: &SessionSettings) -> Result<()> {
    if let Some(m) = &settings.model {
        check_model(m)?;
    }
    if let Some(t) = &settings.thinking {
        check_effort(t)?;
    }
    if let Some(s) = &settings.system_prompt_append {
        check_system_prompt_append(s)?;
    }
    if let Some(x) = settings.max_budget_usd {
        check_budget(x)?;
    }
    Ok(())
}

/// 一个 `claude` 进程启动时用的设置（也是 `ResumeHandle.launch_args` 的内容）。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchOptions {
    pub model: Option<String>,
    /// `off` 或 `--effort` 的取值。
    pub thinking: Option<String>,
    pub mode: String,
    pub system_prompt_append: Option<String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub fast_mode: bool,
    /// 会话设置里的 `env` 覆盖（进程环境本身由 daemon 合并好放在 `SpawnRequest.env`）。
    pub env: BTreeMap<String, String>,
    /// 续接：`--resume <id>`。
    pub resume_id: Option<String>,
    /// 新会话：`--session-id <uuid>`，这样不用等 `system/init` 就知道 agent 会话 id。
    pub session_id: Option<String>,
}

fn invalid(field: &str, reason: impl Into<String>) -> AdapterError {
    AdapterError::InvalidSettings { field: field.to_string(), reason: reason.into() }
}

impl LaunchOptions {
    /// 拉起（含 resume）时用：`settings` 是 daemon 传来的**完整目标设置**，没给的字段 = 清除，回到配置 /
    /// `AgentInfo` 的默认值（或「没有」），和 [`Self::resolve_desired`] 同一套语义。
    ///
    /// **不读 `resume.launch_args`**：它只是上次进程启动时有效设置的快照，每一项（模型 / 思考 / 模式 /
    /// `system_prompt_append` / `max_*` / `fast_mode`）在 `SessionSettings` 里都有对应字段，没有哪一项是只有进程才知道的。
    /// daemon 在没有存活进程时（idle 关闭、daemon 重启、ended）改设置只会改库里的 `settings`，`launch_args` 可能已经过期；
    /// 要是在这里回落到它，用户清掉的系统提示追加 / 预算 / 轮数上限会在 resume 时悄悄复活。
    /// 所以从 `resume` 里只取 `agent_session_id`（拼 `--resume=<id>`）；没有 `resume` 就生成新的 `--session-id`。
    pub fn resolve(
        settings: &SessionSettings,
        config: &AdapterConfig,
        info: &AgentInfo,
        resume: Option<&ResumeHandle>,
    ) -> Result<Self> {
        let (resume_id, session_id) = match resume {
            Some(r) => (Some(r.agent_session_id.clone()), None),
            None => (None, Some(uuid::Uuid::new_v4().to_string())),
        };
        Self::build(settings, config, info, resume_id, session_id)
    }

    /// `AgentHandle::set_settings` 用：`settings` 是**完整的目标设置**（daemon 传的就是合并后的整份设置），
    /// 没给的字段 = 清除，回到配置 / `AgentInfo` 的默认值（或「没有」）。会话 id 沿用 `current` 的。
    pub fn resolve_desired(
        settings: &SessionSettings,
        config: &AdapterConfig,
        info: &AgentInfo,
        current: &LaunchOptions,
    ) -> Result<Self> {
        Self::build(settings, config, info, current.resume_id.clone(), current.session_id.clone())
    }

    fn build(
        settings: &SessionSettings,
        config: &AdapterConfig,
        info: &AgentInfo,
        resume_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<Self> {
        validate_settings_values(settings)?;

        let model = settings
            .model
            .clone()
            .or_else(|| config.default_model.clone())
            .or_else(|| info.models.iter().find(|m| m.default).map(|m| m.id.clone()));

        // 思考：显式设置必须合法；推导出来的值不合法就回落到模型默认。
        let model_thinking =
            model.as_ref().and_then(|id| info.models.iter().find(|m| &m.id == id)).and_then(|m| m.thinking.as_ref());
        let allowed = |t: &str| match model_thinking {
            Some(opts) => opts.options.iter().any(|o| o.id == t),
            None => t == THINKING_OFF || LEVELS_5X.contains(&t),
        };
        let thinking = match &settings.thinking {
            Some(t) => {
                if !allowed(t) {
                    return Err(invalid(
                        "thinking",
                        format!("{t} is not supported by {}", model.as_deref().unwrap_or("this model")),
                    ));
                }
                Some(t.clone())
            }
            None => config
                .default_thinking
                .clone()
                .filter(|t| allowed(t))
                .or_else(|| model_thinking.and_then(|o| o.default.clone())),
        };

        let mode = settings
            .mode
            .clone()
            .or_else(|| config.default_mode.clone())
            .or_else(|| info.modes.iter().find(|m| m.default).map(|m| m.id.clone()))
            .unwrap_or_else(|| "default".to_string());
        if !probe::modes().iter().any(|m| m.id == mode) {
            return Err(invalid("mode", format!("unknown mode {mode}")));
        }

        let fast_mode = match settings.options.get("fast_mode") {
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(invalid("options.fast_mode", "expected a boolean")),
            None => false,
        };

        let opts = Self {
            model,
            thinking,
            mode,
            system_prompt_append: settings.system_prompt_append.clone(),
            max_turns: settings.max_turns,
            max_budget_usd: settings.max_budget_usd,
            fast_mode,
            env: settings.env.clone(),
            resume_id,
            session_id,
        };
        opts.validate()?;
        Ok(opts)
    }

    /// 检查最终要写进 argv 的每个值。`resolve*` 会调；`ClaudeHandle::start` 在拼 argv 前再调一次，
    /// 所以手工构造的 `LaunchOptions` 也过不了这一关。
    pub fn validate(&self) -> Result<()> {
        if let Some(m) = &self.model {
            check_model(m)?;
        }
        if let Some(t) = &self.thinking {
            check_effort(t)?;
        }
        if !probe::modes().iter().any(|m| m.id == self.mode) {
            return Err(invalid("mode", format!("unknown mode {}", self.mode)));
        }
        if let Some(s) = &self.system_prompt_append {
            check_system_prompt_append(s)?;
        }
        if let Some(x) = self.max_budget_usd {
            check_budget(x)?;
        }
        if let Some(id) = &self.resume_id {
            check_session_id("resume", id)?;
        }
        if let Some(id) = &self.session_id {
            check_session_id("session_id", id)?;
        }
        Ok(())
    }

    /// `bypassPermissions` 需要启动时就带 `--dangerously-skip-permissions`，之后才能在会话里切过去。
    pub fn dangerous(&self) -> bool {
        self.mode == MODE_BYPASS
    }

    /// 可变参数（不含 [`base_args`]）。带值的选项一律是单个 `--flag=value` 元素；值须已通过 [`Self::validate`]。
    pub fn to_args(&self) -> Vec<String> {
        fn kv(a: &mut Vec<String>, k: &str, v: &str) {
            a.push(format!("{k}={v}"));
        }
        let mut a: Vec<String> = Vec::new();
        if let Some(m) = &self.model {
            kv(&mut a, "--model", m);
        }
        if let Some(t) = self.thinking.as_ref().filter(|t| t.as_str() != THINKING_OFF) {
            kv(&mut a, "--effort", t);
        }
        kv(&mut a, "--permission-mode", &self.mode);
        if self.dangerous() {
            a.push("--dangerously-skip-permissions".to_string());
        }
        if let Some(s) = &self.system_prompt_append {
            kv(&mut a, "--append-system-prompt", s);
        }
        if let Some(n) = self.max_turns {
            kv(&mut a, "--max-turns", &n.to_string());
        }
        if let Some(x) = self.max_budget_usd {
            kv(&mut a, "--max-budget-usd", &x.to_string());
        }
        if self.fast_mode {
            // 未经真实 CLI 验证：`fastMode` 是 settings.json 的键，这里用内联 JSON 传。
            kv(&mut a, "--settings", &json!({ "fastMode": true }).to_string());
        }
        if let Some(id) = &self.resume_id {
            kv(&mut a, "--resume", id);
        } else if let Some(id) = &self.session_id {
            kv(&mut a, "--session-id", id);
        }
        a
    }

    /// 写进 `ResumeHandle.launch_args`。
    pub fn to_json(&self) -> Value {
        json!({
            "model": self.model,
            "thinking": self.thinking,
            "mode": self.mode,
            "system_prompt_append": self.system_prompt_append,
            "max_turns": self.max_turns,
            "max_budget_usd": self.max_budget_usd,
            "fast_mode": self.fast_mode,
        })
    }

    /// 子进程环境：**原样**用 daemon 给的环境（剔除「在某个 agent 会话里」的标记是 daemon 的事，只在 `env.rs` 做一次，
    /// 这里不再重复）；唯一的改动是关思考用 `MAX_THINKING_TOKENS=0`。
    pub fn child_env(&self, base: &HashMap<String, String>) -> HashMap<String, String> {
        let mut env = base.clone();
        if self.thinking.as_deref() == Some(THINKING_OFF) {
            env.insert("MAX_THINKING_TOKENS".to_string(), "0".to_string());
        }
        env
    }
}

/// 找到要执行的程序 + 固定前置参数（见 [`crate::common::bin::resolve_program`]）。
pub fn resolve_program(
    config: &AdapterConfig,
    probed_path: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<(PathBuf, Vec<String>)> {
    crate::common::bin::resolve_program(config, probed_path, env, DEFAULT_COMMAND, probe::INSTALL_HINT)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::claude_code::models::{assemble, builtin_models, BUILTIN};
    use anycon_protocol::{AgentCapabilities, AgentStatus};

    fn info() -> AgentInfo {
        AgentInfo {
            id: "claude-code".into(),
            label: "Claude Code".into(),
            driver: "claude-stream-json".into(),
            enabled: true,
            status: AgentStatus::Ready,
            version: Some("2.1.293".into()),
            path: None,
            issue: None,
            models: assemble(builtin_models(BUILTIN, None), vec![], &[]),
            allow_custom_model: true,
            modes: probe::modes(),
            options: probe::options(),
            capabilities: AgentCapabilities::default(),
            probed_at_ms: 0,
        }
    }

    fn resolve(settings: SessionSettings, resume: Option<ResumeHandle>) -> Result<LaunchOptions> {
        LaunchOptions::resolve(&settings, &AdapterConfig::default(), &info(), resume.as_ref())
    }

    #[test]
    fn base_args_are_stable() {
        assert_eq!(
            base_args(),
            [
                "-p",
                "--output-format=stream-json",
                "--input-format=stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool=stdio"
            ]
        );
    }

    #[test]
    fn defaults_are_explicit() {
        let o = resolve(SessionSettings::default(), None).unwrap();
        assert_eq!(o.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(o.thinking.as_deref(), Some("medium"));
        assert_eq!(o.mode, "default");
        let args = o.to_args();
        assert_eq!(&args[..3], ["--model=claude-opus-5-5", "--effort=medium", "--permission-mode=default"]);
        assert_eq!(args.len(), 4);
        assert!(uuid::Uuid::parse_str(args[3].strip_prefix("--session-id=").unwrap()).is_ok());
        assert!(!o.dangerous());
    }

    #[test]
    fn config_defaults_beat_info_defaults() {
        let cfg = AdapterConfig {
            default_model: Some("claude-haiku-5-5".into()),
            default_thinking: Some("low".into()),
            default_mode: Some("acceptEdits".into()),
            ..Default::default()
        };
        let o = LaunchOptions::resolve(&SessionSettings::default(), &cfg, &info(), None).unwrap();
        assert_eq!(
            (o.model.as_deref(), o.thinking.as_deref(), o.mode.as_str()),
            (Some("claude-haiku-5-5"), Some("low"), "acceptEdits")
        );
    }

    #[test]
    fn full_settings_to_args() {
        let mut options = serde_json::Map::new();
        options.insert("fast_mode".into(), json!(true));
        let o = resolve(
            SessionSettings {
                model: Some("claude-sonnet-5-5".into()),
                thinking: Some("xhigh".into()),
                mode: Some("plan".into()),
                options,
                system_prompt_append: Some("Be terse.".into()),
                max_turns: Some(7),
                max_budget_usd: Some(1.5),
                ..Default::default()
            },
            Some(ResumeHandle { agent_session_id: "abc-123".into(), launch_args: Value::Null }),
        )
        .unwrap();
        assert_eq!(
            o.to_args(),
            vec![
                "--model=claude-sonnet-5-5",
                "--effort=xhigh",
                "--permission-mode=plan",
                "--append-system-prompt=Be terse.",
                "--max-turns=7",
                "--max-budget-usd=1.5",
                "--settings={\"fastMode\":true}",
                "--resume=abc-123",
            ]
        );
    }

    /// ADR-34 第 9 条：argv 只由设置决定、和 cwd 无关——cwd 是 worktree 时同样不加 `--add-dir`
    /// （不把主仓库 `.git` 放进可写目录）。
    #[test]
    fn argv_never_adds_extra_directories() {
        for mode in ["default", "acceptEdits", "plan", "bypassPermissions"] {
            let o = resolve(SessionSettings { mode: Some(mode.into()), ..Default::default() }, None).unwrap();
            let all: Vec<String> = base_args().into_iter().chain(o.to_args()).collect();
            assert!(!all.iter().any(|a| a.contains("add-dir")), "{mode}: {all:?}");
        }
    }

    #[test]
    fn bypass_mode_adds_dangerous_flag() {
        let o =
            resolve(SessionSettings { mode: Some("bypassPermissions".into()), ..Default::default() }, None).unwrap();
        let a = o.to_args();
        assert!(a.contains(&"--permission-mode=bypassPermissions".to_string()));
        assert!(a.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(o.dangerous());
    }

    #[test]
    fn thinking_off_uses_env_not_flag() {
        let o = resolve(
            SessionSettings {
                model: Some("claude-opus-4-6".into()),
                thinking: Some("off".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(!o.to_args().iter().any(|a| a.starts_with("--effort")));
        let env = o.child_env(&HashMap::from([("PATH".to_string(), "/bin".to_string())]));
        assert_eq!(env.get("MAX_THINKING_TOKENS").map(String::as_str), Some("0"));
        // 其它情况不设
        let o = resolve(SessionSettings::default(), None).unwrap();
        assert!(!o.child_env(&HashMap::new()).contains_key("MAX_THINKING_TOKENS"));
    }

    #[test]
    fn env_is_passed_through_unchanged() {
        // 剔除「嵌套会话」标记是 daemon 的事（env.rs，只做一次）；adapter 不再动它们
        let base = HashMap::from([
            ("CLAUDECODE".to_string(), "1".to_string()),
            ("CLAUDE_CODE_ENTRYPOINT".to_string(), "cli".to_string()),
            ("KEEP".to_string(), "me".to_string()),
        ]);
        let env = resolve(SessionSettings::default(), None).unwrap().child_env(&base);
        assert_eq!(env, base);
        // 关思考是唯一的追加项
        let off = resolve(
            SessionSettings {
                model: Some("claude-opus-4-6".into()),
                thinking: Some("off".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .child_env(&base);
        let mut want = base.clone();
        want.insert("MAX_THINKING_TOKENS".into(), "0".into());
        assert_eq!(off, want);
    }

    #[test]
    fn invalid_settings_rejected() {
        // 5.x 不能关思考
        assert!(matches!(
            resolve(
                SessionSettings {
                    model: Some("claude-opus-5-5".into()),
                    thinking: Some("off".into()),
                    ..Default::default()
                },
                None
            ),
            Err(AdapterError::InvalidSettings { .. })
        ));
        assert!(resolve(SessionSettings { mode: Some("nope".into()), ..Default::default() }, None).is_err());
        let mut options = serde_json::Map::new();
        options.insert("fast_mode".into(), json!("yes"));
        assert!(resolve(SessionSettings { options, ..Default::default() }, None).is_err());
    }

    #[test]
    fn derived_invalid_thinking_falls_back_to_model_default() {
        // 配置默认 xhigh，但所选 4.6 模型没有 xhigh
        let cfg = AdapterConfig { default_thinking: Some("xhigh".into()), ..Default::default() };
        let o = LaunchOptions::resolve(
            &SessionSettings { model: Some("claude-opus-4-6".into()), ..Default::default() },
            &cfg,
            &info(),
            None,
        )
        .unwrap();
        assert_eq!(o.thinking.as_deref(), Some("medium"));
    }

    #[test]
    fn custom_model_without_thinking_options_passes_effort_only_if_asked() {
        let o = resolve(SessionSettings { model: Some("my-proxy-model".into()), ..Default::default() }, None).unwrap();
        assert_eq!(o.thinking, None);
        assert!(!o.to_args().iter().any(|a| a.starts_with("--effort")));
        let o = resolve(
            SessionSettings {
                model: Some("my-proxy-model".into()),
                thinking: Some("high".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(o.to_args().contains(&"--effort=high".to_string()));
    }

    #[test]
    fn resume_takes_only_the_session_id_from_the_prior_launch_args() {
        let first = resolve(
            SessionSettings {
                model: Some("claude-haiku-5-5".into()),
                thinking: Some("high".into()),
                mode: Some("acceptEdits".into()),
                max_turns: Some(3),
                max_budget_usd: Some(2.0),
                system_prompt_append: Some("hi".into()),
                options: serde_json::Map::from_iter([("fast_mode".to_string(), json!(true))]),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let resume = ResumeHandle { agent_session_id: "sid-1".into(), launch_args: first.to_json() };
        // daemon 传的是完整目标设置：什么都没给 = 全部清除 / 回到默认，上次的 launch_args 一项也不补回来
        let again = resolve(SessionSettings::default(), Some(resume.clone())).unwrap();
        let fresh = resolve(SessionSettings::default(), None).unwrap();
        assert_eq!(again.model, fresh.model);
        assert_eq!(again.thinking, fresh.thinking);
        assert_eq!(again.mode, "default");
        assert_eq!(again.system_prompt_append, None);
        assert_eq!((again.max_turns, again.max_budget_usd, again.fast_mode), (None, None, false));
        // 只有会话 id 取自 resume
        assert_eq!(again.resume_id.as_deref(), Some("sid-1"));
        assert_eq!(again.session_id, None);
        let args = again.to_args();
        for flag in ["--append-system-prompt", "--max-turns", "--max-budget-usd", "--settings"] {
            assert!(!args.iter().any(|a| a.starts_with(flag)), "{flag} came back from launch_args: {args:?}");
        }
        assert!(args.contains(&"--resume=sid-1".to_string()));

        // 对照：settings 里带了值就用 settings 的（不是 launch_args 的）
        let changed = resolve(
            SessionSettings {
                thinking: Some("low".into()),
                system_prompt_append: Some("new".into()),
                max_turns: Some(9),
                ..Default::default()
            },
            Some(resume),
        )
        .unwrap();
        assert_eq!(changed.thinking.as_deref(), Some("low"));
        assert_eq!(changed.model, fresh.model, "model comes from the settings / defaults, not from launch_args");
        assert_eq!(changed.system_prompt_append.as_deref(), Some("new"));
        assert_eq!((changed.max_turns, changed.max_budget_usd), (Some(9), None));
    }

    #[test]
    fn program_resolution_order() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), tmp.path().to_string_lossy().into_owned()),
        ]);
        // 配置里的绝对路径直接用，带前置参数
        let cfg = AdapterConfig { command: Some("/opt/x/claude --profile a".into()), ..Default::default() };
        let (p, pre) = resolve_program(&cfg, Some("/probed"), &env).unwrap();
        assert_eq!((p, pre), (PathBuf::from("/opt/x/claude"), vec!["--profile".to_string(), "a".to_string()]));
        // 否则用探测时选中的路径
        let probed = tmp.path().join("claude");
        std::fs::write(&probed, "").unwrap();
        let (p, _) = resolve_program(&AdapterConfig::default(), probed.to_str(), &env).unwrap();
        assert_eq!(p, probed);
        // 都没有 → NotInstalled
        // （已知目录里的 /opt/homebrew/bin、/usr/local/bin 不受临时 HOME 影响，机器上真装在那里时跳过这条断言）
        if !Path::new("/opt/homebrew/bin/claude").exists() && !Path::new("/usr/local/bin/claude").exists() {
            let err = resolve_program(&AdapterConfig::default(), None, &env);
            assert!(matches!(err, Err(AdapterError::NotInstalled(_))));
        }
        // 配置写坏了
        let cfg = AdapterConfig { command: Some("claude 'oops".into()), ..Default::default() };
        assert!(matches!(resolve_program(&cfg, None, &env), Err(AdapterError::Spawn(_))));
    }

    #[test]
    fn relative_paths_never_resolve_against_the_project() {
        let env = HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        // 配置里的相对路径（会按子进程 cwd = 项目目录解析）：拒绝
        for cmd in ["./claude", "bin/claude --x"] {
            let cfg = AdapterConfig { command: Some(cmd.into()), ..Default::default() };
            assert!(matches!(resolve_program(&cfg, None, &env), Err(AdapterError::Spawn(_))), "{cmd}");
        }
        // 探测记下的路径是相对的：不采用，退回现找（找到的一定是绝对路径）
        if let Ok((p, _)) = resolve_program(&AdapterConfig::default(), Some("claude"), &env) {
            assert!(p.is_absolute(), "{p:?}");
        }
    }

    #[test]
    fn lookup_ignores_relative_path_entries_and_uses_a_neutral_cwd() {
        use std::os::unix::fs::PermissionsExt;
        // 「克隆下来的仓库」：里面有个可执行的 `claude`，PATH 里有 `.` 和 `./node_modules/.bin` 和空项
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("node_modules/.bin")).unwrap();
        for p in [repo.path().join("claude"), repo.path().join("node_modules/.bin/claude")] {
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let home = tempfile::tempdir().unwrap();
        let env = HashMap::from([
            ("PATH".to_string(), ".:./node_modules/.bin::/nonexistent".to_string()),
            ("HOME".to_string(), home.path().to_string_lossy().into_owned()),
        ]);
        // 即使进程 cwd / 传入的 cwd 就是这个仓库，也不会把相对项解析过去
        let found = crate::common::bin::collect_candidates("claude", &env, repo.path());
        assert!(
            found.iter().all(|(p, _)| !p.starts_with(repo.path())),
            "a relative PATH entry resolved into the project: {found:?}"
        );
        assert!(crate::common::bin::which_all("claude", ".:./node_modules/.bin:", repo.path()).is_empty());
        // 相对的 HOME 同理：已知目录不会按进程 cwd 解析
        let rel_home =
            HashMap::from([("PATH".to_string(), "/nonexistent".to_string()), ("HOME".to_string(), ".".to_string())]);
        assert!(crate::common::bin::collect_candidates("claude", &rel_home, repo.path())
            .iter()
            .all(|(p, _)| p.is_absolute()));
    }

    #[test]
    fn argv_values_cannot_become_flags() {
        // 以 - 开头 / 含空白或控制字符的模型 id 一律拒绝
        for bad in ["--dangerously-skip-permissions", "-x", " m", "m x", "m\nx", "m\0", "", "é", &"m".repeat(201)] {
            let r = resolve(SessionSettings { model: Some(bad.into()), ..Default::default() }, None);
            assert!(matches!(r, Err(AdapterError::InvalidSettings { ref field, .. }) if field == "model"), "{bad:?}");
        }
        // 正常的写法都在
        for ok in [
            "claude-opus-5-5",
            "opus",
            "sonnet[1m]",
            "claude-opus@20250101",
            "us.anthropic.claude-sonnet-5-5-v1:0",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-x",
            "my-proxy-model",
        ] {
            let o = resolve(SessionSettings { model: Some(ok.into()), ..Default::default() }, None).unwrap();
            assert_eq!(o.to_args()[0], format!("--model={ok}"));
        }
        // 每个带值的选项都是单个 `--flag=value` 元素：值里再有空格 / 以 - 开头的内容也不会拆开
        let o = resolve(
            SessionSettings {
                system_prompt_append: Some("--model evil\n--dangerously-skip-permissions".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let a = o.to_args();
        assert!(a.contains(&"--append-system-prompt=--model evil\n--dangerously-skip-permissions".to_string()));
        assert!(!a.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(base_args()
            .iter()
            .chain(a.iter())
            .all(|x| !x.starts_with("--") || x.contains('=') || !x.contains(' ')));
        for flag in ["--model", "--effort", "--permission-mode", "--append-system-prompt", "--output-format"] {
            assert!(!base_args().iter().chain(a.iter()).any(|x| x == flag), "{flag} must carry its value inline");
        }
    }

    #[test]
    fn system_prompt_append_is_capped_and_clean() {
        let ok = "x".repeat(MAX_SYSTEM_PROMPT_APPEND_BYTES);
        assert!(resolve(SessionSettings { system_prompt_append: Some(ok), ..Default::default() }, None).is_ok());
        let too_long = "x".repeat(MAX_SYSTEM_PROMPT_APPEND_BYTES + 1);
        assert!(resolve(SessionSettings { system_prompt_append: Some(too_long), ..Default::default() }, None).is_err());
        // 换行 / 制表符可以，NUL 和其它控制字符不行
        assert!(resolve(
            SessionSettings { system_prompt_append: Some("a\n\tb\r\n".into()), ..Default::default() },
            None
        )
        .is_ok());
        for bad in ["a\0b", "a\u{1b}[31m", "a\u{7f}"] {
            let r = resolve(SessionSettings { system_prompt_append: Some(bad.into()), ..Default::default() }, None);
            assert!(
                matches!(r, Err(AdapterError::InvalidSettings { ref field, .. }) if field == "system_prompt_append"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn budget_must_be_a_finite_positive_number() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0, -0.0] {
            let r = resolve(SessionSettings { max_budget_usd: Some(bad), ..Default::default() }, None);
            assert!(
                matches!(r, Err(AdapterError::InvalidSettings { ref field, .. }) if field == "max_budget_usd"),
                "{bad}"
            );
        }
        let o = resolve(SessionSettings { max_budget_usd: Some(0.25), ..Default::default() }, None).unwrap();
        assert!(o.to_args().contains(&"--max-budget-usd=0.25".to_string()));
    }

    #[test]
    fn session_ids_are_checked() {
        for bad in ["--resume", "-x", "a b", "a\nb", "", &"a".repeat(129)] {
            let resume = ResumeHandle { agent_session_id: bad.into(), launch_args: Value::Null };
            let r = resolve(SessionSettings::default(), Some(resume));
            assert!(matches!(r, Err(AdapterError::InvalidSettings { ref field, .. }) if field == "resume"), "{bad:?}");
        }
        let resume =
            ResumeHandle { agent_session_id: "0d9d5f1e-6b1c-4a52-9a43-1d6f3a2b7c11".into(), launch_args: Value::Null };
        assert!(resolve(SessionSettings::default(), Some(resume)).is_ok());
        // 落库的 launch_args 不参与解析：里面被篡改的值根本到不了 argv
        let resume = ResumeHandle {
            agent_session_id: "sid-1".into(),
            launch_args: json!({"model": "--dangerously-skip-permissions", "system_prompt_append": "--x"}),
        };
        let o = resolve(SessionSettings::default(), Some(resume)).unwrap();
        assert!(!o.to_args().iter().any(|a| a.contains("dangerously") || a.contains("--x")), "{:?}", o.to_args());
        // 手工构造的选项在拼 argv 前也能被 validate 拦住
        let mut o = resolve(SessionSettings::default(), None).unwrap();
        o.model = Some("-x".into());
        assert!(o.validate().is_err());
    }

    #[test]
    fn thinking_level_is_a_clean_token() {
        let r = resolve(
            SessionSettings {
                model: Some("my-proxy-model".into()),
                thinking: Some("--x".into()),
                ..Default::default()
            },
            None,
        );
        assert!(matches!(r, Err(AdapterError::InvalidSettings { ref field, .. }) if field == "thinking"));
    }

    #[test]
    fn desired_settings_clear_what_is_missing() {
        let full = resolve(
            SessionSettings {
                model: Some("claude-haiku-5-5".into()),
                thinking: Some("high".into()),
                mode: Some("acceptEdits".into()),
                system_prompt_append: Some("hi".into()),
                max_turns: Some(3),
                max_budget_usd: Some(2.0),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // 什么都不给 = 回到默认 / 清除（不会从 `current` 里补）
        let cleared =
            LaunchOptions::resolve_desired(&SessionSettings::default(), &AdapterConfig::default(), &info(), &full)
                .unwrap();
        assert_eq!(cleared.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(cleared.mode, "default");
        assert_eq!(cleared.system_prompt_append, None);
        assert_eq!((cleared.max_turns, cleared.max_budget_usd), (None, None));
        assert!(!cleared.fast_mode);
        // 会话 id 沿用
        assert_eq!(cleared.session_id, full.session_id);
        // resolve（spawn / resume 路径）同一套语义：不会用上次的 launch_args 补，只取会话 id
        let resume = ResumeHandle { agent_session_id: "sid".into(), launch_args: full.to_json() };
        let respawned = resolve(SessionSettings::default(), Some(resume)).unwrap();
        assert_eq!(respawned.system_prompt_append, None);
        assert_eq!((respawned.max_turns, respawned.max_budget_usd), (None, None));
        assert_eq!(respawned.model, cleared.model);
        assert_eq!(respawned.mode, cleared.mode);
        assert_eq!(respawned.resume_id.as_deref(), Some("sid"));
    }
}
