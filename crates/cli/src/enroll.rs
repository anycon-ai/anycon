//! 账号内接入的 CLI 一侧（ADR-36，docs/spec/cli.md § 3.2 `pair --enroll`、§ 3.3 `connect`，消息序列见 docs/protocol.md § 4.4）。
//!
//! - [`pair_enroll`]：`anycon pair --enroll`，让本机 daemon 生成一个 6 位接入码（`node.enroll_code`），等有设备用它完成接入；
//! - [`connect`]：`anycon connect <hub-url> [--node] [--via] [--email]`（不带 `--code`）：登录账号 → 选节点 → 选方式 →
//!   接入 → 校验 `pair.ack` 后落盘。接入的 initiator 状态机在 daemon crate（`enroll_initiator`），这里只做 IO、提示和退出码。
//!
//! 账号登录态（会话 cookie）只在内存里，只用于这一次 WebSocket 升级（hub 只在升级那一刻读它）：连上就 `POST /auth/logout`，
//! 不管成功失败，结束时再保证登出一次。
//! 所有提示都在 `spawn_blocking` 里读，主循环同时继续读 WebSocket（hub 的 ping 要回），`Ctrl+C` 随时发 `enroll_cancel` 后退出（130）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_crypto::Keypair;
use anycon_daemon::config::Paths;
use anycon_daemon::enroll_initiator::{scan_qr_payload, Event, Finished, Initiator, InitiatorConfig, InitiatorError};
use anycon_daemon::hub::{http_request, platform, HttpResponse};
use anycon_protocol::{
    methods, Control, DeviceKind, EnrollFailReason, EnrollFrame, EnrollHint, EnrollMethod, EnrollRejectReason, Kind,
    NodeDevicesResult, NodeEnrollCodeResult, NodeSecurityEvent, SecurityEventKind,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::signal::unix::Signal;
use zeroize::Zeroizing;

use crate::cli::{ConnectArgs, PairArgs, ViaArg};
use crate::commands::App;
use crate::error::{CliError, Result, EXIT_ERROR, EXIT_NOT_PAIRED};
use crate::output::{ago, interrupt_signal, sanitize_label, short_id};
use crate::remote::{self, ConnectFail, HubState, HubWs, NodeEntry, PairDoneInfo};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// 接入会话最长多久（hub 那边也是 5 分钟）：EnrollStarted 没给就用它。
const ENROLL_TTL: Duration = Duration::from_secs(5 * 60);
/// 等 hub 回 `enroll_started` 最多多久。
const STARTED_TIMEOUT: Duration = Duration::from_secs(15);
/// 接入码输入格式不对（不是 6 位数字）最多重问几次；格式错的不算一次尝试。
const CODE_FORMAT_RETRIES: usize = 5;
/// 邮箱验证码在同一次登录里最多输错几次（hub 那边是同一个码 5 次尝试后作废）。
const EMAIL_CODE_TRIES: usize = 3;

// ---------------------------------------------------------------- 提示（可替换，测试里用脚本）

/// 终端交互。方法都是同步的、会阻塞（调用方放进 `spawn_blocking`）；`&self` + `Send + Sync` 好在多处共享。
///
/// 两个 `ask*` 都返回 `Zeroizing<String>`：回答里可能是口令、token、接入码，一路不再变回普通 `String`，用完清零。
pub(crate) trait Ui: Send + Sync + 'static {
    /// 一行说明（stderr）。
    fn say(&self, msg: &str);
    /// 问一行（回显）：选项、邮箱，也包括接入码（另一台设备的屏幕上本来就显示着）。
    fn ask(&self, prompt: &str) -> Result<Zeroizing<String>>;
    /// 问一行（不回显）：口令、token。
    fn ask_secret(&self, prompt: &str) -> Result<Zeroizing<String>>;
    /// 在终端画二维码。
    fn show_qr(&self, payload: &str);
}

pub(crate) struct TermUi;

impl Ui for TermUi {
    fn say(&self, msg: &str) {
        eprintln!("{msg}");
    }

    fn ask(&self, prompt: &str) -> Result<Zeroizing<String>> {
        use std::io::Write;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        // 先留够容量：一行输入通常不会让缓冲重新分配（重新分配会在旧位置留下一份没清零的拷贝）
        let mut s = Zeroizing::new(String::with_capacity(256));
        let n = std::io::stdin().read_line(&mut s)?;
        if n == 0 {
            return Err(CliError::general("end of input"));
        }
        Ok(Zeroizing::new(s.trim().to_string()))
    }

    fn ask_secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        crate::passwd::read_secret(prompt)
    }

    fn show_qr(&self, payload: &str) {
        match qrcode::QrCode::new(payload.as_bytes()) {
            Ok(qr) => {
                let img = qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
                eprintln!("\n{img}\n");
            }
            Err(e) => eprintln!("cannot render a QR code: {e}"),
        }
    }
}

/// 在阻塞线程里问一行，同时等 `Ctrl+C`。
async fn prompt(ui: &Arc<dyn Ui>, secret: bool, text: String, sigint: &mut Signal) -> Result<Zeroizing<String>> {
    let u = ui.clone();
    let task = tokio::task::spawn_blocking(move || if secret { u.ask_secret(&text) } else { u.ask(&text) });
    tokio::select! {
        r = task => r.map_err(|e| CliError::general(format!("prompt failed: {e}")))?,
        _ = sigint.recv() => Err(CliError::interrupted()),
    }
}

