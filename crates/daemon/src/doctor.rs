//! `anycon doctor` / `node.doctor`：逐项检查并给出修复建议（docs/spec/cli.md § 5）。
//!
//! daemon 在跑时通过 RPC 在 daemon 内执行（能看到 hub 连接状态和 agent 缓存）；daemon 没起来时 CLI 本地执行同一套
//! 检查的离线版本（`ctx = None`）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_protocol::{AgentStatus, DoctorCheck};

use crate::config::{Config, Paths};
use crate::daemon::Ctx;

pub const SERVICE_LABEL: &str = "ai.anycon.daemon";
pub const SYSTEMD_UNIT: &str = "anycon.service";
/// Windows 上 `anycon service` 注册的计划任务名。
pub const WINDOWS_TASK: &str = "anycon";

/// 服务单元文件路径：macOS launchd plist / Linux systemd user unit。Windows 的服务是计划任务，没有单元文件（[`WINDOWS_TASK`]）。
pub fn service_unit_path() -> Option<PathBuf> {
    let home = crate::util::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist")))
    } else if cfg!(target_os = "linux") {
        Some(home.join(".config/systemd/user").join(SYSTEMD_UNIT))
    } else {
        None
    }
}

/// 路径所在文件系统的可用字节数（取不到时 0）。
pub fn disk_free(path: &Path) -> u64 {
    crate::sys::disk_free(path).unwrap_or(0)
}

/// Windows：计划任务 [`WINDOWS_TASK`] 是否已注册（`schtasks /Query` 的退出码）。跑不了 `schtasks` 时 `None`。
pub async fn windows_task_installed() -> Option<bool> {
    let mut cmd = tokio::process::Command::new("schtasks");
    cmd.args(["/Query", "/TN", WINDOWS_TASK])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(10), cmd.status()).await {
        Ok(Ok(s)) => Some(s.success()),
        _ => None,
    }
}

fn check(id: &str, ok: bool, detail: impl Into<String>, fix: Option<&str>) -> DoctorCheck {
    DoctorCheck { id: id.into(), ok, detail: detail.into(), fix: if ok { None } else { fix.map(str::to_string) } }
}

/// 文件不存在：`None`；存在：`Some(权限位)`，Windows 没有权限位时是 `Some(None)`（访问控制靠数据目录的 DACL）。
fn mode_of(p: &Path) -> Option<Option<u32>> {
    std::fs::metadata(p).ok().map(|m| crate::sys::mode_bits(&m))
}

