//! 子进程环境解析（docs/spec/daemon.md § 5.5）。
//!
//! launchd / systemd 起的 daemon 没有用户 shell 的 `PATH`。启动时用 `$SHELL -ilc 'printenv -0'` 取登录 shell 的环境
//! （15 秒超时，失败再试 `-lc`，都失败退回进程自身环境），剔除 `ENV_STRIP`，再叠加配置里的 `agents.env` / `agents.<id>.env`。
//!
//! Windows 上没有登录 shell 这回事：用户的环境变量在注册表里，进程（包括由计划任务拉起的 daemon）启动时就拿到了，直接用进程环境。
//! Windows 的变量名不区分大小写，表里的键统一按 [`anycon_agent_api::env_key_canonical`] 存（大写），叠加配置时同名变量互相覆盖，
//! 不会以两种写法并存。

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anycon_agent_api::{env_key_canonical, ENV_STRIP};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSource {
    /// `$SHELL -ilc`
    LoginShellInteractive,
    /// `$SHELL -lc`
    LoginShell,
    /// 退回进程自身环境。
    Process,
}

impl EnvSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EnvSource::LoginShellInteractive => "login shell (-ilc)",
            EnvSource::LoginShell => "login shell (-lc)",
            EnvSource::Process => "process environment",
        }
    }
}

/// 解析结果：daemon 环境。
#[derive(Debug, Clone)]
pub struct DaemonEnv {
    /// 已剔除 `ENV_STRIP`，**未**叠加配置。
    pub base: Arc<HashMap<String, String>>,
    pub source: EnvSource,
    pub shell: Option<String>,
    pub warning: Option<String>,
}

impl DaemonEnv {
    pub fn path(&self) -> String {
        self.base.get("PATH").cloned().unwrap_or_default()
    }

    /// 叠加 `agents.env` 与 `agents.<id>.env` 后的环境（探测与拉起 agent 用）。
    pub fn for_agent(&self, cfg: &Config, agent_id: &str) -> Arc<HashMap<String, String>> {
        let mut m: HashMap<String, String> = (*self.base).clone();
        for (k, v) in &cfg.agents.env {
            m.insert(env_key_canonical(k).into_owned(), v.clone());
        }
        if let Some(a) = cfg.adapter_config(agent_id) {
            for (k, v) in &a.env {
                m.insert(env_key_canonical(k).into_owned(), v.clone());
            }
        }
        Arc::new(m)
    }

    /// 在 `for_agent` 的基础上叠加会话级 `settings.env`。
    ///
    /// 校验阶段（[`validate_session_env`]）已经拒绝了不合规的键；这里再防御一次：`permissions.allow_session_env = false`
    /// 时整体忽略，键不在 `permissions.session_env_allow` 里、不过 `env_key_allowed`、或者是运维在 `agents.env` /
    /// `agents.<id>.env` 里配置过的就丢弃（会话 env 不能覆盖运维配置）。
    pub fn for_session(
        &self,
        cfg: &Config,
        agent_id: &str,
        session_env: &BTreeMap<String, String>,
    ) -> Arc<HashMap<String, String>> {
        let mut m: HashMap<String, String> = (*self.for_agent(cfg, agent_id)).clone();
        if !cfg.permissions.allow_session_env {
            if !session_env.is_empty() {
                tracing::warn!("session env ignored: permissions.allow_session_env is false");
            }
            return Arc::new(m);
        }
        for (k, v) in session_env {
            if anycon_agent_api::env_key_allowed(k).is_err()
                || !anycon_agent_api::env_key_in_allowlist(k, &cfg.permissions.session_env_allow)
                || operator_sets(cfg, agent_id, k)
                || v.contains('\0')
            {
                tracing::warn!(key = %k, "dropping a session env variable that is not allowed");
                continue;
            }
            m.insert(env_key_canonical(k).into_owned(), v.clone());
        }
        Arc::new(m)
    }
}