// ---------------------------------------------------------------- 账号里的节点

/// `GET /api/devices` 里我们关心的几个字段。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AccountDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub online: bool,
}

#[derive(Debug)]
pub(crate) enum NodePick<'a> {
    One(&'a AccountDevice),
    /// 没给 `--node` 而账号里有好几台：要交互选。
    Choose(Vec<&'a AccountDevice>),
}

/// 在账号的设备列表里按 `--node`（名称或 id，id 可以是前缀）找一台 daemon。不是 daemon 的设备不参与；
/// 找不到 / 有歧义都是参数错误（退出码 2）。名字来自 hub、未经认证，只用来选。
pub(crate) fn pick_node<'a>(devices: &'a [AccountDevice], key: Option<&str>) -> Result<NodePick<'a>> {
    let daemons: Vec<&AccountDevice> = devices.iter().filter(|d| d.kind == "daemon").collect();
    let Some(key) = key else {
        return match daemons.len() {
            0 => Err(CliError::usage(
                "this account has no nodes; run `anycon init` and `anycon pair` on the machine first",
            )),
            1 => Ok(NodePick::One(daemons[0])),
            _ => Ok(NodePick::Choose(daemons)),
        };
    };
    if let Some(d) = daemons.iter().find(|d| d.id == key) {
        return Ok(NodePick::One(d));
    }
    let by_name: Vec<&AccountDevice> = daemons.iter().copied().filter(|d| d.name == key).collect();
    let by_prefix: Vec<&AccountDevice> =
        daemons.iter().copied().filter(|d| !key.is_empty() && d.id.starts_with(key)).collect();
    for found in [by_name, by_prefix] {
        match found.len() {
            0 => continue,
            1 => return Ok(NodePick::One(found[0])),
            _ => {
                let names: Vec<String> = found
                    .iter()
                    .map(|d| format!("{} ({})", sanitize_label(&d.name, 64, &d.id), short_id(&d.id)))
                    .collect();
                return Err(CliError::usage(format!(
                    "{key:?} matches several nodes in this account: {}; use the id",
                    names.join(", ")
                )));
            }
        }
    }
    Err(CliError::usage(format!("no node in this account matches {key:?}; see the node list in the web UI")))
}

/// 完整的设备 id（32 位小写 hex）：不用列表也能直接用。
fn looks_like_device_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------- 登录账号

/// 账号登录态：只在内存里。
struct Account {
    hub_url: String,
    email: Option<String>,
    cookie: Option<String>,
}

fn http_err(hub_url: &str, e: String) -> CliError {
    CliError::offline(format!("cannot reach the hub at {hub_url}: {e}"))
}