pub async fn run(paths: &Paths, cfg: &Config, ctx: Option<&Ctx>) -> Vec<DoctorCheck> {
    let mut out = Vec::new();

    // 1. config.toml
    let cfg_path = paths.config_toml();
    if !cfg_path.exists() {
        out.push(check(
            "config",
            false,
            format!("{} not found", cfg_path.display()),
            Some("anycon init --hub <url> --workspace <dir>"),
        ));
    } else {
        match Config::load(paths) {
            Ok(_) => out.push(check("config", true, format!("{} is valid", cfg_path.display()), None)),
            Err(e) => {
                out.push(check("config", false, e.to_string(), Some("fix the value, or run `anycon config edit`")))
            }
        }
    }

    // 2. keys
    let key = paths.node_key();
    match mode_of(&key) {
        None => out.push(check(
            "keys",
            false,
            format!("{} not found", key.display()),
            Some("anycon init --force  (this invalidates all paired devices)"),
        )),
        Some(Some(m)) if m & 0o077 != 0 => {
            let fix = format!("chmod 600 {}", key.display());
            out.push(check("keys", false, format!("{} has mode {m:o}; it must be 600", key.display()), Some(&fix)))
        }
        Some(bits) => {
            let tok = paths.device_token();
            match mode_of(&tok) {
                Some(Some(m)) if m & 0o077 != 0 => {
                    let fix = format!("chmod 600 {}", tok.display());
                    out.push(check(
                        "keys",
                        false,
                        format!("{} has mode {m:o}; it must be 600", tok.display()),
                        Some(&fix),
                    ))
                }
                _ if bits.is_none() => out.push(check("keys", true, "node.key present", None)),
                _ => out.push(check("keys", true, "node.key present, permissions 0600", None)),
            }
        }
    }

    // 3. hub 可达
    match cfg.hub.url.as_deref().filter(|u| !u.is_empty()) {
        None => out.push(check(
            "hub",
            false,
            "hub.url is not set",
            Some("anycon config set hub.url https://hub.example.com"),
        )),
        Some(url) => {
            let health = format!("{}/healthz", url.trim_end_matches('/'));
            match crate::hub::http_get(&health, Duration::from_secs(5)).await {
                Ok((200, _)) => {
                    let extra = ctx
                        .map(|c| c.hub_state.get())
                        .map(|s| if s.connected { ", connected".to_string() } else { ", not connected".to_string() })
                        .unwrap_or_default();
                    out.push(check("hub", true, format!("{health} -> 200{extra}"), None))
                }
                Ok((code, _)) => out.push(check(
                    "hub",
                    false,
                    format!("{health} -> HTTP {code}"),
                    Some("check the hub, its reverse proxy and anycon versions"),
                )),
                Err(e) => out.push(check(
                    "hub",
                    false,
                    format!("{health}: {e}"),
                    Some("check the network / reverse proxy; is hub.url correct?"),
                )),
            }
        }
    }

    // 4. 设备 token
    {
        let has_token = paths.device_token().exists();
        match ctx.map(|c| c.hub_state.get()) {
            Some(s) if s.connected && s.authenticated_with_token => {
                out.push(check("device_token", true, "authenticated with the hub", None))
            }
            Some(s) if has_token => {
                let why = s.auth_fail.or(s.last_error).unwrap_or_else(|| "not connected".into());
                out.push(check(
                    "device_token",
                    false,
                    format!("token present but the hub connection is not authenticated: {why}"),
                    Some("the device may have been revoked: `anycon init --force`, then pair again"),
                ))
            }
            _ if has_token => {
                out.push(check("device_token", true, "token present (daemon not running, not verified)", None))
            }
            _ => out.push(check(
                "device_token",
                false,
                "no device token yet (this node has not been paired)",
                Some("run `anycon pair` and enter the code in the web UI"),
            )),
        }
    }

    // 5. daemon 进程与 socket
    match ctx {
        Some(c) => out.push(check(
            "daemon",
            true,
            format!("running (pid {}), socket {}", std::process::id(), c.paths.sock().display()),
            None,
        )),
        None => {
            let alive = crate::local::connect(&paths.sock()).await.is_ok();
            out.push(check(
                "daemon",
                alive,
                if alive {
                    "socket reachable".to_string()
                } else {
                    "daemon is not running (socket not reachable)".to_string()
                },
                Some("anycon service start   (or run `anycon daemon` in a terminal)"),
            ));
        }
    }

    // 6. workspace
    {
        let root = cfg.workspace_root();
        let mut problems = Vec::new();
        if !root.is_dir() {
            problems.push(format!("{} is not a directory", root.display()));
        } else {
            let probe = root.join(format!(".anycon-doctor-{}", std::process::id()));
            match std::fs::write(&probe, b"x") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => problems.push(format!("{} is not writable: {e}", root.display())),
            }
            for d in [crate::workspace::TRASH_DIR, crate::workspace::SCRATCH_DIR] {
                if !root.join(d).is_dir() {
                    problems.push(format!("{d}/ is missing"));
                }
            }
        }
        if problems.is_empty() {
            out.push(check(
                "workspace",
                true,
                format!("{} (writable, .trash and .scratch present)", root.display()),
                None,
            ));
        } else {
            out.push(check(
                "workspace",
                false,
                problems.join("; "),
                Some("anycon init (creates the directories), or fix permissions"),
            ));
        }
    }

    // 7. 每个 adapter
    let env = match ctx {
        Some(c) => (*c.env).clone(),
        None => crate::env::resolve(cfg.agents.env_from_login_shell, Duration::from_secs(15)).await,
    };
    let infos: Vec<anycon_protocol::AgentInfo> = match ctx {
        Some(c) => c.agents.list().agents,
        None => offline_probe(paths, cfg, &env).await,
    };
    if infos.is_empty() {
        out.push(check(
            "agents",
            false,
            "no agent is configured",
            Some("anycon config set agents.claude-code.enabled true"),
        ));
    }
    for i in &infos {
        let ok = matches!(i.status, AgentStatus::Ready | AgentStatus::Disabled);
        let detail = match (&i.status, &i.issue) {
            (AgentStatus::Ready, _) => {
                let at = i.path.as_ref().filter(|p| !p.is_empty()).map(|p| format!(" at {p}")).unwrap_or_default();
                format!("{} {} ready ({} models){at}", i.label, i.version.clone().unwrap_or_default(), i.models.len())
            }
            (AgentStatus::Disabled, _) => format!("{} is disabled in config", i.label),
            (AgentStatus::Loading, _) => format!("{}: probe in progress", i.label),
            (s, Some(issue)) => format!("{}: {:?}: {}", i.label, s, issue.message),
            (s, None) => format!("{}: {:?}", i.label, s),
        };
        let fix = i.issue.as_ref().and_then(|x| x.hint.clone());
        out.push(DoctorCheck { id: format!("agent:{}", i.id), ok, detail, fix: if ok { None } else { fix } });
    }

    // 8. 登录 shell 的 PATH
    {
        use crate::env::EnvSource;
        let path = env.path();
        if !cfg.agents.env_from_login_shell {
            out.push(check(
                "login_shell_path",
                true,
                "agents.env_from_login_shell is off; using the process environment",
                None,
            ));
        } else if env.source == EnvSource::Process {
            out.push(check(
                "login_shell_path",
                false,
                format!(
                    "could not read the login shell environment ({}); PATH={}",
                    env.warning.clone().unwrap_or_default(),
                    path
                ),
                Some("set agents.<id>.command to an absolute path in config.toml"),
            ));
        } else {
            let missing: Vec<String> = infos
                .iter()
                .filter(|i| i.status == AgentStatus::Ready)
                .filter_map(|i| i.path.as_ref().map(|p| (i, p)))
                .filter(|(_, p)| {
                    Path::new(p.as_str()).parent().is_some_and(|d| !path.split(':').any(|x| Path::new(x) == d))
                })
                .map(|(i, p)| format!("{} ({p})", i.id))
                .collect();
            if missing.is_empty() {
                out.push(check(
                    "login_shell_path",
                    true,
                    format!("agents are on the login shell PATH ({})", env.source.as_str()),
                    None,
                ));
            } else {
                out.push(check(
                    "login_shell_path",
                    false,
                    format!("found outside the login shell PATH: {}", missing.join(", ")),
                    Some("launchd/systemd environments differ from your terminal; write the absolute path in agents.<id>.command"),
                ));
            }
        }
    }

    // 9. 服务
    match service_unit_path() {
        None if cfg!(windows) => out.push(match windows_task_installed().await {
            Some(true) => check("service", true, format!("scheduled task {WINDOWS_TASK} installed"), None),
            Some(false) => check(
                "service",
                false,
                format!("scheduled task {WINDOWS_TASK} not found"),
                Some("anycon service install"),
            ),
            None => check("service", true, "could not run schtasks", None),
        }),
        None => out.push(check("service", true, "service management is not supported on this platform", None)),
        Some(p) if p.exists() => out.push(check("service", true, format!("{} installed", p.display()), None)),
        Some(p) => {
            out.push(check("service", false, format!("{} not found", p.display()), Some("anycon service install")))
        }
    }

    // 10. 睡眠策略
    out.push(sleep_check().await);

    // 节点口令（ADR-36，docs/spec/cli.md § 5）：是否设置、是否被锁定、连续失败次数。没设口令只是提示（可选），不算失败；
    // 被锁定才失败。只有 daemon 在跑的时候读得到（记录在它打开的数据库里）
    out.push(enroll_password_check(ctx).await);

    // 11. 时钟
    match ctx.and_then(|c| c.hub_state.get().clock_skew_ms) {
        Some(skew) if skew.unsigned_abs() < 60_000 => {
            out.push(check("clock", true, format!("skew vs hub: {} ms", skew), None))
        }
        Some(skew) => {
            out.push(check("clock", false, format!("skew vs hub: {} s", skew / 1000), Some("enable network time sync")))
        }
        None => out.push(check("clock", true, "unknown (not connected to the hub)", None)),
    }

    // 12. 附件（blob）：个数、字节数和剩余磁盘。剩余空间低于底线时 daemon 不再收新的图片上传（`blob.put_begin` → `busy` / `disk_full`）。
    // 只有 daemon 在跑的时候才有数字（记录在它打开的数据库里）
    if let Some(c) = ctx {
        use crate::util::human_bytes;
        match c.blobs.stats().await {
            Ok(b) => {
                let ok = b.disk_free >= crate::blobs::DISK_RESERVE_BYTES;
                out.push(check(
                    "blobs",
                    ok,
                    format!(
                        "{} attachment(s), {} ({} not attached to a message yet); {} upload(s) in progress; {} free on the data volume",
                        b.count,
                        human_bytes(b.bytes),
                        human_bytes(b.unreferenced_bytes),
                        b.uploads,
                        human_bytes(b.disk_free)
                    ),
                    Some("free up disk space; image uploads are refused while less than 1 GiB is free"),
                ));
            }
            Err(e) => out.push(check("blobs", false, format!("cannot read the attachment records: {e}"), None)),
        }
    }
    out
}

