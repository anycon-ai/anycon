//! 各 adapter 的 `probe` / `diagnose` 共用的小件：总预算计时、探测环境、`--version` 超时、诊断字段。
//!
//! 不含任何 agent 专属知识。`claude_code`、`codex` 都用它。

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anycon_agent_api::ProbeContext;
use anycon_protocol::DiagnosticField;

use super::bin::{CandidateProbe, Resolution};
use super::run::run_capture;

/// `<cmd> --version` 的默认超时（规格：2 秒）。冷启动很慢的 shim（nvm / volta）可以用环境变量
/// `ANYCON_VERSION_TIMEOUT_MS`（放在 `agents.<id>.env` 或 daemon 环境里）调大。
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
pub const VERSION_TIMEOUT_ENV: &str = "ANYCON_VERSION_TIMEOUT_MS";
const SHELL_TYPE_TIMEOUT: Duration = Duration::from_secs(5);

/// 整个 `probe()` 的总预算：各步从剩余时间里取。
pub struct Budget {
    start: Instant,
    total: Duration,
}

impl Budget {
    pub fn new(total: Duration) -> Self {
        Self { start: Instant::now(), total }
    }

    pub fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.start.elapsed())
    }

    /// 单步上限 `d` 与剩余时间取小。
    pub fn cap(&self, d: Duration) -> Duration {
        d.min(self.remaining())
    }
}

/// 探测用的环境：daemon 给的 `ctx.env`（已含登录 shell 环境和 `config.env`，且已剔除「我正在某个 agent 会话里」
/// 的标记——那是 daemon 的事，只在一处做）；再叠加一遍 `config.env`（幂等，直接调 `probe` 的调用方也能生效）。
pub fn effective_env(ctx: &ProbeContext) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = ctx.env.as_ref().clone();
    for (k, v) in &ctx.config.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// 单个候选 `--version` 的超时：环境变量可调（200 ms – 60 s），缺省 2 秒。
pub fn version_timeout(env: &HashMap<String, String>) -> Duration {
    env.get(VERSION_TIMEOUT_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|ms| Duration::from_millis(ms.clamp(200, 60_000)))
        .unwrap_or(VERSION_TIMEOUT)
}

pub fn field(label: &str, value: impl Into<String>) -> DiagnosticField {
    DiagnosticField { label: label.to_string(), value: value.into() }
}

/// `$SHELL -lc 'type -a <cmd>'`：对照「终端里的命令和 daemon 找到的是不是同一个」。
pub async fn shell_type_a(env: &HashMap<String, String>, name: &str, cwd: &Path) -> String {
    let Some(shell) = env.get("SHELL").filter(|s| !s.is_empty()) else { return "(SHELL is not set)".to_string() };
    let script = format!("type -a {}", shell_words::quote(name));
    let out = run_capture(Path::new(shell), &["-lc".to_string(), script], env, cwd, SHELL_TYPE_TIMEOUT).await;
    let text = format!("{}{}", out.stdout.trim(), if out.stdout.trim().is_empty() { out.stderr.trim() } else { "" });
    if text.is_empty() {
        format!("(no output: {})", out.failure_reason())
    } else {
        text
    }
}

/// 各候选的探活结果，一行一个：`路径 [来源] ok: … / failed: … / not probed`。
pub fn candidate_lines(res: &Resolution) -> String {
    let lines: Vec<String> = res
        .candidates
        .iter()
        .map(|c| {
            let probe = match &c.probe {
                CandidateProbe::Ok { output } => format!("ok: {}", output.lines().next().unwrap_or("")),
                CandidateProbe::Failed { reason } => format!("failed: {reason}"),
                CandidateProbe::NotRun => "not probed".to_string(),
            };
            format!("{} [{}] {}", c.path.display(), c.origin.as_str(), probe)
        })
        .collect();
    if lines.is_empty() {
        "(none)".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_timeout_is_clamped() {
        let env = |v: &str| HashMap::from([(VERSION_TIMEOUT_ENV.to_string(), v.to_string())]);
        assert_eq!(version_timeout(&HashMap::new()), VERSION_TIMEOUT);
        assert_eq!(version_timeout(&env("5000")), Duration::from_millis(5000));
        assert_eq!(version_timeout(&env("1")), Duration::from_millis(200));
        assert_eq!(version_timeout(&env("999999999")), Duration::from_millis(60_000));
        assert_eq!(version_timeout(&env("abc")), VERSION_TIMEOUT);
    }

    #[test]
    fn budget_caps_steps() {
        let b = Budget::new(Duration::from_millis(50));
        assert!(b.cap(Duration::from_secs(10)) <= Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.remaining().is_zero());
        assert!(b.cap(Duration::from_secs(1)).is_zero());
    }
}