/// 响应里的会话 cookie：每个 `Set-Cookie` 取第一段 `name=value`，空值（登出时清 cookie）不要。
fn session_cookie(r: &HttpResponse) -> Option<String> {
    let parts: Vec<String> = r
        .headers_named("set-cookie")
        .iter()
        .filter_map(|c| c.split(';').next())
        .filter(|kv| kv.split_once('=').is_some_and(|(k, v)| !k.trim().is_empty() && !v.trim().is_empty()))
        .map(|kv| kv.trim().to_string())
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// hub 的错误体：`{ "error": { "code", "message", "retry_after"? } }`（也容忍旧形状 `{ "error": "<code>", "message" }`）。
/// 返回 `(code, message, retry_after 秒)`，缺的是空 / `None`。
pub(crate) fn error_fields(r: &HttpResponse) -> (String, String, Option<u64>) {
    let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    let text = |x: Option<&Value>| x.and_then(Value::as_str).unwrap_or_default().to_string();
    let (code, message, body_retry) = match v.get("error") {
        Some(e) if e.is_object() => {
            (text(e.get("code")), text(e.get("message")), e.get("retry_after").and_then(Value::as_u64))
        }
        other => (text(other), text(v.get("message")), None),
    };
    let retry = body_retry.or_else(|| r.header("retry-after").and_then(|s| s.trim().parse().ok()));
    (code, message, retry)
}

pub(crate) fn api_error(what: &str, r: &HttpResponse) -> CliError {
    let (_, detail, retry) = error_fields(r);
    let msg = if detail.is_empty() {
        format!("{what} failed (HTTP {})", r.status)
    } else {
        format!("{what} failed: {detail}")
    };
    match r.status {
        401 | 403 => CliError::denied("unauthorized", msg),
        429 => {
            let wait = retry.map_or_else(|| "wait a minute".to_string(), |s| format!("wait {s} seconds"));
            CliError::new(EXIT_ERROR, "rate_limited", format!("{msg}; {wait} and try again"))
        }
        _ => CliError::general(msg),
    }
}

impl Account {
    fn new(hub_url: &str, email: Option<String>) -> Account {
        Account { hub_url: hub_url.to_string(), email, cookie: None }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.hub_url)
    }

    /// 登录（已经登录就什么都不做）。方式来自 `GET /api/config`：邮箱验证码，或 token。
    async fn ensure(&mut self, ui: &Arc<dyn Ui>, sigint: &mut Signal) -> Result<()> {
        if self.cookie.is_some() {
            return Ok(());
        }
        let r = http_request("GET", &self.url("/api/config"), &[], None, HTTP_TIMEOUT)
            .await
            .map_err(|e| http_err(&self.hub_url, e))?;
        if r.status != 200 {
            return Err(api_error("reading the hub configuration", &r));
        }
        let cfg: Value = r.json().map_err(CliError::general)?;
        let methods: Vec<String> = cfg
            .get("login_methods")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let (has_email, has_token) = (methods.iter().any(|m| m == "email"), methods.iter().any(|m| m == "token"));
        if !has_email && !has_token {
            return Err(CliError::usage(
                "this hub only offers sign-in methods the CLI cannot use (OAuth); pair with a short code instead: `anycon connect <hub-url> --code <code>`",
            ));
        }
        let use_email = match (has_email, has_token) {
            (true, false) => true,
            (false, _) => false,
            (true, true) => {
                if self.email.is_some() {
                    true
                } else {
                    let answer = prompt(
                        ui,
                        false,
                        "Sign in with an email code or an account token? [email/token] (email): ".into(),
                        sigint,
                    )
                    .await?;
                    !answer.trim().eq_ignore_ascii_case("token")
                }
            }
        };
        self.cookie =
            Some(if use_email { self.login_email(ui, sigint).await? } else { self.login_token(ui, sigint).await? });
        Ok(())
    }

    async fn login_token(&self, ui: &Arc<dyn Ui>, sigint: &mut Signal) -> Result<String> {
        let token = prompt(ui, true, "Account token: ".into(), sigint).await?;
        if token.trim().is_empty() {
            return Err(CliError::usage("no token entered"));
        }
        let body = serde_json::to_vec(&json!({ "token": token.trim() }))?;
        let r = http_request("POST", &self.url("/auth/token"), &[("X-Anycon", "1")], Some(&body), HTTP_TIMEOUT)
            .await
            .map_err(|e| http_err(&self.hub_url, e))?;
        if r.status != 204 && r.status != 200 {
            return Err(api_error("signing in", &r));
        }
        session_cookie(&r).ok_or_else(|| CliError::general("the hub signed us in but sent no session"))
    }

    async fn login_email(&self, ui: &Arc<dyn Ui>, sigint: &mut Signal) -> Result<String> {
        let email = match &self.email {
            Some(e) => e.clone(),
            None => prompt(ui, false, "Email address: ".into(), sigint).await?.to_string(),
        };
        let email = email.trim().to_string();
        if !email.contains('@') {
            return Err(CliError::usage("that does not look like an email address"));
        }
        let body = serde_json::to_vec(&json!({ "email": email }))?;
        let r = http_request("POST", &self.url("/auth/email/send"), &[("X-Anycon", "1")], Some(&body), HTTP_TIMEOUT)
            .await
            .map_err(|e| http_err(&self.hub_url, e))?;
        if r.status != 204 && r.status != 200 {
            return Err(api_error("sending the sign-in code", &r));
        }
        // hub 对绑定和没绑定的邮箱回答一模一样（不暴露哪些邮箱有账号）：措辞也不能说「已发送」
        ui.say("If that email is bound to an account, a 6-digit code was sent to it (valid for 10 minutes).");
        let mut format_errors = 0;
        let mut wrong = 0;
        loop {
            let typed = prompt(ui, false, "Code from the email: ".into(), sigint).await?;
            // hub 容忍 `123 456` / `123-456`；本地先把格式不对的挡掉，不白白消耗 hub 那边 5 次的机会
            let code: String = typed.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
            if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
                format_errors += 1;
                if format_errors >= CODE_FORMAT_RETRIES {
                    return Err(CliError::usage("that is not a 6-digit code"));
                }
                ui.say("The code is 6 digits, e.g. 123456.");
                continue;
            }
            let body = serde_json::to_vec(&json!({ "email": email, "code": code }))?;
            let r =
                http_request("POST", &self.url("/auth/email/verify"), &[("X-Anycon", "1")], Some(&body), HTTP_TIMEOUT)
                    .await
                    .map_err(|e| http_err(&self.hub_url, e))?;
            if r.status == 204 || r.status == 200 {
                return session_cookie(&r).ok_or_else(|| CliError::general("the hub signed us in but sent no session"));
            }
            if r.status == 401 && error_fields(&r).0 == "invalid_code" {
                wrong += 1;
                if wrong >= EMAIL_CODE_TRIES {
                    return Err(CliError::denied(
                        "invalid_code",
                        "the code is wrong or has expired; run the command again to get a new one",
                    ));
                }
                ui.say("That code is wrong or has expired. Try again.");
                continue;
            }
            return Err(api_error("signing in", &r));
        }
    }

    /// 账号里的设备（需要登录）。
    async fn devices(&self) -> Result<Vec<AccountDevice>> {
        let cookie = self.cookie.as_deref().ok_or_else(|| CliError::general("not signed in"))?;
        let r = http_request("GET", &self.url("/api/devices"), &[("Cookie", cookie)], None, HTTP_TIMEOUT)
            .await
            .map_err(|e| http_err(&self.hub_url, e))?;
        if r.status != 200 {
            return Err(api_error("listing the devices of the account", &r));
        }
        r.json().map_err(CliError::general)
    }

    /// 用这台 CLI 已有的设备 token 列账号里的设备（`Authorization: Bearer`，只读，不用登录）。
    /// `Ok(None)` = hub 不认这个 token（401：已吊销 / 不存在，或者 hub 太旧还不支持）：调用方回退到登录。
    async fn devices_with_token(&self, token: &str) -> Result<Option<Vec<AccountDevice>>> {
        let bearer = format!("Bearer {token}");
        let r = http_request("GET", &self.url("/api/devices"), &[("Authorization", &bearer)], None, HTTP_TIMEOUT)
            .await
            .map_err(|e| http_err(&self.hub_url, e))?;
        match r.status {
            200 => Ok(Some(r.json().map_err(CliError::general)?)),
            401 => Ok(None),
            _ => Err(api_error("listing the devices of the account", &r)),
        }
    }

    /// 结束时一定登出（尽力而为）：cookie 不留在任何地方。
    async fn logout(&mut self) {
        if let Some(cookie) = self.cookie.take() {
            let r = http_request(
                "POST",
                &self.url("/auth/logout"),
                &[("X-Anycon", "1"), ("Cookie", &cookie)],
                None,
                Duration::from_secs(5),
            )
            .await;
            if let Err(e) = r {
                tracing::debug!(error = %e, "sign-out request failed");
            }
        }
    }
}