/// 节点口令这一项（见上）。
async fn enroll_password_check(ctx: Option<&Ctx>) -> DoctorCheck {
    use anycon_protocol::EnrollPasswordState;
    let Some(c) = ctx else {
        return check("enroll_password", true, "not checked (the daemon is not running)", None);
    };
    let st = match c.enroll.status().await {
        Ok(s) => s.password,
        Err(e) => {
            return check("enroll_password", false, format!("cannot read the node password: {}", e.message), None)
        }
    };
    if !st.set {
        return check(
            "enroll_password",
            true,
            "no node password is set (optional). Without one, a new device can only join with a paired device's help; set one with `anycon passwd`",
            None,
        );
    }
    match st.state {
        EnrollPasswordState::Locked => check(
            "enroll_password",
            false,
            format!("password unlock is locked after {} consecutive failed attempts", st.failures),
            Some("look at `anycon audit` for enroll.failed first; if it was you who mistyped it, run `anycon passwd --enable`"),
        ),
        EnrollPasswordState::Disabled => {
            check("enroll_password", true, "a node password is set, but password unlock is turned off ([enroll].password = false)", None)
        }
        EnrollPasswordState::Enabled => {
            let mut detail = "a node password is set".to_string();
            if st.failures > 0 {
                detail.push_str(&format!("; {} consecutive failed attempt(s)", st.failures));
            }
            if let Some(ms) = st.retry_after_ms {
                detail.push_str(&format!("; the next attempt is allowed in {} s", ms.div_ceil(1000)));
            }
            check("enroll_password", true, detail, None)
        }
    }
}