/// 运维是否在配置里设置过这个环境变量（`agents.env` 或 `agents.<id>.env`；Windows 上不区分大小写）。
fn operator_sets(cfg: &Config, agent_id: &str, key: &str) -> bool {
    let key = env_key_canonical(key);
    let sets = |env: &HashMap<String, String>| env.keys().any(|k| env_key_canonical(k) == key);
    sets(&cfg.agents.env) || cfg.adapter_config(agent_id).is_some_and(|a| sets(&a.env))
}

/// 会话 env 最多这么多个、键值最长这么长。
const SESSION_ENV_MAX_ENTRIES: usize = 64;
const SESSION_ENV_MAX_VALUE: usize = 8 * 1024;

/// 校验 `settings.env`（ADR-31，2026-10 修订：允许列表为主、黑名单兜底）。返回第一个违规的 `(field, reason)`，
/// `field` 形如 `env` 或 `env.<KEY>`。
///
/// - 非空 env 要求 `permissions.allow_session_env = true`，否则 `field = "env"`；
/// - 每个键依次要过三关：
///   1. `anycon_agent_api::env_key_allowed`（合法标识符、不在 `ENV_PROTECTED`、不命中黑名单）——**黑名单优先报**，
///      这是永久的，不管运维怎么配置；
///   2. 在运维配置的 `permissions.session_env_allow` 里（`env_key_in_allowlist`：精确键名或 `PREFIX_*`；默认空）；
///   3. 不覆盖运维在配置里设置的键（`agents.env` / `agents.<id>.env`）；
/// - 值不含 NUL、长度有上限。
pub fn validate_session_env(
    cfg: &Config,
    agent_id: &str,
    env: &BTreeMap<String, String>,
) -> Result<(), (String, String)> {
    if env.is_empty() {
        return Ok(());
    }
    if !cfg.permissions.allow_session_env {
        return Err((
            "env".into(),
            "per-session environment variables are disabled on this node (permissions.allow_session_env = false)"
                .into(),
        ));
    }
    if env.len() > SESSION_ENV_MAX_ENTRIES {
        return Err(("env".into(), format!("at most {SESSION_ENV_MAX_ENTRIES} variables")));
    }
    for (k, v) in env {
        let field = format!("env.{k}");
        anycon_agent_api::env_key_allowed(k).map_err(|reason| (field.clone(), format!("{reason} (denylisted)")))?;
        if !anycon_agent_api::env_key_in_allowlist(k, &cfg.permissions.session_env_allow) {
            return Err((
                field,
                "not in permissions.session_env_allow; the node's operator must list the variables clients may set"
                    .into(),
            ));
        }
        if operator_sets(cfg, agent_id, k) {
            return Err((field, "set in the node's config (agents.env); it cannot be overridden per session".into()));
        }
        if v.contains('\0') || v.len() > SESSION_ENV_MAX_VALUE {
            return Err((field, "value must not contain NUL and must be at most 8 KiB".into()));
        }
    }
    Ok(())
}