// ---------------------------------------------------------------- 接入

pub(crate) struct EnrollEnv {
    pub paths: Paths,
    pub hub_url: String,
    pub device_name: String,
    pub ack_timeout: Duration,
}

pub(crate) struct EnrollRequest {
    /// 全局 `--node`：名称或 id（前缀）。
    pub node: Option<String>,
    pub via: Option<ViaArg>,
    pub email: Option<String>,
}

pub(crate) struct Joined {
    pub node: NodeEntry,
    pub method: EnrollMethod,
}

/// `anycon connect <hub-url>`（账号内接入）：成功返回新落盘的节点。
pub(crate) async fn enroll(env: &EnrollEnv, ui: &Arc<dyn Ui>, req: EnrollRequest) -> Result<Joined> {
    let hub_url = env.hub_url.trim().trim_end_matches('/').to_string();
    let dir = env.paths.client_dir(&remote::host_of(&hub_url)?);
    let mut state = HubState::load(&dir, &hub_url);
    state.check()?;
    state.hub_url = hub_url.clone();
    let mut sigint = interrupt_signal();
    let mut account = Account::new(&hub_url, req.email.clone());
    let result = enroll_inner(env, ui, &req, &mut state, &mut account, &mut sigint).await;
    account.logout().await;
    result
}

fn via_to_method(v: ViaArg) -> (EnrollMethod, bool) {
    match v {
        ViaArg::Scan => (EnrollMethod::Scan, false),
        ViaArg::Approve => (EnrollMethod::Code, true),
        ViaArg::Code => (EnrollMethod::Code, false),
        ViaArg::Password => (EnrollMethod::Password, false),
    }
}

async fn choose_via(ui: &Arc<dyn Ui>, sigint: &mut Signal) -> Result<ViaArg> {
    ui.say("How do you want to unlock the node?");
    ui.say("  1) scan      scan a QR code with a paired phone (web UI: approve a new device)");
    ui.say("  2) approve   ask a paired device to approve this one, then type the 6-digit code it shows");
    ui.say("  3) code      type a join code (a paired device's \"add device\", or `anycon pair --enroll`)");
    ui.say("  4) password  type the node password (`anycon passwd`)");
    for _ in 0..3 {
        let a = prompt(ui, false, "Choose [1-4]: ".into(), sigint).await?;
        match a.trim().to_ascii_lowercase().as_str() {
            "1" | "scan" => return Ok(ViaArg::Scan),
            "2" | "approve" => return Ok(ViaArg::Approve),
            "3" | "code" => return Ok(ViaArg::Code),
            "4" | "password" => return Ok(ViaArg::Password),
            _ => ui.say("please enter 1, 2, 3 or 4"),
        }
    }
    Err(CliError::usage("no method chosen"))
}

async fn choose_node(ui: &Arc<dyn Ui>, nodes: &[&AccountDevice], sigint: &mut Signal) -> Result<String> {
    ui.say("Which node?");
    for (i, d) in nodes.iter().enumerate() {
        ui.say(&format!(
            "  {}) {} ({}){}",
            i + 1,
            sanitize_label(&d.name, 64, &d.id),
            short_id(&d.id),
            if d.online { "" } else { "  [offline]" }
        ));
    }
    for _ in 0..3 {
        let a = prompt(ui, false, format!("Choose [1-{}]: ", nodes.len()), sigint).await?;
        if let Some(d) = a.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| nodes.get(i)) {
            return Ok(d.id.clone());
        }
        ui.say("not a valid choice");
    }
    Err(CliError::usage("no node chosen"))
}