async fn sleep_check() -> DoctorCheck {
    if !cfg!(target_os = "macos") {
        return check("sleep", true, "not applicable on this platform", None);
    }
    let out =
        tokio::time::timeout(Duration::from_secs(3), tokio::process::Command::new("pmset").arg("-g").output()).await;
    let Ok(Ok(o)) = out else { return check("sleep", true, "could not run pmset", None) };
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let val = |key: &str| -> Option<i64> {
        text.lines().find_map(|l| {
            l.split_whitespace().collect::<Vec<_>>().windows(2).find(|w| w[0] == key).and_then(|w| w[1].parse().ok())
        })
    };
    let sleep = val("sleep");
    let womp = val("womp");
    match (sleep, womp) {
        (Some(0), _) => check("sleep", true, "system sleep is disabled", None),
        (Some(m), Some(1)) => {
            check("sleep", true, format!("system sleeps after {m} min, but Wake for network access is on"), None)
        }
        (Some(m), _) => check(
            "sleep",
            false,
            format!("the machine sleeps after {m} min; it cannot receive messages while asleep"),
            Some("sudo pmset -a sleep 0   (or enable \"Wake for network access\")"),
        ),
        _ => check("sleep", true, "sleep policy unknown", None),
    }
}

/// daemon 没起来时：直接用 adapter 探测（一次完整探测，各 20 秒预算）。
async fn offline_probe(paths: &Paths, cfg: &Config, env: &crate::env::DaemonEnv) -> Vec<anycon_protocol::AgentInfo> {
    let scratch = paths.probe_scratch();
    let _ = std::fs::create_dir_all(&scratch);
    let mut out = Vec::new();
    for a in anycon_adapters::all() {
        let Some(ac) = cfg.adapter_config(a.id()) else { continue };
        if !ac.enabled {
            out.push(anycon_protocol::AgentInfo {
                id: a.id().into(),
                label: a.label().into(),
                driver: a.driver().into(),
                enabled: false,
                status: AgentStatus::Disabled,
                version: None,
                path: None,
                issue: None,
                models: vec![],
                allow_custom_model: false,
                modes: vec![],
                options: vec![],
                capabilities: Default::default(),
                probed_at_ms: crate::util::now_ms(),
            });
            continue;
        }
        let ctx = anycon_agent_api::ProbeContext {
            env: env.for_agent(cfg, a.id()),
            config: ac.clone(),
            deadline: Duration::from_secs(20),
            cheap: false,
            previous: None,
            scratch_dir: scratch.clone(),
        };
        match tokio::time::timeout(Duration::from_secs(25), a.probe(&ctx)).await {
            Ok(i) => out.push(i),
            Err(_) => out.push(anycon_protocol::AgentInfo {
                id: a.id().into(),
                label: a.label().into(),
                driver: a.driver().into(),
                enabled: true,
                status: AgentStatus::Error,
                version: None,
                path: None,
                issue: Some(anycon_protocol::AgentIssue {
                    code: "probe_timeout".into(),
                    message: "probe timed out".into(),
                    hint: None,
                }),
                models: vec![],
                allow_custom_model: false,
                modes: vec![],
                options: vec![],
                capabilities: Default::default(),
                probed_at_ms: crate::util::now_ms(),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_is_positive_for_tmp() {
        assert!(disk_free(&std::env::temp_dir()) > 0);
        assert_eq!(disk_free(Path::new("/definitely/not/here")), 0);
    }

    #[tokio::test]
    async fn offline_doctor_flags_missing_config_and_daemon() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        let mut cfg = Config::default();
        cfg.agents.env_from_login_shell = false;
        let checks = run(&paths, &cfg, None).await;
        let get =
            |id: &str| checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("missing check {id}: {checks:?}"));
        assert!(!get("config").ok && get("config").fix.as_deref().unwrap().contains("anycon init"));
        assert!(!get("keys").ok);
        assert!(!get("hub").ok);
        assert!(!get("daemon").ok);
        assert!(!get("device_token").ok);
        assert!(get("clock").ok);
        // 每项失败都给修复建议
        for c in checks.iter().filter(|c| !c.ok) {
            assert!(c.fix.is_some(), "check {} failed without a fix: {c:?}", c.id);
        }
    }
}