/// 解析 `printenv -0` 输出。
pub fn parse_printenv0(raw: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for entry in raw.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(entry);
        if let Some((k, v)) = s.split_once('=') {
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}

/// 环境变量表，键按 [`env_key_canonical`] 规范化（Windows 上统一大写）。
pub fn canonical_env(vars: impl IntoIterator<Item = (String, String)>) -> HashMap<String, String> {
    vars.into_iter().map(|(k, v)| (env_key_canonical(&k).into_owned(), v)).collect()
}

/// 剔除 `ENV_STRIP`（「我正在某个 agent 会话里」的标记）。**整个系统里只有这一处**剥离：adapters 不再自己剥，
/// 它们拿到的环境（探测和拉起都一样）已经是干净的。
fn strip(mut m: HashMap<String, String>) -> HashMap<String, String> {
    for k in ENV_STRIP {
        m.remove(*k);
    }
    m
}

async fn run_shell(shell: &str, flags: &str, timeout: Duration) -> Result<HashMap<String, String>, String> {
    let mut cmd = Command::new(shell);
    // macOS 的 BSD printenv 没有 -0（会报 illegal option），`env -0` 两边都有：先试 printenv，失败再退到 env
    cmd.arg(flags)
        .arg("printenv -0 2>/dev/null || env -0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("spawn {shell}: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let work = async {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((buf, status))
    };
    match tokio::time::timeout(timeout, work).await {
        Err(_) => Err(format!("{shell} {flags}: timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(e),
        Ok(Ok((buf, status))) => {
            let m = parse_printenv0(&buf);
            if m.is_empty() {
                Err(format!("{shell} {flags}: exit {:?}, no variables", status.code()))
            } else {
                Ok(m)
            }
        }
    }
}

/// 解析 daemon 环境。`use_login_shell = false` 时（以及在 Windows 上）直接用进程环境。
pub async fn resolve(use_login_shell: bool, timeout: Duration) -> DaemonEnv {
    let process_env = canonical_env(std::env::vars());
    if cfg!(windows) {
        // 从 Git Bash 里启动时 `SHELL` 是 `/usr/bin/bash` 这种 MSYS 路径，不能拿来用
        return DaemonEnv {
            base: Arc::new(strip(process_env)),
            source: EnvSource::Process,
            shell: None,
            warning: None,
        };
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    if !use_login_shell {
        return DaemonEnv { base: Arc::new(strip(process_env)), source: EnvSource::Process, shell, warning: None };
    }
    let Some(sh) = shell.clone() else {
        return DaemonEnv {
            base: Arc::new(strip(process_env)),
            source: EnvSource::Process,
            shell: None,
            warning: Some("$SHELL is not set; using the process environment".into()),
        };
    };
    let mut errors = Vec::new();
    match run_shell(&sh, "-ilc", timeout).await {
        Ok(m) => {
            return DaemonEnv {
                base: Arc::new(strip(m)),
                source: EnvSource::LoginShellInteractive,
                shell,
                warning: None,
            }
        }
        Err(e) => errors.push(e),
    }
    match run_shell(&sh, "-lc", timeout).await {
        Ok(m) => {
            return DaemonEnv {
                base: Arc::new(strip(m)),
                source: EnvSource::LoginShell,
                shell,
                warning: Some(format!("interactive login shell failed ({}); used non-interactive", errors.join("; "))),
            }
        }
        Err(e) => errors.push(e),
    }
    tracing::warn!(errors = ?errors, "could not resolve login-shell environment; falling back to the process environment");
    DaemonEnv {
        base: Arc::new(strip(process_env)),
        source: EnvSource::Process,
        shell,
        warning: Some(format!(
            "login shell environment unavailable ({}); using the process environment",
            errors.join("; ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_shell(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn parses_nul_separated() {
        let m = parse_printenv0(b"A=1\0B=two words\0C=\0=bad\0weird key=x\0PATH=/a:/b\0");
        assert_eq!(m.get("A").unwrap(), "1");
        assert_eq!(m.get("B").unwrap(), "two words");
        assert_eq!(m.get("C").unwrap(), "");
        assert_eq!(m.get("PATH").unwrap(), "/a:/b");
        assert!(!m.contains_key("weird key"));
        assert_eq!(m.len(), 4);
    }

    // 注意：这些测试会改进程环境变量 SHELL，所以放在同一个测试里顺序执行。
    #[cfg(unix)]
    #[tokio::test]
    async fn login_shell_resolution_and_fallbacks() {
        let d = tempfile::tempdir().unwrap();
        // 1) 交互式登录 shell 成功；CLAUDECODE 被剔除
        let ok = fake_shell(
            d.path(),
            "shell-ok",
            r#"if [ "$1" = "-ilc" ]; then printf 'PATH=/fake/bin:/usr/bin\0FOO=from-shell\0CLAUDECODE=1\0CLAUDE_CODE_ENTRYPOINT=cli\0'; else exit 1; fi"#,
        );
        std::env::set_var("SHELL", &ok);
        let e = resolve(true, Duration::from_secs(5)).await;
        assert_eq!(e.source, EnvSource::LoginShellInteractive);
        assert_eq!(e.base.get("FOO").unwrap(), "from-shell");
        assert_eq!(e.path(), "/fake/bin:/usr/bin");
        assert!(!e.base.contains_key("CLAUDECODE"));
        assert!(!e.base.contains_key("CLAUDE_CODE_ENTRYPOINT"));

        // 2) -ilc 失败、-lc 成功
        let second = fake_shell(
            d.path(),
            "shell-lc",
            r#"if [ "$1" = "-lc" ]; then printf 'FOO=lc\0PATH=/lc\0'; else exit 3; fi"#,
        );
        std::env::set_var("SHELL", &second);
        let e = resolve(true, Duration::from_secs(5)).await;
        assert_eq!(e.source, EnvSource::LoginShell);
        assert_eq!(e.base.get("FOO").unwrap(), "lc");
        assert!(e.warning.is_some());

        // 3) 两次都失败：退回进程环境并带警告
        let bad = fake_shell(d.path(), "shell-bad", "exit 1");
        std::env::set_var("SHELL", &bad);
        let e = resolve(true, Duration::from_secs(5)).await;
        assert_eq!(e.source, EnvSource::Process);
        assert!(e.warning.unwrap().contains("process environment"));

        // 4) 超时
        let slow = fake_shell(d.path(), "shell-slow", "sleep 5");
        std::env::set_var("SHELL", &slow);
        let t = std::time::Instant::now();
        let e = resolve(true, Duration::from_millis(300)).await;
        assert_eq!(e.source, EnvSource::Process);
        assert!(t.elapsed() < Duration::from_secs(4));

        // 5) 关闭登录 shell 解析
        let e = resolve(false, Duration::from_secs(1)).await;
        assert_eq!(e.source, EnvSource::Process);
    }

    /// Windows：不跑登录 shell，进程环境的键统一成大写；配置里换个大小写写的同名变量覆盖它，而不是并存。
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_uses_the_process_environment_with_canonical_keys() {
        let env = resolve(true, Duration::from_secs(1)).await;
        assert_eq!(env.source, EnvSource::Process);
        assert!(env.base.contains_key("PATH"), "the Windows `Path` is stored as `PATH`");
        assert!(env.base.keys().all(|k| !k.bytes().any(|b| b.is_ascii_lowercase())));
        let mut cfg = Config::default();
        cfg.agents.env.insert("Path".into(), r"C:\tools".into());
        let m = env.for_agent(&cfg, "x");
        assert_eq!(m["PATH"], r"C:\tools");
        assert!(!m.contains_key("Path"));
    }

    #[test]
    fn layering_and_session_env_rules() {
        let base: HashMap<String, String> =
            [("PATH".to_string(), "/usr/bin".to_string()), ("A".to_string(), "base".to_string())].into();
        let env = DaemonEnv { base: Arc::new(base), source: EnvSource::Process, shell: None, warning: None };
        let mut cfg = Config::default();
        cfg.agents.env.insert("A".into(), "global".into());
        cfg.agents.env.insert("G".into(), "g".into());
        let mut ac = anycon_agent_api::AdapterConfig { enabled: true, ..Default::default() };
        ac.env.insert("A".into(), "agent".into());
        cfg.agents.agent.insert("x".into(), ac);
        let m = env.for_agent(&cfg, "x");
        assert_eq!(m["A"], "agent");
        assert_eq!(m["G"], "g");
        assert_eq!(env.for_agent(&cfg, "other")["A"], "global");

        // 默认关闭：任何非空 env 都被拒绝，且不会进子进程环境
        let mut sess = BTreeMap::new();
        sess.insert("FOO".to_string(), "bar".to_string());
        let (field, _) = validate_session_env(&cfg, "x", &sess).unwrap_err();
        assert_eq!(field, "env");
        assert!(!env.for_session(&cfg, "x", &sess).contains_key("FOO"));
        assert!(validate_session_env(&cfg, "x", &BTreeMap::new()).is_ok());

        // 开关打开了、允许列表还是空：一个都不行，reason 指向允许列表
        cfg.permissions.allow_session_env = true;
        let (field, reason) = validate_session_env(&cfg, "x", &sess).unwrap_err();
        assert_eq!(field, "env.FOO");
        assert!(reason.contains("session_env_allow"), "{reason}");
        assert!(!env.for_session(&cfg, "x", &sess).contains_key("FOO"), "defence in depth: not in the allowlist");

        // 列进允许列表（精确名或 PREFIX_*）之后：合规的键通过，不在列表里的仍然不行
        cfg.permissions.session_env_allow = vec!["FOO".into(), "FEATURE_*".into()];
        assert!(validate_session_env(&cfg, "x", &sess).is_ok());
        assert_eq!(env.for_session(&cfg, "x", &sess)["FOO"], "bar");
        let feature: BTreeMap<String, String> = [("FEATURE_X".to_string(), "1".to_string())].into();
        assert!(validate_session_env(&cfg, "x", &feature).is_ok());
        // Windows 的变量名不区分大小写：`feature_x` 就是 `FEATURE_X`，在允许列表里
        let rejected: &[&str] =
            if cfg!(windows) { &["OTHER", "FOO2", "FEATURE"] } else { &["OTHER", "FOO2", "feature_x", "FEATURE"] };
        for k in rejected {
            let e: BTreeMap<String, String> = [(k.to_string(), "v".to_string())].into();
            let (field, reason) = validate_session_env(&cfg, "x", &e).unwrap_err();
            assert_eq!(field, format!("env.{k}"));
            assert!(reason.contains("not in permissions.session_env_allow"), "{k}: {reason}");
        }

        // 即使运维把它们都写进了允许列表：运维配置过的键、受保护的键、黑名单键（含已知能执行代码的那些）仍然不行
        cfg.permissions.session_env_allow = [
            "A",
            "G",
            "PATH",
            "ANYCON_*",
            "BAD KEY",
            "1ABC",
            "SHELLOPTS",
            "PS4",
            "LESSOPEN",
            "LUA_INIT",
            "npm_config_registry",
            "RUSTC_WRAPPER",
        ]
        .map(String::from)
        .to_vec();
        for (k, why) in [
            ("A", "set in the node's config"),
            ("G", "set in the node's config"),
            ("PATH", "may not be set per session"),
            ("ANYCON_X", "ANYCON_"),
            ("BAD KEY", "invalid variable name"),
            ("1ABC", "invalid variable name"),
            ("SHELLOPTS", "may not be set per session"),
            ("PS4", "may not be set per session"),
            ("LESSOPEN", "may not be set per session"),
            ("LUA_INIT", "may not be set per session"),
            ("npm_config_registry", "may not be set per session"),
            ("RUSTC_WRAPPER", "may not be set per session"),
        ] {
            let e: BTreeMap<String, String> = [(k.to_string(), "v".to_string())].into();
            let (field, reason) = validate_session_env(&cfg, "x", &e).unwrap_err();
            assert_eq!(field, format!("env.{k}"));
            assert!(reason.contains(why), "{k}: {reason}");
            // 防御：即使绕过校验，for_session 也不会放行
            let m = env.for_session(&cfg, "x", &e);
            assert_ne!(m.get(k).map(String::as_str), Some("v"), "{k}");
        }
        cfg.permissions.session_env_allow = vec!["A".into(), "G".into(), "OK".into(), "K*".into()];
        // 另一个 agent 没有配置 A 的覆盖，但 G（全局）仍然受保护
        let e: BTreeMap<String, String> = [("G".to_string(), "v".to_string())].into();
        assert!(validate_session_env(&cfg, "other", &e).is_err());
        let e: BTreeMap<String, String> = [("A".to_string(), "v".to_string())].into();
        assert!(validate_session_env(&cfg, "other", &e).is_err(), "A is in agents.env");
        // 值里的 NUL、过多的键
        let e: BTreeMap<String, String> = [("OK".to_string(), "a\0b".to_string())].into();
        assert!(validate_session_env(&cfg, "x", &e).is_err());
        let many: BTreeMap<String, String> = (0..65).map(|i| (format!("K{i}"), "v".to_string())).collect();
        assert_eq!(validate_session_env(&cfg, "x", &many).unwrap_err().0, "env");
    }
}