async fn enroll_inner(
    env: &EnrollEnv,
    ui: &Arc<dyn Ui>,
    req: &EnrollRequest,
    state: &mut HubState,
    account: &mut Account,
    sigint: &mut Signal,
) -> Result<Joined> {
    // 1. 选节点。这台 CLI 已经配对过它（本地记录）或者给的是完整的 id：不用列表，也不用登录；否则按账号里的设备列表选：
    //    这台 CLI 已经是这个 hub 上的设备就先用设备 token 列（只读），hub 不认（401）才回退到登录
    let node_id = match req.node.as_deref() {
        Some(key) if state.find_node(key).is_some() => state.find_node(key).map(|n| n.id.clone()).unwrap_or_default(),
        Some(key) if looks_like_device_id(key) => key.to_string(),
        key => {
            let by_token = match state.token.as_deref() {
                Some(token) => account.devices_with_token(token).await?,
                None => None,
            };
            let devices = match by_token {
                Some(list) => list,
                None => {
                    account.ensure(ui, sigint).await?;
                    account.devices().await?
                }
            };
            match pick_node(&devices, key)? {
                NodePick::One(d) => d.id.clone(),
                NodePick::Choose(list) => choose_node(ui, &list, sigint).await?,
            }
        }
    };
    // 2. 选方式，收凭据（连上 hub 之前先问好：码 / 口令输错格式不用占着接入会话）
    let via = match req.via {
        Some(v) => v,
        None => choose_via(ui, sigint).await?,
    };
    let (method, approval) = via_to_method(via);
    let kp = match state.keypair() {
        Some(k) => k,
        None => {
            let secret = anycon_daemon::util::random_bytes::<32>();
            state.secret = Some(secret);
            Keypair::from_secret(secret)
        }
    };
    let name = env.device_name.clone();
    let mut init = Initiator::new(InitiatorConfig {
        node_id: node_id.clone(),
        method,
        approval,
        hint: EnrollHint { name: name.clone(), kind: DeviceKind::Cli, platform: platform() },
        client_info: remote::client_info(&name),
        keypair: kp,
    });
    match via {
        ViaArg::Password => {
            let pw = prompt(ui, true, "Node password: ".into(), sigint).await?;
            if pw.is_empty() {
                return Err(CliError::usage("no password entered"));
            }
            init.set_password(&pw);
        }
        ViaArg::Code => ask_code(ui, &mut init, "Join code (6 digits): ", sigint).await?,
        ViaArg::Scan | ViaArg::Approve => {}
    }
    // 3. 连 hub：已有设备身份就带着 token（账号取自设备）；没有就开临时连接，升级请求里带登录 cookie
    let mut h = match state.token.clone() {
        Some(token) => match HubWs::connect_raw(&env.hub_url, Some(token), &name, account.cookie.as_deref()).await {
            Ok(h) => h,
            Err(ConnectFail::Auth(
                anycon_protocol::AuthFailReason::Invalid | anycon_protocol::AuthFailReason::Revoked,
            )) => {
                ui.say("note: the hub no longer accepts this device's saved credentials; joining as a new device");
                state.forget_credentials()?;
                account.ensure(ui, sigint).await?;
                HubWs::connect_raw(&env.hub_url, None, &name, account.cookie.as_deref())
                    .await
                    .map_err(ConnectFail::into_cli)?
            }
            Err(f) => return Err(f.into_cli()),
        },
        None => {
            account.ensure(ui, sigint).await?;
            HubWs::connect_raw(&env.hub_url, None, &name, account.cookie.as_deref())
                .await
                .map_err(ConnectFail::into_cli)?
        }
    };
    // hub 只在升级那一刻读一次 cookie（账号已经记在这条连接上）：连上了就马上登出，会话 cookie 不多留一秒
    account.logout().await;
    // 4. enroll_begin
    h.send_control(&Control::EnrollBegin { node: node_id.clone() }).await?;
    let (enroll_id, expires_at_ms) = match h
        .wait_control(STARTED_TIMEOUT, |c| matches!(c, Control::EnrollStarted { .. } | Control::EnrollFailed { .. }))
        .await?
    {
        Control::EnrollStarted { enroll_id, expires_at_ms, .. } => (enroll_id, expires_at_ms),
        Control::EnrollFailed { reason, .. } => return Err(enroll_failed(reason)),
        _ => unreachable!(),
    };
    let ttl =
        Duration::from_millis(expires_at_ms.saturating_sub(anycon_daemon::util::now_ms()).max(1_000)).min(ENROLL_TTL);
    let outcome = run_handshake(&mut h, ui, &mut init, &node_id, &enroll_id, ttl, env.ack_timeout, state, sigint).await;
    match outcome {
        Ok(node) => Ok(Joined { node, method }),
        Err(e) => {
            // 接入会话还没走完：告诉 hub 和 daemon 放弃（已经完成的话 hub 已经清掉了，多发一条无害）
            let _ = h.send_control(&Control::EnrollCancel { enroll_id: enroll_id.clone() }).await;
            Err(e)
        }
    }
}

/// 问接入码，格式不对（不是 6 位数字）就重问；格式错的不会发给 daemon，不占尝试次数。
async fn ask_code(ui: &Arc<dyn Ui>, init: &mut Initiator, prompt_text: &str, sigint: &mut Signal) -> Result<()> {
    for _ in 0..CODE_FORMAT_RETRIES {
        let code = prompt(ui, false, prompt_text.into(), sigint).await?;
        match init.set_code(&code) {
            Ok(_) => return Ok(()),
            Err(InitiatorError::InvalidCode) => ui.say("the code is 6 digits, like 123456 or 123-456"),
            Err(e) => return Err(CliError::general(e.to_string())),
        }
    }
    Err(CliError::usage("no valid join code entered"))
}

