//! 全部子命令的实现。

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anycon_daemon::config::{Config, Paths};
use anycon_protocol::*;
use serde_json::{json, Value};

use crate::cli::*;
use crate::conn::{connect_local, Conn};
use crate::error::{CliError, Result, EXIT_DENIED, EXIT_ERROR, EXIT_OFFLINE};
use crate::follow::{follow, FollowEnd, FollowOpts};
use crate::output::{ago, short_id, table, visible, Out};
use crate::remote::{self, HubState, NodeEntry};

pub struct App {
    pub out: Out,
    pub hub: Option<String>,
    pub node: Option<String>,
    pub local: bool,
    /// 单个请求的超时（`--timeout`）。
    pub timeout: Duration,
    pub paths: Paths,
}

fn device_name() -> String {
    anycon_daemon::config::hostname()
}

impl App {
    pub fn json(&self) -> bool {
        self.out.json
    }

    pub async fn local_conn(&self) -> Result<Conn> {
        let mut c = connect_local(&self.paths.home).await?;
        c.set_timeout(self.timeout);
        Ok(c)
    }

    /// 打开到某个远程节点的连接：IK 握手之后用 `node.info` 检查协议版本、顺便刷新本地保存的节点名。
    async fn open_remote(&self, h: &HubState, n: &NodeEntry) -> Result<Conn> {
        let mut c = remote::open(h, n, &device_name()).await?;
        c.set_timeout(self.timeout);
        let info: NodeInfo = c.call(methods::NODE_INFO, json!({})).await?;
        remote::check_protocol(info.protocol_version)?;
        let mut state = HubState::load(&h.dir, &h.hub_url);
        remote::refresh_node_name(&mut state, &n.id, &info.name);
        c.node_name = Some(crate::output::sanitize_label(&info.name, 64, &n.id));
        Ok(c)
    }

    fn hubs(&self) -> Vec<HubState> {
        let all = remote::list_hubs(&self.paths);
        match &self.hub {
            None => all,
            Some(h) => {
                let want = h.trim_end_matches('/');
                let host = remote::host_of(want).ok();
                all.into_iter()
                    .filter(|s| s.hub_url.trim_end_matches('/') == want || remote::host_of(&s.hub_url).ok() == host)
                    .collect()
            }
        }
    }

    /// 客户端命令的目标：本机 daemon 的 socket，或经 hub 的远程节点。
    pub async fn conn(&self) -> Result<Conn> {
        if self.local {
            return self.local_conn().await;
        }
        let hubs = self.hubs();
        if let Some(key) = &self.node {
            // 先看本机 daemon 是不是这个节点（是就走 socket，断网也能用）
            let local = self.local_conn().await.ok();
            let mut found: Option<(&HubState, &NodeEntry)> = None;
            for h in &hubs {
                if let Some(n) = h.find_node(key) {
                    found = Some((h, n));
                    break;
                }
            }
            if let Some(c) = local {
                if let Ok(info) = c.call::<NodeInfo>(methods::NODE_INFO, json!({})).await {
                    let is_it = match found {
                        Some((_, n)) => n.id == info.node_id,
                        None => info.name == *key || info.node_id.starts_with(key.as_str()),
                    };
                    if is_it {
                        return Ok(c);
                    }
                }
            }
            return match found {
                Some((h, n)) => self.open_remote(h, n).await,
                None => Err(CliError::not_paired(format!(
                    "no paired node matches {key:?}; see `anycon nodes`, or pair with `anycon connect`"
                ))),
            };
        }
        let local_err = match self.local_conn().await {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        let candidates: Vec<(&HubState, &NodeEntry)> =
            hubs.iter().flat_map(|h| h.nodes.iter().map(move |n| (h, n))).collect();
        match candidates.len() {
            0 => Err(local_err),
            1 => self.open_remote(candidates[0].0, candidates[0].1).await,
            _ => Err(CliError::usage("several nodes are paired; choose one with --node <name>")),
        }
    }
}

fn print_json_or(app: &App, v: &impl serde::Serialize, human: impl FnOnce() -> String) {
    if app.json() {
        app.out.json_line(v);
    } else {
        print!("{}", human());
    }
}

// ============================================================ init / daemon / service

fn ask(prompt: &str, default: &str) -> String {
    eprint!("{prompt} [{default}]: ");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    let t = s.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

pub async fn init(app: &App, a: InitArgs) -> Result<u8> {
    let interactive = !a.yes && std::io::stdin().is_terminal() && !app.json();
    let already = app.paths.config_toml().exists() || app.paths.node_key().exists();
    if already && a.force && interactive {
        let ans =
            ask("Re-initializing replaces this node's key and invalidates every paired device. Continue? (y/N)", "n");
        if !ans.eq_ignore_ascii_case("y") && !ans.eq_ignore_ascii_case("yes") {
            return Err(CliError::general("aborted"));
        }
    }
    let mut opts =
        anycon_daemon::setup::InitOptions { hub: a.hub, workspace: a.workspace, name: a.name, force: a.force };
    if interactive && !already {
        if opts.hub.is_none() {
            let h = ask("Hub URL (leave empty to set later)", "");
            opts.hub = Some(h).filter(|h| !h.is_empty());
        }
        if opts.workspace.is_none() {
            opts.workspace = Some(ask("Workspace directory", "~/anycon"));
        }
        if opts.name.is_none() {
            opts.name = Some(ask("Node name", &device_name()));
        }
    }
    let out = anycon_daemon::setup::init(&app.paths, &opts).await.map_err(|e| match e {
        anycon_daemon::setup::InitError::AlreadyInitialized(_) => CliError::new(EXIT_ERROR, "exists", e.to_string()),
        anycon_daemon::setup::InitError::Invalid(m) => CliError::usage(m),
        other => CliError::general(other.to_string()),
    })?;
    if app.json() {
        app.out.json_line(&json!({ "config": out.config_path, "workspace": out.workspace_root, "name": out.node_name, "reinitialized": out.reinitialized, "home": app.paths.home }));
    } else {
        println!("initialized {}", app.paths.home.display());
        println!("  config:    {}", out.config_path.display());
        println!("  workspace: {}", out.workspace_root.display());
        println!("  node name: {}", out.node_name);
        if out.reinitialized {
            println!("  note: every previously paired device must be paired again");
        }
        println!();
        println!("Next: `anycon daemon` (foreground) or `anycon service install`, then `anycon pair`.");
        println!("Optional: once the daemon is running, `anycon passwd` sets a node password. With it you can unlock this node from a new device when none of your paired devices is at hand.");
    }
    Ok(0)
}

pub async fn daemon(app: &App, _a: DaemonArgs) -> Result<u8> {
    let cfg = Config::load(&app.paths).map_err(|e| {
        if app.paths.config_toml().exists() {
            CliError::general(e.to_string())
        } else {
            CliError::not_initialized(format!(
                "{} is not initialized; run `anycon init` first",
                app.paths.home.display()
            ))
        }
    })?;
    let _guard = anycon_daemon::logging::init(&app.paths.logs_dir(), &cfg.log.level, cfg.log.retention_days, true);
    match anycon_daemon::run(app.paths.clone()).await {
        Ok(()) => Ok(0),
        Err(e) if e.is_not_initialized() => Err(CliError::not_initialized(e.to_string())),
        Err(e) => Err(CliError::general(e.to_string())),
    }
}

pub async fn service(app: &App, cmd: ServiceCmd) -> Result<u8> {
    let msg = match cmd {
        ServiceCmd::Install => crate::service::install(&app.paths.home).await?,
        ServiceCmd::Uninstall => crate::service::uninstall().await?,
        ServiceCmd::Start => crate::service::control("start").await?,
        ServiceCmd::Stop => crate::service::control("stop").await?,
        ServiceCmd::Restart => crate::service::control("restart").await?,
        ServiceCmd::Status => crate::service::status(&app.paths.home).await?,
    };
    if app.json() {
        app.out.json_line(&json!({ "message": msg }));
    } else {
        println!("{}", msg.trim_end());
    }
    Ok(0)
}

// ============================================================ 配对与设备

pub async fn pair(app: &App, a: PairArgs) -> Result<u8> {
    // `--enroll`：不是短码，而是 6 位接入码（账号内接入，ADR-36）
    if a.enroll {
        return crate::enroll::pair_enroll(app, a).await;
    }
    let conn = app.local_conn().await?;
    let before: NodeDevicesResult = conn.call(methods::NODE_DEVICES, json!({})).await?;
    // 已知设备再配对一次（同一个设备 id）也要能检测到：比较配对时间而不只是看有没有新 id
    let known: std::collections::HashMap<String, u64> =
        before.devices.iter().map(|d| (d.device_id.clone(), d.paired_at_ms)).collect();
    // 默认 16 位；`--short` 才是 8 位；`--long` 是无操作（旧脚本兼容）
    let long = !a.short;
    let r: NodePairBeginResult = match conn.call(methods::NODE_PAIR_BEGIN, json!({ "long": long })).await {
        Ok(r) => r,
        // `unsupported`：daemon 没连上 hub
        Err(e) if e.code == "unsupported" => return Err(CliError::offline(e.message)),
        // hub 拒绝了登记（每日配对次数用完、被限速……）：和之后的 `pair_failed` 同样处理
        Err(e) => {
            return Err(match pair_begin_refusal(&e) {
                Some(reason) => pair_failure_report(app, reason),
                None => e,
            })
        }
    };
    // 8 位短码只适合自建 hub：恶意 hub 拿到 match_id 之后可以离线穷举（即使用了 Argon2id 也只是更贵）
    if a.short {
        if let Some(w) = short_code_warning(r.link.split("/pair#").next().unwrap_or("")) {
            eprintln!("{}", app.out.err_yellow(&w));
        }
    }
    if app.json() {
        app.out.json_line(&json!({ "code": r.code, "link": r.link, "expires_at_ms": r.expires_at_ms }));
    } else {
        println!("Pairing code: {}", app.out.bold(&r.code));
        println!("Link:         {}", r.link);
        println!("Valid for 5 minutes. Enter the code in the web UI (or run `anycon connect <hub-url> --code {}` on another machine).", r.code);
        if a.qr {
            match qrcode::QrCode::new(r.link.as_bytes()) {
                Ok(qr) => {
                    let img = qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
                    println!("\n{img}\n");
                }
                Err(e) => eprintln!("cannot render a QR code: {e}"),
            }
        }
    }
    if a.no_wait {
        return Ok(0);
    }
    // 阻塞直到出现新设备（或已知设备重新配对）或码过期
    let deadline = std::time::Instant::now()
        + Duration::from_millis(r.expires_at_ms.saturating_sub(anycon_daemon::util::now_ms()).max(1000))
        + Duration::from_secs(2);
    let mut sigint = crate::output::interrupt_signal();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = sigint.recv() => return Err(CliError::interrupted()),
        }
        let now: NodeDevicesResult = conn.call(methods::NODE_DEVICES, json!({})).await?;
        if let Some(d) = now.devices.iter().find(|d| known.get(&d.device_id).is_none_or(|t| d.paired_at_ms > *t)) {
            // `--name`：给刚配对的设备起名
            let name = match &a.name {
                Some(n) => {
                    conn.call_raw(
                        anycon_daemon::local_methods::RENAME_DEVICE,
                        json!({ "device": d.device_id, "name": n }),
                    )
                    .await?;
                    n.clone()
                }
                None => d.name.clone(),
            };
            let name = crate::output::sanitize_label(&name, 100, &d.device_id);
            if app.json() {
                app.out.json_line(&json!({ "paired": { "device_id": d.device_id, "name": name, "kind": d.kind } }));
            } else {
                println!("Paired with {} ({:?}, {})", name, d.kind, short_id(&d.device_id));
            }
            return Ok(0);
        }
        // hub 拒绝了这次配对（设备配额满了、对端中途掉线……）：daemon 记下了结果，不必干等到短码过期。
        // 老版本的 daemon 没有这个本机方法：报错就当没有，退回到只等设备出现。
        let outcome = conn
            .call_raw(anycon_daemon::local_methods::PAIR_OUTCOME, json!({ "expires_at_ms": r.expires_at_ms }))
            .await
            .ok();
        if let Some(reason) = outcome.as_ref().and_then(|o| o["failed"]["reason"].as_str()) {
            return Err(pair_failure_report(app, reason));
        }
        if std::time::Instant::now() > deadline {
            return Err(CliError::general(
                "timed out waiting for the new device to join (the code is valid for 5 minutes)",
            ));
        }
    }
}