/// hub 的 `enroll_failed` → 错误和退出码。
pub(crate) fn enroll_failed(reason: EnrollFailReason) -> CliError {
    match reason {
        EnrollFailReason::Forbidden => CliError::denied(
            "forbidden",
            "the hub does not know which account this connection belongs to; sign in again, or pair with a short code (`--code`)",
        ),
        EnrollFailReason::NotFound => CliError::usage("that node is not in your account (or the request is gone)"),
        EnrollFailReason::Offline => CliError::offline("the node is offline; is its daemon running and connected to the hub?"),
        EnrollFailReason::RateLimited => {
            CliError::new(EXIT_ERROR, "rate_limited", "too many join requests; wait a minute and try again")
        }
        EnrollFailReason::Quota => {
            CliError::new(EXIT_ERROR, "quota", "the daily limit for pairing and joining on this hub is used up")
        }
        EnrollFailReason::Busy => CliError::new(
            EXIT_ERROR,
            "busy",
            "the node is already handling other join requests (or this connection has one in progress); try again in a moment",
        ),
        EnrollFailReason::Timeout => CliError::new(EXIT_ERROR, "timeout", "timed out: the join request was not completed within 5 minutes"),
        EnrollFailReason::PeerGone => CliError::offline("the node disconnected during the join"),
        EnrollFailReason::Cancelled => CliError::new(EXIT_ERROR, "cancelled", "the node cancelled the join request"),
    }
}

/// daemon 的 `rejected` → 错误和退出码（5 = 码 / 口令不对、被拒绝、方式没开启、退避中、已锁定）。
pub(crate) fn rejected_error(reason: EnrollRejectReason, retry_after_ms: Option<u64>) -> CliError {
    let wait = retry_after_ms.map(|ms| format!(" Try again in {} s.", ms.div_ceil(1000))).unwrap_or_default();
    match reason {
        EnrollRejectReason::WrongSecret => CliError::denied("wrong_secret", "wrong code or password"),
        EnrollRejectReason::Denied => CliError::denied("denied", "the join request was denied on a paired device"),
        EnrollRejectReason::MethodDisabled => CliError::denied(
            "method_disabled",
            "this way of unlocking is turned off on the node ([enroll] in its config.toml)",
        ),
        EnrollRejectReason::PasswordNotSet => CliError::denied(
            "password_not_set",
            "the node has no password; set one on the node's machine with `anycon passwd`",
        ),
        EnrollRejectReason::Backoff => CliError::denied(
            "backoff",
            format!("too many wrong passwords in a row; the node is making you wait.{wait}"),
        ),
        EnrollRejectReason::Locked => CliError::denied(
            "locked",
            "password unlock is locked after too many wrong attempts. On the node's machine run `anycon passwd --enable` (after checking `anycon audit`)",
        ),
        EnrollRejectReason::NoCode => CliError::denied(
            "no_code",
            "the node has no valid join code; create one with `anycon pair --enroll` on its machine or \"add device\" in the web UI, or use --via approve",
        ),
        EnrollRejectReason::Busy => CliError::new(EXIT_ERROR, "busy", "the node is busy with other join requests; try again in a moment"),
        EnrollRejectReason::Expired => CliError::new(EXIT_ERROR, "expired", "the request or the join code expired; start again"),
        EnrollRejectReason::Invalid => CliError::new(EXIT_ERROR, "protocol_error", "the node rejected the request as malformed"),
    }
}

fn describe_initiator_error(e: InitiatorError) -> CliError {
    match e {
        InitiatorError::InvalidCode => CliError::usage("the join code must be 6 digits"),
        other => CliError::general(other.to_string()),
    }
}

/// 等一个后台提示的结果。
async fn input_ready(p: &mut PendingInput) -> (Input, Result<Zeroizing<String>>) {
    match p.as_mut() {
        Some((kind, h)) => {
            let r = h.await.map_err(|e| CliError::general(format!("prompt failed: {e}"))).and_then(|r| r);
            (*kind, r)
        }
        None => std::future::pending().await,
    }
}

fn encode_frame(f: &EnrollFrame) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(f).map_err(|e| CliError::general(format!("cannot encode a frame: {e}")))
}

/// 在主循环里并发等的一次输入（循环同时继续读 WebSocket）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    /// 批准之后输入对方显示的 6 位码。
    ApprovalCode,
    /// 接入码输错了，重新输入后再来一轮（同一个会话里重发 hello）。
    RetryCode,
}

type PendingInput = Option<(Input, tokio::task::JoinHandle<Result<Zeroizing<String>>>)>;

#[derive(PartialEq, Eq)]
enum Phase {
    /// hello 已发，等 daemon 的回复。
    AwaitReply,
    /// daemon 在等已配对设备批准。
    WaitApproval,
    /// 握手走完，已发 `pair_confirm`，等 `pair_done`。
    Confirming,
}

/// 发 hello 起，到 `pair.ack` 校验完落盘为止。
#[allow(clippy::too_many_arguments)]
async fn run_handshake(
    h: &mut HubWs,
    ui: &Arc<dyn Ui>,
    init: &mut Initiator,
    node_id: &str,
    enroll_id: &str,
    ttl: Duration,
    ack_timeout: Duration,
    state: &mut HubState,
    sigint: &mut Signal,
) -> Result<NodeEntry> {
    let method = init.method();
    let deadline = Instant::now() + ttl;
    if method == EnrollMethod::Scan {
        let secret = *init.scan_secret().ok_or_else(|| CliError::general("no scan secret"))?;
        ui.say(
            "Scan this with the web UI of a device that is already paired with the node (\"approve a new device\"):",
        );
        ui.show_qr(&scan_qr_payload(node_id, enroll_id, &secret));
    }
    let hello = init.hello();
    h.send_env(node_id, Kind::Handshake, encode_frame(&hello)?).await?;
    let mut phase = Phase::AwaitReply;
    let mut confirm_sent = false;
    let mut finished: Option<Finished> = None;
    let mut pending_input: PendingInput = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_notice = Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => return Err(CliError::interrupted()),
            (kind, code) = input_ready(&mut pending_input), if pending_input.is_some() => {
                pending_input = None;
                let code = code?;
                match init.set_code(&code) {
                    Ok(Some(Event::Send(frames))) => {
                        for f in frames {
                            h.send_env(node_id, Kind::Handshake, encode_frame(&f)?).await?;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        // 接入码输错重试：码换了，开新的一轮
                        if kind == Input::RetryCode {
                            let hello = init.hello();
                            h.send_env(node_id, Kind::Handshake, encode_frame(&hello)?).await?;
                            phase = Phase::AwaitReply;
                            finished = None;
                        }
                    }
                    Err(InitiatorError::InvalidCode) => {
                        ui.say("the code is 6 digits, like 123456 or 123-456");
                        pending_input = Some((kind, spawn_prompt(ui, false, "Code: ")));
                    }
                    Err(e) => return Err(describe_initiator_error(e)),
                }
            }
            env = h.recv(Duration::from_secs(3600)) => {
                let Some(env) = env? else { continue };
                match env.kind {
                    Kind::Handshake if env.from == node_id => {
                        let Ok(frame) = rmp_serde::from_slice::<EnrollFrame>(&env.body) else { continue };
                        let ev = init.handle(frame).map_err(describe_initiator_error)?;
                        match ev {
                            Event::Send(frames) => {
                                phase = Phase::AwaitReply;
                                for f in frames {
                                    h.send_env(node_id, Kind::Handshake, encode_frame(&f)?).await?;
                                }
                            }
                            Event::WaitApproval => {
                                phase = Phase::WaitApproval;
                                last_notice = Instant::now();
                                ui.say("Waiting for a paired device to approve this request...");
                            }
                            Event::NeedCode => {
                                ui.say("Approved. A paired device now shows a 6-digit code.");
                                pending_input = Some((Input::ApprovalCode, spawn_prompt(ui, false, "Code shown on the approving device: ")));
                            }
                            Event::Rejected { reason, retry_after_ms, attempts_left } => {
                                // 接入码输错、还有次数：重新问一个码再来一轮（同一个会话里重发 hello）
                                if reason == EnrollRejectReason::WrongSecret
                                    && method == EnrollMethod::Code
                                    && attempts_left.is_some_and(|n| n > 0)
                                {
                                    let left = attempts_left.unwrap_or(0);
                                    ui.say(&format!("Wrong code. {left} attempt{} left.", if left == 1 { "" } else { "s" }));
                                    pending_input = Some((Input::RetryCode, spawn_prompt(ui, false, "Join code (6 digits): ")));
                                    continue;
                                }
                                return Err(rejected_error(reason, retry_after_ms));
                            }
                            Event::Finished(fin) => {
                                for f in &fin.frames {
                                    h.send_env(node_id, Kind::Handshake, encode_frame(f)?).await?;
                                }
                                // pair_confirm 发一次：接入码输错重试时 hub 那边已经记下了我们的确认
                                if !confirm_sent {
                                    h.send_control(&Control::PairConfirm { peer: node_id.to_string() }).await?;
                                    confirm_sent = true;
                                }
                                phase = Phase::Confirming;
                                finished = Some(*fin);
                            }
                        }
                    }
                    Kind::Control => match env.control_body() {
                        Ok(Control::EnrollFailed { enroll_id: id, reason }) if id.as_deref().is_none_or(|i| i == enroll_id) => {
                            return Err(enroll_failed(reason));
                        }
                        Ok(Control::PairFailed { reason }) => return Err(remote::pair_failed(reason)),
                        Ok(Control::PairDone { peer, device_id, device_token }) => {
                            let Some(mut fin) = finished.take() else {
                                // 还没握手完 hub 就说完成了：不可信，什么都不落盘
                                return Err(CliError::new(
                                    EXIT_NOT_PAIRED,
                                    "pair_failed",
                                    "the hub reported the join as done before the node proved anything; nothing was saved",
                                ));
                            };
                            let done = PairDoneInfo { peer, device_id, device_token };
                            return remote::complete_pairing(state, h, &mut fin.transport, fin.node_pub, done, ack_timeout).await;
                        }
                        Ok(Control::Undeliverable { to, .. }) if to == node_id => {
                            return Err(CliError::offline("the node went offline"));
                        }
                        Ok(Control::PeerOffline { device }) if device == node_id => {
                            return Err(CliError::offline("the node went offline"));
                        }
                        _ => {}
                    },
                    Kind::Data | Kind::Notify => h.stash_data(env),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if Instant::now() >= deadline {
                    return Err(CliError::new(EXIT_ERROR, "timeout", "timed out: the join request was not completed within 5 minutes"));
                }
                if phase == Phase::WaitApproval && last_notice.elapsed() >= Duration::from_secs(20) {
                    last_notice = Instant::now();
                    ui.say(&format!("  still waiting... {} s left (Ctrl+C to cancel)", deadline.saturating_duration_since(Instant::now()).as_secs()));
                }
            }
        }
    }
}

fn spawn_prompt(ui: &Arc<dyn Ui>, secret: bool, text: &str) -> tokio::task::JoinHandle<Result<Zeroizing<String>>> {
    let (u, text) = (ui.clone(), text.to_string());
    tokio::task::spawn_blocking(move || if secret { u.ask_secret(&text) } else { u.ask(&text) })
}