/// `node.pair_begin` 的错误是不是 hub 拒绝了登记：`busy`（`quota` / `rate_limited`）或 `forbidden`，`data.reason` 是
/// `pair_failed` 的原因。`unsupported`（daemon 没连上 hub）和别的错误不是。
fn pair_begin_refusal(e: &CliError) -> Option<&str> {
    match (e.code.as_str(), e.reason.as_deref()) {
        ("busy" | "forbidden", Some(reason)) => Some(reason),
        _ => None,
    }
}

/// 配对被 hub 拒绝：`--json` 时先在 stdout 打一行 `{"failed":{"reason","message"}}`，再返回给 `main` 打印的错误。
fn pair_failure_report(app: &App, reason: &str) -> CliError {
    let err = pair_failure_error(reason);
    if app.json() {
        app.out.json_line(&json!({ "failed": { "reason": reason, "message": err.message } }));
    }
    err
}

/// daemon 报告的配对失败原因（`pair_failed.reason` 的 snake_case 名字）→ 退出码和给用户的话。
/// 退出码按 docs/spec/cli.md § 4：配额、超时、限速、对端掉线、短码失效都是 1；账号不匹配（`forbidden`）是 5。
/// 错误码（`error: <code>: …` 里的 `<code>`）就是原因本身。
pub(crate) fn pair_failure_error(reason: &str) -> CliError {
    let retry = "run `anycon pair` again for a new code";
    let (exit, message) = match reason {
        "quota" => (
            EXIT_ERROR,
            "the hub refused the pairing: quota exceeded on the hub. This account has reached its limit on devices, nodes or daily pairing attempts; remove a device you no longer use (Settings → Devices in the web UI) or check which limit is full (Settings → Quota), then run `anycon pair` again".to_string(),
        ),
        "timeout" => (EXIT_ERROR, format!("pairing timed out: the new device did not finish in time; {retry}")),
        "peer_gone" => (EXIT_ERROR, format!("the new device disconnected during pairing; {retry}")),
        "rate_limited" => (
            EXIT_ERROR,
            "the hub rate-limited this pairing (too many attempts); wait a minute, then run `anycon pair` again".to_string(),
        ),
        "not_found" => (
            EXIT_ERROR,
            format!("the hub no longer knows this pairing code (it expired or was already used); {retry}"),
        ),
        "forbidden" => (
            EXIT_DENIED,
            "the hub refused this pairing (the devices belong to different accounts, or neither has one)".to_string(),
        ),
        other => (EXIT_ERROR, format!("the hub refused the pairing ({other}); {retry}")),
    };
    let code = if reason.chars().all(|c| c.is_ascii_lowercase() || c == '_') && !reason.is_empty() {
        reason
    } else {
        "pair_failed"
    };
    CliError::new(exit, code, message)
}

/// `--short` 配对时对 hub 地址的警告（本机 / 私网的自建 hub 不警告）。
pub(crate) fn short_code_warning(hub_url: &str) -> Option<String> {
    (!is_private_hub(hub_url)).then(|| {
        format!(
            "warning: an 8-character code is only suitable for a hub you run yourself; {hub_url} does not look like a local or private address. A hub you do not control could brute-force it offline. Drop --short to get a 16-character code."
        )
    })
}

/// 这个 hub 地址是不是 localhost / 回环 / 私网（8 位短码只适合这种自建 hub）。
/// 认的：`localhost`、`*.localhost`、`*.local`、127.0.0.0/8、::1、10/8、172.16/12、192.168/16、fc00::/7。
pub(crate) fn is_private_hub(hub_url: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let rest = hub_url.split_once("://").map(|x| x.1).unwrap_or(hub_url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|x| x.1).unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map(|x| x.0).unwrap_or(authority)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(IpAddr::V6(ip)) => {
            let unique_local = (ip.segments()[0] & 0xfe00) == 0xfc00;
            ip == Ipv6Addr::LOCALHOST
                || unique_local
                || ip.to_ipv4_mapped().is_some_and(|v4: Ipv4Addr| v4.is_loopback() || v4.is_private())
        }
        Err(_) => false,
    }
}

pub async fn devices(app: &App) -> Result<u8> {
    let conn = app.local_conn().await?;
    let r: NodeDevicesResult = conn.call(methods::NODE_DEVICES, json!({})).await?;
    print_json_or(app, &r, || {
        if r.devices.is_empty() {
            return "no paired devices; run `anycon pair`\n".into();
        }
        let mut rows = vec![vec![
            "NAME".into(),
            "KIND".into(),
            "ID".into(),
            "FINGERPRINT".into(),
            "PAIRED".into(),
            "LAST SEEN".into(),
            "ONLINE".into(),
        ]];
        for d in &r.devices {
            rows.push(vec![
                d.name.clone(),
                format!("{:?}", d.kind).to_lowercase(),
                short_id(&d.device_id),
                d.fingerprint.clone(),
                ago(d.paired_at_ms),
                d.last_seen_ms.map(ago).unwrap_or_else(|| "-".into()),
                if d.online { app.out.green("yes") } else { "no".into() },
            ]);
        }
        table(&rows)
    });
    Ok(0)
}

pub async fn revoke(app: &App, device: String) -> Result<u8> {
    let conn = app.local_conn().await?;
    let _: Empty = conn.call(methods::NODE_REVOKE_DEVICE, json!({ "device_id": device })).await?;
    if app.json() {
        app.out.json_line(&json!({ "revoked": device }));
    } else {
        println!("revoked {device}: its public key was removed here and the hub route was dropped");
    }
    Ok(0)
}

pub async fn rename_device(app: &App, device: String, name: String) -> Result<u8> {
    let conn = app.local_conn().await?;
    conn.call_raw(anycon_daemon::local_methods::RENAME_DEVICE, json!({ "device": device, "name": name })).await?;
    if app.json() {
        app.out.json_line(&json!({ "renamed": device, "name": name }));
    } else {
        println!("renamed {device} to {name}");
    }
    Ok(0)
}

// ============================================================ doctor / stats / version

pub async fn doctor(app: &App) -> Result<u8> {
    let (checks, conn) = match app.local_conn().await {
        Ok(c) => {
            let r: NodeDoctorResult = c.call_slow(methods::NODE_DOCTOR, json!({})).await?;
            (r.checks, Some(c))
        }
        Err(_) => {
            let cfg = Config::load(&app.paths).unwrap_or_default();
            (anycon_daemon::doctor::run(&app.paths, &cfg, None).await, None)
        }
    };
    let failed = checks.iter().filter(|c| !c.ok).count();
    if app.json() {
        app.out.json_line(&json!({ "ok": failed == 0, "checks": checks }));
    } else {
        for c in &checks {
            let mark = if c.ok { app.out.green("ok  ") } else { app.out.red("FAIL") };
            println!("{mark} {:<18} {}", c.id, c.detail);
            if let Some(f) = &c.fix {
                println!("     {}", app.out.yellow(&format!("fix: {f}")));
            }
        }
        // 有问题的 agent：附上诊断全文
        if let Some(conn) = &conn {
            for c in checks.iter().filter(|c| !c.ok && c.id.starts_with("agent:")) {
                let id = c.id.trim_start_matches("agent:");
                if let Ok(d) = conn.call_slow::<AgentDiagnoseResult>(methods::AGENT_DIAGNOSE, json!({ "id": id })).await
                {
                    println!("\n-- diagnosis: {id} --\n{}", d.text.trim_end());
                }
            }
        }
        println!(
            "\n{}",
            if failed == 0 { "all checks passed".to_string() } else { format!("{failed} check(s) failed") }
        );
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

pub async fn stats(app: &App) -> Result<u8> {
    let conn = app.conn().await?;
    let s: NodeStats = conn.call(methods::NODE_STATS, json!({})).await?;
    let status: Option<Value> =
        if conn.local { conn.call_raw(anycon_daemon::local_methods::STATUS, json!({})).await.ok() } else { None };
    if app.json() {
        app.out.json_line(&json!({ "stats": s, "status": status }));
        return Ok(0);
    }
    let up = s.uptime_ms / 1000;
    println!("uptime:           {}h {}m {}s", up / 3600, (up % 3600) / 60, up % 60);
    println!(
        "hub:              {}",
        if s.hub_connected { app.out.green("connected") } else { app.out.red("not connected") }
    );
    if let Some(h) = status.as_ref().map(|s| &s["hub"]) {
        if let Some(e) = h["last_error"].as_str() {
            println!("                  last error: {e}");
        }
    }
    println!("clients online:   {}", s.peers_online);
    println!(
        "sessions:         {} running, {} waiting permission, {} queued, {} idle, {} ended, {} error",
        s.sessions.running,
        s.sessions.waiting_permission,
        s.sessions.queued,
        s.sessions.idle,
        s.sessions.ended,
        s.sessions.error
    );
    println!("agent processes:  {}", s.agent_processes);
    println!("database:         {}", anycon_daemon::util::human_bytes(s.db_bytes));
    println!("disk free:        {}", anycon_daemon::util::human_bytes(s.disk_free_bytes));
    Ok(0)
}

pub fn version(app: &App) -> Result<u8> {
    let v = json!({ "version": env!("CARGO_PKG_VERSION"), "protocol_version": PROTOCOL_VERSION, "envelope_version": ENVELOPE_VERSION, "platform": anycon_daemon::hub::platform() });
    if app.json() {
        app.out.json_line(&v);
    } else {
        println!(
            "anycon {} (protocol {}, {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
            anycon_daemon::hub::platform()
        );
    }
    Ok(0)
}

// ============================================================ agents / projects

pub async fn agents(app: &App, a: AgentsArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let r: AgentListResult = if a.refresh {
        conn.call_slow(methods::AGENT_LIST, json!({ "refresh": true })).await?
    } else {
        conn.call(methods::AGENT_LIST, json!({ "refresh": false })).await?
    };
    if app.json() {
        app.out.json_line(&r);
        return Ok(0);
    }
    if r.agents.is_empty() {
        println!("no agents on this node; run `anycon doctor` to see why");
    }
    for ag in &r.agents {
        let status = format!("{:?}", ag.status).to_lowercase();
        let colored = if ag.status == AgentStatus::Ready { app.out.green(&status) } else { app.out.yellow(&status) };
        println!(
            "{} ({}) — {}{}",
            app.out.bold(&ag.id),
            ag.label,
            colored,
            ag.version.as_ref().map(|v| format!(" · {v}")).unwrap_or_default()
        );
        if let Some(p) = &ag.path {
            println!("  path:   {p}");
        }
        if let Some(i) = &ag.issue {
            println!("  issue:  {}: {}", i.code, i.message);
            if let Some(h) = &i.hint {
                println!("  hint:   {h}");
            }
        }
        if !ag.models.is_empty() {
            println!("  models:");
            for m in &ag.models {
                let thinking = m
                    .thinking
                    .as_ref()
                    .map(|t| {
                        format!(
                            "  thinking: {}{}",
                            t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>().join("/"),
                            t.default.as_ref().map(|d| format!(" (default {d})")).unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                println!("    {}{}{}", m.id, if m.default { " *" } else { "" }, thinking);
            }
        }
        if !ag.modes.is_empty() {
            println!(
                "  modes:  {}",
                ag.modes
                    .iter()
                    .map(|m| format!(
                        "{}{}{}",
                        m.id,
                        if m.default { "*" } else { "" },
                        if m.risk == ModeRisk::Dangerous { " (dangerous)" } else { "" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !ag.options.is_empty() {
            println!(
                "  options: {}",
                ag.options
                    .iter()
                    .map(|o| format!("{} ({:?})", o.id, o.kind).to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(0)
}

pub async fn projects(app: &App, a: ProjectsArgs) -> Result<u8> {
    let conn = app.conn().await?;
    match a.cmd {
        None => {
            let r: WorkspaceListResult = conn.call(methods::WORKSPACE_LIST, json!({})).await?;
            print_json_or(app, &r, || {
                if r.projects.is_empty() {
                    return format!(
                        "no projects in {}; create one with `anycon projects create <name>`, or link an existing directory with `anycon projects link <path>`\n",
                        r.root
                    );
                }
                let mut rows =
                    vec![vec!["PROJECT".into(), "GIT".into(), "SESSIONS".into(), "MODIFIED".into(), "LOCATION".into()]];
                for p in &r.projects {
                    let git = p
                        .git
                        .as_ref()
                        .map(|g| {
                            format!(
                                "{}{}",
                                g.branch.clone().unwrap_or_else(|| "-".into()),
                                if g.dirty { " *" } else { "" }
                            )
                        })
                        .unwrap_or_else(|| "-".into());
                    // 关联的目录标 linked 和路径；目录不在了再标 missing
                    let location = match (p.kind, p.missing) {
                        (ProjectKind::Folder, _) => String::new(),
                        (ProjectKind::Linked, false) => format!("linked: {}", p.path),
                        (ProjectKind::Linked, true) => format!("linked: {} (missing)", p.path),
                    };
                    rows.push(vec![
                        p.name.clone(),
                        git,
                        p.active_sessions.to_string(),
                        ago(p.modified_at_ms),
                        location,
                    ]);
                }
                table(&rows)
            });
        }
        Some(ProjectsCmd::Create { name, git }) => {
            let p: Project = conn.call(methods::WORKSPACE_CREATE, json!({ "name": name, "git_init": git })).await?;
            print_json_or(app, &p, || format!("created {} at {}\n", visible(&p.name), visible(&p.path)));
        }
        Some(ProjectsCmd::Rename { from, to }) => {
            let r: RenamedProject = conn.call(methods::WORKSPACE_RENAME, json!({ "from": from, "to": to })).await?;
            print_json_or(app, &r, || format!("renamed {} to {}\n", visible(&r.from), visible(&r.to)));
        }
        Some(ProjectsCmd::Trash { name }) => {
            let e: TrashEntry = conn.call(methods::WORKSPACE_TRASH, json!({ "name": name })).await?;
            print_json_or(app, &e, || {
                format!("moved {} to the trash ({})\n", visible(&e.original_name), visible(&e.id))
            });
        }
        Some(ProjectsCmd::Link { path, name }) => {
            let path = local_path_arg(&conn, &path)?;
            let p: Project = conn.call(methods::WORKSPACE_LINK, json!({ "path": path, "name": name })).await?;
            print_json_or(app, &p, || format!("linked {} -> {}\n", visible(&p.name), visible(&p.path)));
        }
        Some(ProjectsCmd::Unlink { name }) => {
            let _: Empty = conn.call(methods::WORKSPACE_UNLINK, json!({ "name": name })).await?;
            print_json_or(app, &Empty {}, || {
                format!("unlinked {} (the directory and its files are untouched)\n", visible(&name))
            });
        }
        Some(ProjectsCmd::Browse { path, all }) => {
            let path = path.map(|p| local_path_arg(&conn, &p)).transpose()?;
            let r: WorkspaceBrowseResult =
                conn.call(methods::WORKSPACE_BROWSE, json!({ "path": path, "show_hidden": all })).await?;
            print_json_or(app, &r, || render_browse(&r));
        }
        Some(ProjectsCmd::Clone { url, name }) => return clone(app, conn, url, name).await,
    }
    Ok(0)
}

/// 本机 daemon 时，相对路径按 CLI 当前目录转成绝对路径再发（daemon 的 cwd 不是用户的 cwd）；
/// 远程节点（Noise）时一律原样发，本机的当前目录对它没有意义，`~` 由节点展开。
fn local_path_arg(conn: &Conn, raw: &str) -> Result<String> {
    if conn.local {
        absolute_path_arg(raw)
    } else {
        Ok(raw.to_string())
    }
}

/// 绝对路径和 `~` / `~/…`（由 daemon 按它的 HOME 展开）原样返回；其余相对当前目录转成绝对路径（不解析符号链接，daemon 会 canonicalize）。
fn absolute_path_arg(raw: &str) -> Result<String> {
    if raw.starts_with('/') || raw == "~" || raw.starts_with("~/") {
        return Ok(raw.to_string());
    }
    let abs = std::path::absolute(raw)
        .map_err(|e| CliError::usage(format!("cannot resolve {raw:?} to an absolute path: {e}")))?;
    Ok(abs.to_string_lossy().into_owned())
}

/// 浏览时一个目录不能关联的原因，一句话。
fn block_note(b: LinkBlock) -> &'static str {
    match b {
        LinkBlock::Hidden => "hidden directory (list it in workspace.link_roots to allow)",
        LinkBlock::Sensitive => "protected system or credential directory, or one that contains them",
        LinkBlock::Workspace => "inside or containing the workspace",
        LinkBlock::DataDir => "anycon data directory",
        LinkBlock::FilesystemRoot => "filesystem root",
    }
}

/// `projects browse` 的人读输出：当前目录、能不能关联（不能时说明原因）、子目录列表（git 标记、已是项目的标出名字）。
/// 目录名和路径来自机器上的文件系统（可以带控制字符），一律可见化。
fn render_browse(r: &WorkspaceBrowseResult) -> String {
    let mut out = String::new();
    let state = match (&r.project, r.linkable, r.blocked) {
        (Some(p), _, _) => format!("already project {}", visible(p)),
        (None, true, _) => "linkable".to_string(),
        (None, false, Some(b)) => format!("not linkable: {}", block_note(b)),
        (None, false, None) => "not linkable".to_string(),
    };
    out.push_str(&format!("{}  ({state})\n", visible(&r.path)));
    if let Some(parent) = &r.parent {
        out.push_str(&format!("parent: {}\n", visible(parent)));
    }
    if r.entries.is_empty() {
        out.push_str("(no sub-directories)\n");
        return out;
    }
    let mut rows = Vec::new();
    for e in &r.entries {
        let note = match (&e.project, e.linkable, e.blocked) {
            (Some(p), _, _) => format!("project {p}"),
            (None, true, _) => String::new(),
            (None, false, Some(b)) => format!("not linkable: {}", block_note(b)),
            (None, false, None) => "not linkable".to_string(),
        };
        rows.push(vec![e.name.clone(), if e.git { "git".to_string() } else { String::new() }, note]);
    }
    out.push_str(&table(&rows));
    if r.truncated {
        out.push_str("(truncated: only the first entries are shown; give a deeper path)\n");
    }
    out.push_str("link one with: anycon projects link <path> [--name <name>]\n");
    out
}

async fn clone(app: &App, mut conn: Conn, url: String, name: Option<String>) -> Result<u8> {
    let mut notes = conn.notifications();
    let job: JobStarted = conn.call(methods::WORKSPACE_CLONE, json!({ "url": url, "name": name })).await?;
    let mut sigint = crate::output::interrupt_signal();
    let tty = std::io::stderr().is_terminal();
    loop {
        tokio::select! {
            n = notes.recv() => {
                let Some(n) = n else { return Err(CliError::offline("connection lost while cloning")) };
                match n.method.as_str() {
                    methods::JOB_PROGRESS => {
                        let Ok(p) = serde_json::from_value::<JobProgress>(n.params.clone()) else { continue };
                        if p.job_id != job.job_id { continue; }
                        if app.json() {
                            app.out.json_line(&n.params);
                        } else if tty {
                            eprint!("\r\x1b[2K{}{}", p.stage, p.percent.map(|x| format!(" {x:.0}%")).unwrap_or_default());
                            let _ = std::io::stderr().flush();
                        } else if let Some(l) = p.log_line {
                            eprintln!("{l}");
                        }
                    }
                    methods::JOB_DONE => {
                        let Ok(d) = serde_json::from_value::<JobDone>(n.params.clone()) else { continue };
                        if d.job_id != job.job_id { continue; }
                        if app.json() { app.out.json_line(&n.params); } else if tty { eprintln!(); }
                        return if d.ok {
                            if !app.json() { println!("cloned"); }
                            Ok(0)
                        } else {
                            Err(CliError::general(format!("clone failed: {}", d.error.unwrap_or_default())))
                        };
                    }
                    _ => {}
                }
            }
            _ = sigint.recv() => {
                let _ = conn.call::<Empty>(methods::JOB_CANCEL, json!({ "job_id": job.job_id })).await;
                return Err(CliError::interrupted());
            }
        }
    }
}

// ============================================================ 会话

/// 会话引用：完整 id、id 前缀（`sess_` 可省）、`last`。
pub(crate) async fn resolve_session(conn: &Conn, key: &str) -> Result<String> {
    if key.starts_with("sess_") && key.len() == 27 {
        return Ok(key.to_string());
    }
    let l: SessionListResult = conn.call(methods::SESSION_LIST, json!({ "include_archived": true })).await?;
    if key == "last" {
        return l
            .sessions
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| CliError::new(EXIT_ERROR, "not_found", "there are no sessions yet"));
    }
    let m: Vec<&SessionSummary> = l
        .sessions
        .iter()
        .filter(|s| s.id.starts_with(key) || s.id.strip_prefix("sess_").is_some_and(|r| r.starts_with(key)))
        .collect();
    match m.len() {
        1 => Ok(m[0].id.clone()),
        0 => Err(CliError::new(EXIT_ERROR, "not_found", format!("no session matches {key:?}"))),
        n => Err(CliError::usage(format!("{key:?} matches {n} sessions; use a longer prefix"))),
    }
}

pub async fn ls(app: &App, a: LsArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let r: SessionListResult =
        conn.call(methods::SESSION_LIST, json!({ "project": a.project, "include_archived": a.all })).await?;
    let want = |s: &SessionSummary| match a.status.as_deref() {
        None => true,
        Some("running") => matches!(s.status, SessionStatus::Running | SessionStatus::Starting),
        Some("waiting") => s.status == SessionStatus::WaitingPermission,
        Some("idle") => s.status == SessionStatus::Idle,
        Some("ended") => matches!(s.status, SessionStatus::Ended | SessionStatus::Interrupted),
        Some("error") => s.status == SessionStatus::Error,
        Some("queued") => s.status == SessionStatus::Queued,
        Some(_) => true,
    };
    if let Some(st) = a.status.as_deref() {
        if !matches!(st, "running" | "waiting" | "idle" | "ended" | "error" | "queued") {
            return Err(CliError::usage("--status must be one of running | waiting | idle | ended | error | queued"));
        }
    }
    let mut sessions: Vec<SessionSummary> = r.sessions.into_iter().filter(want).collect();
    if !a.all {
        sessions.truncate(20);
    }
    if app.json() {
        app.out.json_line(&SessionListResult { sessions });
        return Ok(0);
    }
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }
    // 有 worktree 的会话多一列 BRANCH
    let with_branch = sessions.iter().any(|s| s.worktree.is_some());
    let mut header: Vec<String> =
        ["ID", "STATUS", "PROJECT", "AGENT", "UPDATED", "TITLE"].iter().map(|h| h.to_string()).collect();
    if with_branch {
        header.insert(3, "BRANCH".into());
    }
    let mut rows = vec![header];
    for s in &sessions {
        let st = format!("{:?}", s.status).to_lowercase();
        let st =
            if s.pending_permissions > 0 { app.out.yellow(&format!("{st} ({})", s.pending_permissions)) } else { st };
        let title = crate::output::sanitize_label(&s.title, 60, "");
        let mut row = vec![
            short_id(&s.id),
            st,
            s.project.clone().unwrap_or_else(|| "(scratch)".into()),
            s.agent.clone(),
            ago(s.updated_at_ms),
            title,
        ];
        if with_branch {
            row.insert(3, crate::worktree_cmds::branch_cell(s));
        }
        rows.push(row);
    }
    print!("{}", table(&rows));
    Ok(0)
}

fn parse_options(items: &[String]) -> Result<serde_json::Map<String, Value>> {
    let mut m = serde_json::Map::new();
    for it in items {
        let (k, v) = it.split_once('=').ok_or_else(|| CliError::usage(format!("-o expects key=value, got {it:?}")))?;
        let val = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string()));
        m.insert(k.trim().to_string(), val);
    }
    Ok(m)
}

async fn pick_agent(conn: &Conn, wanted: Option<String>) -> Result<String> {
    if let Some(a) = wanted {
        return Ok(a);
    }
    let r: AgentListResult = conn.call(methods::AGENT_LIST, json!({})).await?;
    let ready: Vec<&AgentInfo> = r.agents.iter().filter(|a| a.status == AgentStatus::Ready).collect();
    match ready.len() {
        1 => Ok(ready[0].id.clone()),
        0 => Err(CliError::new(
            EXIT_ERROR,
            "agent_unavailable",
            format!(
                "no agent is ready ({}); see `anycon agents` and `anycon doctor`",
                r.agents
                    .iter()
                    .map(|a| format!("{}: {:?}", a.id, a.status).to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
        _ => Err(CliError::usage(format!(
            "several agents are ready ({}); choose one with --agent",
            ready.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn end_code(end: &FollowEnd) -> u8 {
    match end {
        FollowEnd::TurnDone(TurnDoneReason::Completed | TurnDoneReason::MaxTurns | TurnDoneReason::Budget) => 0,
        FollowEnd::TurnDone(_) => 1,
        FollowEnd::SessionEnded(_) | FollowEnd::Deleted => 1,
        FollowEnd::Disconnected => EXIT_OFFLINE,
    }
}

pub async fn new_session(app: &App, a: NewArgs) -> Result<u8> {
    let mut conn = app.conn().await?;
    // 工作位置（ADR-34）：--worktree / --here / --base / --branch；都不给就按节点的 worktree.default
    let choice = crate::worktree_cmds::choose_workspace(&conn, &a).await?;
    let agent = pick_agent(&conn, a.agent).await?;
    let settings = SessionSettings {
        model: a.model,
        thinking: a.thinking,
        mode: a.mode,
        options: parse_options(&a.options)?,
        ..Default::default()
    };
    let message_id = uuid::Uuid::new_v4().to_string();
    let mut notes = conn.notifications();
    let mut params = json!({ "project": a.project, "agent": agent, "prompt": a.prompt, "message_id": message_id, "settings": settings });
    if let Some(w) = &choice.workspace {
        params["workspace"] = w.clone();
    }
    let created: Result<SessionSummary> = conn.call(methods::SESSION_CREATE, params.clone()).await;
    let s: SessionSummary = match created {
        // 按节点默认值选的 worktree，而项目开不了（不是仓库根目录 / 没有提交）：退回项目目录，不算错
        Err(e) if choice.defaulted && crate::worktree_cmds::is_not_worktree_ready(&e) => {
            if !app.json() {
                eprintln!(
                    "{}",
                    app.out.err_dim("(this project cannot have a worktree; working in the project directory)")
                );
            }
            params.as_object_mut().map(|m| m.remove("workspace"));
            conn.call(methods::SESSION_CREATE, params).await?
        }
        other => other?,
    };
    if a.no_wait {
        if app.json() {
            app.out.json_line(&s);
        } else {
            println!("{}", s.id);
        }
        return Ok(0);
    }
    if !app.json() {
        eprintln!("{}", app.out.err_dim(&format!("session {}", s.id)));
    }
    let end = follow(
        &conn,
        &mut notes,
        FollowOpts {
            session_id: s.id.clone(),
            show_thinking: a.show_thinking,
            auto_approve: a.auto_approve,
            out: app.out,
            start_seq: 0,
            wait_message: Some(message_id),
        },
    )
    .await?;
    Ok(end_code(&end))
}

pub async fn send(app: &App, a: SendArgs) -> Result<u8> {
    // 图片先在本地看一眼（文件头、大小、张数）：明显不对的不必连节点
    let images = crate::attach::load_images(&a.images)?;
    for advice in images.iter().filter_map(crate::attach::LocalImage::advice) {
        eprintln!("{}", app.out.err_yellow(&advice));
    }
    let mut conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    let before: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    let archived = || {
        CliError::usage(format!(
            "session {} is archived; run `anycon unarchive {}` first, then send again",
            short_id(&sid),
            short_id(&sid)
        ))
    };
    // 已归档的会话收不了消息：别先把图传上去再丢掉
    if before.archived && !images.is_empty() {
        return Err(archived());
    }
    // 经 blob 通道传图（ADR-35），消息只带 daemon 返回的引用
    let attachments = crate::attach::upload_all(&conn, &images).await?;
    let message_id = uuid::Uuid::new_v4().to_string();
    let mut notes = conn.notifications();
    let mut params = json!({ "session_id": sid, "message_id": message_id, "content": a.text.unwrap_or_default() });
    if !attachments.is_empty() {
        params["attachments"] = serde_json::to_value(&attachments)?;
    }
    let sent: Result<SessionSendResult> = conn.call(methods::SESSION_SEND, params).await;
    if sent.is_err() {
        // 没发出去的图没人引用：丢掉（发出去了只是没收到答复的，daemon 回 in_use，不影响）
        crate::attach::discard_all(&conn, &attachments).await;
    }
    // 已归档的会话不能直接发消息：告诉用户怎么办（退出码 2，和别的参数 / 状态错误一致）
    let r: SessionSendResult = sent.map_err(|e| if crate::worktree_cmds::is_archived(&e) { archived() } else { e })?;
    if a.no_wait {
        if app.json() {
            app.out.json_line(&r);
        } else {
            println!("{}", if r.queued { "queued" } else { "sent" });
        }
        return Ok(0);
    }
    let end = follow(
        &conn,
        &mut notes,
        FollowOpts {
            session_id: sid,
            show_thinking: a.show_thinking,
            auto_approve: a.auto_approve,
            out: app.out,
            start_seq: before.last_seq,
            wait_message: Some(message_id),
        },
    )
    .await?;
    Ok(end_code(&end))
}

/// attach 的起点：正在跑的会话回放当前 turn（从触发它的用户消息开始），空闲会话从现在开始。
/// 历史页按字节封顶，一页可能装不下整个 turn：往前翻几页再放弃。
async fn attach_start(conn: &Conn, s: &SessionSummary) -> Result<u64> {
    if !matches!(
        s.status,
        SessionStatus::Running | SessionStatus::Starting | SessionStatus::WaitingPermission | SessionStatus::Queued
    ) {
        return Ok(s.last_seq);
    }
    let mut before = s.last_seq + 1;
    let mut turn_start: Option<u64> = None;
    for _ in 0..8 {
        let h: SessionHistoryResult = conn
            .call(methods::SESSION_HISTORY, json!({ "session_id": s.id, "before_seq": before, "limit": 1000 }))
            .await?;
        if turn_start.is_none() {
            turn_start = h.events.iter().rev().find(|e| matches!(e.event, Event::TurnStart { .. })).map(|e| e.seq);
        }
        if let Some(ts) = turn_start {
            if let Some(um) = h.events.iter().rev().find(|e| e.seq < ts && matches!(e.event, Event::UserMessage { .. }))
            {
                return Ok(um.seq.saturating_sub(1));
            }
            // 用户消息在更早的页里：继续往前
        }
        let Some(first) = h.events.first().map(|e| e.seq) else { break };
        if !h.has_more || first >= before {
            break;
        }
        before = first;
    }
    Ok(turn_start.map(|ts| ts.saturating_sub(1)).unwrap_or(s.last_seq))
}

pub async fn attach(app: &App, a: AttachArgs) -> Result<u8> {
    let mut conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    let s: SessionSummary = conn.call(methods::SESSION_GET, json!({ "session_id": sid })).await?;
    let start = match a.since {
        Some(n) => n,
        None => attach_start(&conn, &s).await?,
    };
    if !app.json() {
        eprintln!(
            "{}",
            app.out.err_dim(&format!(
                "attached to {} — {} [{}] (Ctrl+C detaches; it does not cancel)",
                short_id(&s.id),
                if s.title.is_empty() { "(untitled)" } else { &s.title },
                format!("{:?}", s.status).to_lowercase()
            ))
        );
    }
    let mut notes = conn.notifications();
    let end = follow(
        &conn,
        &mut notes,
        FollowOpts {
            session_id: sid,
            show_thinking: a.show_thinking,
            auto_approve: a.auto_approve,
            out: app.out,
            start_seq: start,
            wait_message: None,
        },
    )
    .await?;
    Ok(if matches!(end, FollowEnd::Disconnected) { EXIT_OFFLINE } else { 0 })
}

pub async fn simple_session_op(app: &App, method: &'static str, session: String, done: &str) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &session).await?;
    let _: Empty = conn.call(method, json!({ "session_id": sid })).await?;
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "ok": true }));
    } else {
        println!("{done} {}", short_id(&sid));
    }
    Ok(0)
}

pub async fn rename_session(app: &App, session: String, title: String) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &session).await?;
    let _: Empty = conn.call(methods::SESSION_RENAME, json!({ "session_id": sid, "title": title })).await?;
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "title": title }));
    } else {
        println!("renamed {}", short_id(&sid));
    }
    Ok(0)
}

pub async fn promote(app: &App, session: String, name: String) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &session).await?;
    let _: Empty = conn.call(methods::SESSION_PROMOTE, json!({ "session_id": sid, "name": name })).await?;
    if app.json() {
        app.out.json_line(&json!({ "session_id": sid, "project": name }));
    } else {
        println!("promoted {} to project {name}", short_id(&sid));
    }
    Ok(0)
}

pub async fn history(app: &App, a: HistoryArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let sid = resolve_session(&conn, &a.session).await?;
    let mut after = a.since;
    let mut r = crate::render::Renderer::new(app.out, a.show_thinking);
    loop {
        let before = after;
        let page: SessionHistoryResult =
            conn.call(methods::SESSION_HISTORY, json!({ "session_id": sid, "after_seq": after, "limit": 500 })).await?;
        for ev in &page.events {
            after = ev.seq;
            if app.json() {
                app.out.json_line(ev);
            } else {
                r.event(ev);
            }
        }
        if !page.has_more {
            break;
        }
        // 防死循环：节点说还有更多、页却没有带来新的事件
        if after == before {
            return Err(CliError::general("the node reports more history but returned no new events; giving up"));
        }
    }
    Ok(0)
}

pub async fn search(app: &App, a: SearchArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let r: SessionSearchResult =
        conn.call(methods::SESSION_SEARCH, json!({ "query": a.query, "project": a.project })).await?;
    print_json_or(app, &r, || {
        if r.hits.is_empty() {
            return "no matches\n".into();
        }
        let mut s = String::new();
        for h in &r.hits {
            // 片段和标题是会话里的文字（agent 说的、工具输出的）：控制字符先显示成转义，再换上我们自己的高亮
            let snippet = crate::output::safe_text(&h.snippet)
                .replace("<mark>", if app.out.color { "\x1b[1;33m" } else { "" })
                .replace("</mark>", if app.out.color { "\x1b[0m" } else { "" })
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&");
            s.push_str(&format!(
                "{}  #{}  {}\n    {}\n",
                short_id(&h.session_id),
                h.seq,
                crate::output::sanitize_label(&h.title, 80, "(untitled)"),
                snippet.replace('\n', " ")
            ));
        }
        s
    });
    Ok(0)
}

pub async fn cancel(app: &App, session: String) -> Result<u8> {
    simple_session_op(app, methods::SESSION_CANCEL, session, "cancelled").await
}

// ============================================================ 权限

pub async fn pending(app: &App) -> Result<u8> {
    let conn = app.conn().await?;
    let r: PermissionListResult = conn.call(methods::PERMISSION_LIST, json!({})).await?;
    print_json_or(app, &r, || {
        if r.requests.is_empty() {
            return "no pending requests\n".into();
        }
        let mut rows = vec![vec!["REQUEST".into(), "SESSION".into(), "KIND".into(), "WAITING".into(), "TITLE".into()]];
        for q in &r.requests {
            rows.push(vec![
                q.request_id.clone(),
                short_id(&q.session_id),
                format!("{:?}", q.kind).to_lowercase(),
                ago(q.created_at_ms),
                // 标题是 agent 给的文本：控制字符显示成转义，保持单行
                crate::output::visible(&q.title).replace('\n', " "),
            ]);
        }
        table(&rows)
    });
    Ok(0)
}

async fn find_request(conn: &Conn, key: &str) -> Result<PermissionRequest> {
    let r: PermissionListResult = conn.call(methods::PERMISSION_LIST, json!({})).await?;
    let m: Vec<&PermissionRequest> = r
        .requests
        .iter()
        .filter(|q| q.request_id == key || (!key.is_empty() && q.request_id.starts_with(key)))
        .collect();
    match m.len() {
        1 => Ok(m[0].clone()),
        0 => Err(CliError::new(
            EXIT_ERROR,
            "not_found",
            format!("no pending request matches {key:?} (it may already be answered)"),
        )),
        n => Err(CliError::usage(format!("{key:?} matches {n} requests; use a longer prefix"))),
    }
}

pub async fn approve(app: &App, a: ApproveArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let req = find_request(&conn, &a.request_id).await?;
    // 默认只批准「这一次」；要整个会话都允许得显式 `--session`。没有对应选项时报错，绝不悄悄换成范围更大的授权
    let want = if a.session { PermissionOptionKind::AllowAlways } else { PermissionOptionKind::AllowOnce };
    let opt = req.options.iter().find(|o| o.kind == want).ok_or_else(|| {
        CliError::usage(if a.session {
            "this request has no \"allow for the session\" option"
        } else if req.options.iter().any(|o| o.kind == PermissionOptionKind::AllowAlways) {
            "this request has no one-time allow option; use `anycon approve --session` to allow it for the whole session, or answer it with `anycon attach`"
        } else {
            "this request has no allow option (a question needs `anycon attach`)"
        })
    })?;
    let _: Empty =
        conn.call(methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": opt.id })).await?;
    if app.json() {
        app.out.json_line(&json!({ "request_id": req.request_id, "option_id": opt.id }));
    } else {
        println!("approved ({})", opt.label);
    }
    Ok(0)
}

pub async fn deny(app: &App, a: DenyArgs) -> Result<u8> {
    let conn = app.conn().await?;
    let req = find_request(&conn, &a.request_id).await?;
    let opt = crate::render::first_reject(&req).ok_or_else(|| CliError::usage("this request has no deny option"))?;
    let _: Empty = conn
        .call(
            methods::PERMISSION_RESPOND,
            json!({ "request_id": req.request_id, "option_id": opt.id, "message": a.message }),
        )
        .await?;
    if app.json() {
        app.out.json_line(&json!({ "request_id": req.request_id, "option_id": opt.id }));
    } else {
        println!("denied");
    }
    Ok(0)
}

// ============================================================ audit / config / logs

fn parse_since(s: &str) -> Result<u64> {
    if let Ok(ms) = s.parse::<u64>() {
        if ms > 10_000_000_000 {
            return Ok(ms);
        }
    }
    let d = anycon_daemon::util::parse_duration(s)
        .map_err(|e| CliError::usage(format!("--since: {e} (use e.g. 1h, 2d, or Unix milliseconds)")))?;
    Ok(anycon_daemon::util::now_ms().saturating_sub(d.as_millis() as u64))
}

pub async fn audit(app: &App, a: AuditArgs) -> Result<u8> {
    let conn = app.local_conn().await?;
    let since = a.since.as_deref().map(parse_since).transpose()?;
    let r: NodeAuditResult =
        conn.call(methods::NODE_AUDIT, json!({ "since_ms": since, "device_id": a.device, "limit": a.limit })).await?;
    print_json_or(app, &r, || {
        if r.entries.is_empty() {
            return "no audit entries\n".into();
        }
        let mut rows = vec![vec!["TIME".into(), "DEVICE".into(), "ACTION".into(), "TARGET".into(), "DETAIL".into()]];
        for e in &r.entries {
            let detail = if e.detail.is_null() || e.detail == json!({}) {
                String::new()
            } else {
                e.detail.to_string().chars().take(80).collect()
            };
            rows.push(vec![
                anycon_daemon::util::fmt_utc(e.ts_ms),
                e.device_name.clone(),
                e.action.clone(),
                e.target.clone().map(|t| short_id(&t)).unwrap_or_default(),
                detail,
            ]);
        }
        table(&rows)
    });
    Ok(0)
}

/// 配置写入失败的退出码：值 / 键不合法 = 2（参数错误）；读写文件失败、锁超时等 = 1（一般错误）。
fn config_error(e: anycon_daemon::config::ConfigError) -> CliError {
    use anycon_daemon::config::ConfigError;
    match e {
        ConfigError::Parse(_) | ConfigError::Invalid(_) => CliError::usage(e.to_string()),
        ConfigError::Io(_) => CliError::general(e.to_string()),
    }
}

pub async fn config(app: &App, cmd: ConfigCmd) -> Result<u8> {
    match cmd {
        ConfigCmd::Path => {
            if app.json() {
                app.out.json_line(&json!({ "path": app.paths.config_toml() }));
            } else {
                println!("{}", app.paths.config_toml().display());
            }
            Ok(0)
        }
        ConfigCmd::Get { key } => {
            let cfg = Config::load(&app.paths).map_err(|e| CliError::general(e.to_string()))?;
            let v = anycon_daemon::config::get_value(&cfg, &key)
                .ok_or_else(|| CliError::new(EXIT_ERROR, "not_found", format!("no such config key: {key}")))?;
            if app.json() {
                app.out.json_line(&json!({ "key": key, "value": v }));
            } else {
                match v {
                    Value::String(s) => println!("{s}"),
                    other => println!("{other}"),
                }
            }
            Ok(0)
        }
        ConfigCmd::Set { key, value } => {
            anycon_daemon::config::set_value_blocking(&app.paths, &key, &value).await.map_err(config_error)?;
            let hot = anycon_daemon::config::is_hot_key(&key);
            // daemon 在跑就让它立刻重读（它自己也会在 1 秒内轮询到）；返回里带它对 `link_roots` 的判定
            let reloaded = match app.local_conn().await {
                Ok(c) => c.call_raw(anycon_daemon::local_methods::CONFIG_RELOAD, json!({})).await.ok(),
                Err(_) => None,
            };
            let running = reloaded.is_some();
            let unusable: Vec<Value> = if key == "workspace.link_roots" {
                reloaded
                    .as_ref()
                    .and_then(|r| r.get("link_roots_unusable"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let note = if !running {
                "saved (the daemon is not running; it will read this at the next start)"
            } else if hot {
                "saved and applied"
            } else {
                "saved; this key needs a daemon restart to take effect (`anycon service restart`)"
            };
            if app.json() {
                let mut out = json!({ "key": key, "applied": running && hot, "restart_required": running && !hot });
                if key == "workspace.link_roots" {
                    out["link_roots_unusable"] = Value::Array(unusable);
                }
                app.out.json_line(&out);
            } else {
                println!("{note}");
                for line in link_roots_notes(&unusable) {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        ConfigCmd::Edit => {
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let parts = shell_words_split(&editor);
            let (prog, args) = parts.split_first().ok_or_else(|| CliError::general("$EDITOR is empty"))?;
            let status = std::process::Command::new(prog)
                .args(args)
                .arg(app.paths.config_toml())
                .status()
                .map_err(|e| CliError::general(format!("cannot run {editor}: {e}")))?;
            if !status.success() {
                return Err(CliError::general(format!("{editor} exited with {status}")));
            }
            match Config::load(&app.paths) {
                Ok(_) if app.json() => app.out.json_line(&json!({ "path": app.paths.config_toml(), "valid": true })),
                Ok(_) => println!("config.toml is valid"),
                Err(e) => return Err(CliError::general(format!("config.toml is now invalid: {e}"))),
            }
            Ok(0)
        }
    }
}

/// `config set workspace.link_roots …` 之后 daemon 判定为不能用的项（被忽略的，或目录不存在的），每项一行带原因。
fn link_roots_notes(unusable: &[Value]) -> Vec<String> {
    unusable
        .iter()
        .map(|r| {
            let entry = r.get("entry").and_then(Value::as_str).unwrap_or_default();
            let reason = r.get("reason").and_then(Value::as_str).unwrap_or_default();
            let word =
                if r.get("missing").and_then(Value::as_bool).unwrap_or(false) { "not usable" } else { "ignored" };
            format!("warning: workspace.link_roots entry {entry:?} is {word}: {reason}")
        })
        .collect()
}

fn shell_words_split(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// `logs -f` 的跟随状态：按**字节偏移**跟踪当前日志文件，只吐出完整的行。
///
/// - 偏移加的是实际读到的字节数（不是之前 `metadata().len()` 的值）：读的时候文件又长了也不会重复输出；
/// - 最后一行还没写完（没有换行）就先留着，等写完再输出；
/// - 日志轮转（出现了更新的文件）时，先把旧文件剩下的读干净再切到新文件，不丢旧文件的尾部；
/// - 文件被截短（偏移超过长度）就从头读。
struct LogTail {
    path: std::path::PathBuf,
    pos: u64,
    partial: Vec<u8>,
}

impl LogTail {
    /// 读 `path` 里 `pos` 之后新增的内容，吐出完整的行。
    fn drain(&mut self) -> Vec<String> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else { return vec![] };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
            self.partial.clear();
        }
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return vec![];
        }
        let mut buf = Vec::new();
        let Ok(n) = f.read_to_end(&mut buf) else { return vec![] };
        self.pos += n as u64;
        self.partial.extend_from_slice(&buf);
        let Some(last_nl) = self.partial.iter().rposition(|b| *b == b'\n') else { return vec![] };
        let rest = self.partial.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        String::from_utf8_lossy(&complete).lines().map(str::to_string).collect()
    }

    /// 一次轮询：新增的完整行；如果出现了更新的日志文件，旧文件剩下的先输出（包括没写完的最后一行），再切过去。
    fn poll(&mut self, dir: &std::path::Path) -> Vec<String> {
        let mut out = self.drain();
        if let Some(newest) = anycon_daemon::logging::latest_log(dir) {
            if newest != self.path {
                if !self.partial.is_empty() {
                    out.push(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned());
                }
                self.path = newest;
                self.pos = 0;
                out.extend(self.drain());
            }
        }
        out
    }
}

pub async fn logs(app: &App, a: LogsArgs) -> Result<u8> {
    let dir = app.paths.logs_dir();
    let Some(path) = anycon_daemon::logging::latest_log(&dir) else {
        return Err(CliError::new(EXIT_ERROR, "not_found", format!("no log files in {}", dir.display())));
    };
    // 日志可能含非 UTF-8 字节：宽松解码，别因此失败
    let bytes = std::fs::read(&path)?;
    let pos = bytes.len() as u64;
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let emit = |l: &str| {
        if app.json() {
            app.out.json_line(&json!({ "line": l }));
        } else {
            println!("{l}");
        }
    };
    for l in &lines[lines.len().saturating_sub(a.lines)..] {
        emit(l);
    }
    if !a.follow {
        // 最后一行没写完也要给出来
        if end < bytes.len() {
            emit(&String::from_utf8_lossy(&bytes[end..]));
        }
        return Ok(0);
    }
    let mut tail = LogTail { path, pos, partial: bytes[end..].to_vec() };
    let mut sigint = crate::output::interrupt_signal();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = sigint.recv() => return Err(CliError::interrupted()),
        }
        for l in tail.poll(&dir) {
            emit(&l);
        }
        let _ = std::io::stdout().flush();
    }
}

// ============================================================ 客户端：connect / nodes / disconnect

pub async fn connect(app: &App, a: ConnectArgs) -> Result<u8> {
    // 不带 `--code`：账号内接入（登录账号、选节点、选方式，ADR-36）
    let Some(code) = a.code.clone() else {
        return crate::enroll::connect(app, a).await;
    };
    let name = a.name.unwrap_or_else(device_name);
    let node = remote::pair(&app.paths, &a.hub_url, &code, &name).await?;
    if app.json() {
        app.out.json_line(&json!({ "node": { "id": node.id, "name": node.name }, "hub": a.hub_url }));
    } else {
        println!("Paired with node {} ({}) via {}", node.name, short_id(&node.id), a.hub_url);
        println!("Try: `anycon --node {} agents`", node.name);
    }
    Ok(0)
}

pub async fn nodes(app: &App) -> Result<u8> {
    let hubs = app.hubs();
    let local_id = match app.local_conn().await {
        Ok(c) => c.call::<NodeInfo>(methods::NODE_INFO, json!({})).await.ok().map(|i| i.node_id),
        Err(_) => None,
    };
    let mut rows = vec![vec!["NODE".into(), "ID".into(), "HUB".into(), "STATUS".into()]];
    let mut jv = Vec::new();
    for h in &hubs {
        if let Some(p) = &h.problem {
            eprintln!(
                "{}",
                app.out.err_yellow(&format!("warning: the saved state for {} is unusable: {p}", h.hub_url))
            );
        }
        let online = remote::online_nodes(h, &device_name()).await;
        for n in &h.nodes {
            let shown = crate::output::sanitize_label(&n.name, 64, &n.id);
            let st = match &online {
                Some(list) if list.contains(&n.id) => "online",
                Some(_) => "offline",
                None => "unknown (hub unreachable)",
            };
            let is_local = local_id.as_deref() == Some(n.id.as_str());
            rows.push(vec![
                shown.clone(),
                short_id(&n.id),
                h.hub_url.clone(),
                format!("{st}{}", if is_local { " · this machine" } else { "" }),
            ]);
            jv.push(json!({ "id": n.id, "name": shown, "hub": h.hub_url, "status": st, "local": is_local }));
        }
    }
    if app.json() {
        app.out.json_line(&json!({ "nodes": jv }));
    } else if rows.len() == 1 {
        println!("no paired nodes; pair with `anycon connect <hub-url> --code <code>`");
    } else {
        print!("{}", table(&rows));
    }
    Ok(0)
}

pub async fn disconnect(app: &App, node: String) -> Result<u8> {
    for mut h in app.hubs() {
        if let Some(n) = h.find_node(&node).cloned() {
            h.nodes.retain(|x| x.id != n.id);
            h.save()?;
            if app.json() {
                app.out.json_line(&json!({ "disconnected": n.id }));
            } else {
                println!("removed the local pairing with {} (the node still lists this device; revoke it there with `anycon revoke`)", n.name);
            }
            return Ok(0);
        }
    }
    Err(CliError::not_paired(format!("no paired node matches {node:?}")))
}

pub fn completions(shell: clap_complete::Shell) -> Result<u8> {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "anycon", &mut std::io::stdout());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_failure_reported_by_the_daemon_maps_to_an_exit_code_a_code_and_a_message() {
        // 退出码按 docs/spec/cli.md § 4：配额 / 超时 / 限速 / 对端掉线 / 短码失效 → 1；账号不匹配 → 5
        for (reason, exit, words) in [
            ("quota", 1, "quota exceeded on the hub"),
            ("timeout", 1, "timed out"),
            ("peer_gone", 1, "disconnected"),
            ("rate_limited", 1, "rate-limited"),
            ("not_found", 1, "no longer knows"),
            ("forbidden", 5, "different accounts"),
        ] {
            let e = pair_failure_error(reason);
            assert_eq!((e.exit, e.code.as_str()), (exit, reason), "{reason}");
            assert!(e.message.contains(words), "{reason}: {}", e.message);
        }
        let quota = pair_failure_error("quota").message;
        assert!(quota.contains("daily pairing attempts") && quota.contains("Settings → Quota"), "{quota}");
        assert!(quota.contains("anycon pair"), "{quota}");
        // 以后 hub 加了新原因：退出码 1，形状规整（小写字母和下划线）的原因照用做错误码；别的字符串不照抄，退回 pair_failed
        let e = pair_failure_error("brand_new");
        assert_eq!((e.exit, e.code.as_str()), (1, "brand_new"));
        let e = pair_failure_error("Weird Value!");
        assert_eq!((e.exit, e.code.as_str()), (1, "pair_failed"));
    }

    #[test]
    fn a_hub_refusal_of_pair_begin_is_told_apart_from_the_daemon_being_offline() {
        use anycon_protocol::{ErrorCode, RpcError, RpcErrorData};
        let rpc = |code, reason: Option<&str>| {
            let mut e = RpcError::new(code, "the hub refused the pairing request");
            e.data = Some(RpcErrorData { code, field: None, reason: reason.map(str::to_string), retry_after_ms: None });
            CliError::from(e)
        };
        // 每日配对次数用完 / 被限速：busy + data.reason → 退出码 1；forbidden → 5
        let quota = rpc(ErrorCode::Busy, Some("quota"));
        assert_eq!(pair_begin_refusal(&quota), Some("quota"));
        assert_eq!((pair_failure_error("quota").exit, pair_failure_error("quota").code.as_str()), (1, "quota"));
        assert_eq!(pair_begin_refusal(&rpc(ErrorCode::Busy, Some("rate_limited"))), Some("rate_limited"));
        let forbidden = rpc(ErrorCode::Forbidden, Some("forbidden"));
        assert_eq!(pair_begin_refusal(&forbidden), Some("forbidden"));
        assert_eq!(pair_failure_error("forbidden").exit, 5);
        // 没连上 hub（unsupported）、没有细分原因的 busy / forbidden、别的错误：不算拒绝
        assert_eq!(pair_begin_refusal(&rpc(ErrorCode::Unsupported, None)), None);
        assert_eq!(pair_begin_refusal(&rpc(ErrorCode::Busy, None)), None);
        assert_eq!(pair_begin_refusal(&rpc(ErrorCode::Internal, Some("timeout"))), None);
        assert_eq!(pair_begin_refusal(&CliError::offline("not connected")), None);
    }

    #[test]
    fn config_errors_map_to_usage_only_when_the_input_is_bad() {
        use anycon_daemon::config::ConfigError;
        assert_eq!(config_error(ConfigError::Invalid("sessions.max_concurrent: must be >= 1".into())).exit, 2);
        assert_eq!(config_error(ConfigError::Parse("bad toml".into())).exit, 2);
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "read-only file system");
        let e = config_error(ConfigError::Io(io));
        assert_eq!((e.exit, e.code.as_str()), (1, "error"), "an I/O failure is not a usage error");
    }

    #[test]
    fn log_follow_neither_repeats_nor_loses_lines_across_races_partial_writes_and_rotation() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("daemon.log.2026-10-10");
        std::fs::write(&old, "one\ntwo\n").unwrap();
        let mut tail = LogTail { path: old.clone(), pos: 8, partial: vec![] };
        assert!(tail.poll(d.path()).is_empty());
        // 追加：一行完整的 + 一行写了一半的
        let mut f = std::fs::OpenOptions::new().append(true).open(&old).unwrap();
        f.write_all(b"three\nfou").unwrap();
        assert_eq!(tail.poll(d.path()), vec!["three"], "the half-written line is held back");
        assert!(tail.poll(d.path()).is_empty(), "nothing is repeated");
        f.write_all(b"r\n").unwrap();
        assert_eq!(tail.poll(d.path()), vec!["four"]);
        // 轮转：旧文件在切换之前又写了两行（含没换行的结尾），新文件已经出现
        f.write_all(b"five\nsix").unwrap();
        let new = d.path().join("daemon.log.2026-10-11");
        std::fs::write(&new, "seven\neight\n").unwrap();
        assert_eq!(
            tail.poll(d.path()),
            vec!["five", "six", "seven", "eight"],
            "the old file's tail is flushed before switching to the new file"
        );
        assert!(tail.poll(d.path()).is_empty());
        // 非 UTF-8 的字节不会让它失败
        std::fs::OpenOptions::new().append(true).open(&new).unwrap().write_all(b"bad\xff\n").unwrap();
        assert_eq!(tail.poll(d.path()), vec!["bad\u{fffd}"]);
        // 文件被截短：从头读
        std::fs::write(&new, "fresh\n").unwrap();
        assert_eq!(tail.poll(d.path()), vec!["fresh"]);
    }

    #[test]
    fn only_local_and_private_hubs_count_as_self_hosted() {
        for url in [
            "http://localhost:8080",
            "https://LOCALHOST",
            "http://dev.localhost:3000",
            "http://127.0.0.1:8080",
            "http://127.9.9.9",
            "http://[::1]:8080",
            "http://10.1.2.3:8080",
            "http://172.16.0.5",
            "http://172.31.255.1",
            "http://192.168.1.20:8080/base",
            "http://[fd12:3456::1]:8080",
            "http://[fc00::1]",
            "https://nas.local",
            "http://user:pw@192.168.0.2:80",
            "http://[::ffff:10.0.0.1]:80",
        ] {
            assert!(is_private_hub(url), "{url} should count as private");
        }
        for url in [
            "https://hub.example.com",
            "http://8.8.8.8",
            "http://172.32.0.1",
            "http://172.15.0.1",
            "http://192.169.0.1",
            "http://11.0.0.1",
            "http://[2001:db8::1]",
            "http://[fe00::1]",
            "https://localhost.example.com",
            "https://notlocal",
            "https://evil.com/@127.0.0.1",
            "",
        ] {
            assert!(!is_private_hub(url), "{url} should not count as private");
        }
    }

    #[test]
    fn short_codes_warn_unless_the_hub_is_local_or_private() {
        assert!(short_code_warning("http://127.0.0.1:8080").is_none());
        assert!(short_code_warning("http://192.168.1.5").is_none());
        let w = short_code_warning("https://hub.example.com").unwrap();
        assert!(w.contains("only suitable for a hub you run yourself") && w.contains("hub.example.com"), "{w}");
    }

    #[test]
    fn path_arguments_become_absolute_only_when_relative() {
        assert_eq!(absolute_path_arg("/abs/x").unwrap(), "/abs/x");
        assert_eq!(absolute_path_arg("~").unwrap(), "~");
        assert_eq!(absolute_path_arg("~/work/x").unwrap(), "~/work/x");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute_path_arg("proj").unwrap(), cwd.join("proj").to_string_lossy());
        assert_eq!(absolute_path_arg("./a/./b").unwrap(), cwd.join("a/b").to_string_lossy());
        // `~user` 不是 `~/…`：当相对路径处理，由 daemon 判为 invalid 之前先变成绝对路径（不会被误展开）
        assert!(absolute_path_arg("~user/x").unwrap().starts_with('/'));
    }

    #[test]
    fn browse_output_marks_projects_git_and_unlinkable_entries() {
        let entry = |name: &str, git: bool, project: Option<&str>, linkable: bool| BrowseEntry {
            name: name.into(),
            path: format!("/m/{name}"),
            git,
            project: project.map(str::to_string),
            linkable,
            blocked: None,
        };
        let r = WorkspaceBrowseResult {
            path: "/m".into(),
            parent: Some("/".into()),
            roots: vec!["/m".into()],
            linkable: true,
            project: None,
            blocked: None,
            entries: vec![
                entry("app", true, Some("app"), false),
                entry("data", false, None, false),
                entry("free", true, None, true),
                BrowseEntry { blocked: Some(LinkBlock::Hidden), ..entry(".cache", false, None, false) },
                BrowseEntry { blocked: Some(LinkBlock::Sensitive), ..entry("Library", false, None, false) },
            ],
            truncated: true,
        };
        let out = render_browse(&r);
        assert!(out.starts_with("/m  (linkable)\nparent: /\n"), "{out}");
        assert!(out.contains("app") && out.contains("git") && out.contains("project app"), "{out}");
        assert!(out.contains("data") && out.contains("not linkable"), "{out}");
        assert!(out.contains("hidden directory (list it in workspace.link_roots to allow)"), "{out}");
        assert!(out.contains("protected system or credential directory, or one that contains them"), "{out}");
        assert!(out.contains("truncated") && out.contains("anycon projects link"), "{out}");
        // 当前目录不能关联时说明原因；目录名和路径里的控制字符显示成转义
        let blocked = WorkspaceBrowseResult {
            linkable: false,
            blocked: Some(LinkBlock::DataDir),
            path: "/m/\x1b]52;c;ZXZpbA==\x07x".into(),
            entries: vec![entry("evil\x1b[2Kdir\n.git", false, None, true)],
            ..r.clone()
        };
        let out = render_browse(&blocked);
        assert!(out.contains("not linkable: anycon data directory"), "{out}");
        assert!(!out.contains('\x1b') && !out.contains('\x07'), "{out:?}");
        assert!(out.contains("\\x1b]52;c;ZXZpbA==\\x07x") && out.contains("evil\\x1b[2Kdir\\n.git"), "{out}");
        assert_eq!(out.lines().count(), 5, "{out}");
        let empty = WorkspaceBrowseResult { entries: vec![], truncated: false, parent: None, ..r.clone() };
        assert!(render_browse(&empty).contains("(no sub-directories)"));
        let proj = WorkspaceBrowseResult { project: Some("p".into()), ..empty };
        assert!(render_browse(&proj).starts_with("/m  (already project p)"));
    }
}