/// `anycon connect <hub-url>`（不带 `--code`）。
pub async fn connect(app: &App, a: ConnectArgs) -> Result<u8> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(CliError::usage(
            "joining with your account needs an interactive terminal (sign-in codes and approval codes are typed by a person); in scripts pair with a short code: `anycon connect <hub-url> --code <code>`",
        ));
    }
    let env = EnrollEnv {
        paths: app.paths.clone(),
        hub_url: a.hub_url.clone(),
        device_name: a.name.clone().unwrap_or_else(anycon_daemon::config::hostname),
        ack_timeout: remote::ACK_TIMEOUT,
    };
    let ui: Arc<dyn Ui> = Arc::new(TermUi);
    let joined =
        enroll(&env, &ui, EnrollRequest { node: app.node.clone(), via: a.via, email: a.email.clone() }).await?;
    if app.json() {
        app.out.json_line(&json!({
            "node": { "id": joined.node.id, "name": joined.node.name },
            "hub": a.hub_url,
            "method": joined.method,
        }));
    } else {
        println!("Joined node {} ({}) via {}", joined.node.name, short_id(&joined.node.id), a.hub_url);
        println!("Try: `anycon --node {} agents`", joined.node.name);
    }
    Ok(0)
}

// ---------------------------------------------------------------- anycon pair --enroll

/// `123456` → `123-456`。
pub(crate) fn pretty_code(code: &str) -> String {
    if code.len() == 6 {
        format!("{}-{}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// `anycon pair --enroll`：生成 6 位接入码（5 分钟、合计 3 次、新的顶掉旧的），等有设备用它完成接入。
pub async fn pair_enroll(app: &App, a: PairArgs) -> Result<u8> {
    let mut conn = app.local_conn().await?;
    // 先订阅通知再生成码：别错过完成事件
    let mut notes = conn.notifications();
    let hub: Option<Value> = conn.call_raw(anycon_daemon::local_methods::STATUS, json!({})).await.ok();
    let hub_connected = hub.as_ref().and_then(|s| s["hub"]["connected"].as_bool()).unwrap_or(false);
    let hub_url = hub.as_ref().and_then(|s| s["hub"]["url"].as_str()).map(str::to_string);
    let r: NodeEnrollCodeResult = conn.call(methods::NODE_ENROLL_CODE, json!({})).await.map_err(|e| {
        if e.code == "forbidden" {
            CliError::denied(
                "forbidden",
                "join codes are turned off on this node; turn them on with `anycon config set enroll.approval true`",
            )
        } else {
            e
        }
    })?;
    if !hub_connected {
        eprintln!(
            "{}",
            app.out.err_yellow("warning: the daemon is not connected to the hub right now; a new device cannot reach it until it is (see `anycon doctor`)")
        );
    }
    if app.json() {
        app.out.json_line(&json!({ "code": r.code, "expires_at_ms": r.expires_at_ms }));
    } else {
        println!("Join code: {}", app.out.bold(&pretty_code(&r.code)));
        println!("Valid for 5 minutes, up to 3 tries; generating another replaces it.");
        println!(
            "On a new device signed in to the same account: web unlock page -> \"I have a join code\", or `anycon connect {} --via code`.",
            hub_url.as_deref().unwrap_or("<hub-url>")
        );
    }
    if a.no_wait {
        return Ok(0);
    }
    let deadline = std::time::Instant::now()
        + Duration::from_millis(r.expires_at_ms.saturating_sub(anycon_daemon::util::now_ms()).max(1000))
        + Duration::from_secs(2);
    let mut sigint = interrupt_signal();
    loop {
        let wait = deadline.saturating_duration_since(std::time::Instant::now());
        if wait.is_zero() {
            return Err(CliError::general("the join code expired before a device used it (it is valid for 5 minutes)"));
        }
        tokio::select! {
            _ = sigint.recv() => return Err(CliError::interrupted()),
            _ = tokio::time::sleep(wait) => {}
            n = notes.recv() => {
                let Some(n) = n else { return Err(CliError::offline("the connection to the daemon was lost")) };
                if n.method != methods::NODE_SECURITY_EVENT {
                    continue;
                }
                let Ok(ev) = serde_json::from_value::<NodeSecurityEvent>(n.params) else { continue };
                if ev.kind != SecurityEventKind::DeviceEnrolled || ev.method != Some(EnrollMethod::Code) {
                    continue;
                }
                let Some(device_id) = ev.device_id else { continue };
                let mut name = ev.device_name.unwrap_or_else(|| short_id(&device_id));
                if let Some(n) = &a.name {
                    conn.call_raw(
                        anycon_daemon::local_methods::RENAME_DEVICE,
                        json!({ "device": device_id, "name": n }),
                    )
                    .await?;
                    name = n.clone();
                }
                let devices: Option<NodeDevicesResult> = conn.call(methods::NODE_DEVICES, json!({})).await.ok();
                let kind = devices
                    .and_then(|d| d.devices.into_iter().find(|d| d.device_id == device_id))
                    .map(|d| d.kind)
                    .unwrap_or(DeviceKind::Web);
                let name = sanitize_label(&name, 100, &device_id);
                if app.json() {
                    app.out.json_line(&json!({ "enrolled": { "device_id": device_id, "name": name, "kind": kind } }));
                } else {
                    println!("Joined: {} ({:?}, {}) {}", name, kind, short_id(&device_id), ago(ev.ts_ms));
                }
                return Ok(0);
            }
        }
    }
}
