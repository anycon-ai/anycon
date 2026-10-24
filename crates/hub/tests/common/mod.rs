//! 集成测试公共设施：起真实 hub（随机端口、临时数据目录）、假设备（tokio-tungstenite + msgpack 信封）。
#![allow(dead_code)]

pub mod smtp;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_hub::config::{Mode, QuotaLimits};
use anycon_hub::mail::{OutgoingMail, RecordingMailer};
use anycon_hub::store::NewDevice;
use anycon_hub::util::{hash_token, new_id, new_secret, now_ms};
use anycon_hub::{Config, Handle};
use anycon_protocol::{ClientInfo, Control, DeviceKind, Envelope, Kind, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// 测试 hub 的预览域名（`http://*.preview.localhost`，没有端口：测试请求自己带 `Host`，直连随机端口）。
pub const PREVIEW_DOMAIN: &str = "preview.localhost";

/// 一个新的、合法的预览 label。
pub fn new_label() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    anycon_protocol::preview::tunnel::label_from_bytes(&bytes)
}

pub struct TestHub {
    pub handle: Handle,
    pub dir: tempfile::TempDir,
    pub http: reqwest::Client,
    /// bootstrap 管理员的 token。`multi` 没有，是空串（`login_admin` 会直接报错）。
    pub admin_token: String,
}

impl TestHub {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::new("https://hub.test", dir.path()).expect("config");
        config.listen = "127.0.0.1:0".parse().unwrap();
        // 验证码验证失败默认要等 250 ms：测试里不关心，免得整套用例平白变慢。要测它的用例自己设回来。
        config.timing.email_verify_failure_delay = Duration::ZERO;
        tweak(&mut config);
        let multi = config.mode == Mode::Multi;
        let handle = anycon_hub::run(config).await.expect("hub starts");
        let admin_token = if multi {
            assert!(handle.generated_admin_token.is_none(), "multi has no bootstrap admin token");
            String::new()
        } else {
            handle.generated_admin_token.clone().expect("first run generates an admin token")
        };
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { handle, dir, http, admin_token }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.handle.base_url(), path)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/ws", self.handle.addr.port())
    }

    /// bootstrap 管理员的账号 id（只有 `single`）。
    pub fn admin_account_id(&self) -> String {
        self.handle.state.admin.as_ref().expect("single mode has a bootstrap admin").account_id.clone()
    }

    /// `POST /auth/token`，返回 `Cookie` 头可直接使用的 `anycon_session=…`。
    pub async fn login(&self, token: &str) -> String {
        let res = self
            .http
            .post(self.url("/auth/token"))
            .header("x-anycon", "1")
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204, "login failed");
        cookie_pair(&res)
    }

    pub async fn login_admin(&self) -> String {
        assert!(!self.admin_token.is_empty(), "multi mode has no admin token to log in with");
        self.login(&self.admin_token.clone()).await
    }

    pub async fn get(&self, path: &str, cookie: &str) -> reqwest::Response {
        self.http.get(self.url(path)).header("cookie", cookie).send().await.unwrap()
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        cookie: &str,
        body: serde_json::Value,
    ) -> reqwest::Response {
        let mut req = self.http.request(method, self.url(path)).header("x-anycon", "1").header("cookie", cookie);
        if !body.is_null() {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    /// 起一个配了预览（`http://*.preview.localhost`）的 hub。
    pub async fn start_preview() -> Self {
        Self::start_preview_with(|_| {}).await
    }

    pub async fn start_preview_with(tweak: impl FnOnce(&mut Config)) -> Self {
        Self::start_with(|c| {
            c.preview =
                Some(anycon_hub::config::parse_preview_url("http://*.preview.localhost", &c.public_url).unwrap());
            tweak(c);
        })
        .await
    }

    /// `<label>.preview.localhost`：预览域名上的 `Host`。
    pub fn preview_host(label: &str) -> String {
        format!("{label}.{PREVIEW_DOMAIN}")
    }

    /// 浏览器在这个预览页上会带的 `Origin`。
    pub fn preview_origin(label: &str) -> String {
        format!("http://{}", Self::preview_host(label))
    }

    /// 以预览域名的 `Host` 请求（直连测试 hub 的随机端口）。
    pub fn preview_request(&self, method: reqwest::Method, label: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, self.url(path)).header("host", Self::preview_host(label))
    }

    pub async fn preview_get(&self, label: &str, path: &str) -> reqwest::Response {
        self.preview_request(reqwest::Method::GET, label, path).send().await.unwrap()
    }

    /// `POST /api/preview/tokens`（设备 token，`X-Anycon: 1`）。
    pub async fn request_preview_token(&self, device_token: &str, node: &str, label: &str) -> reqwest::Response {
        self.http
            .post(self.url("/api/preview/tokens"))
            .header("x-anycon", "1")
            .header("authorization", format!("Bearer {device_token}"))
            .json(&serde_json::json!({ "node": node, "label": label }))
            .send()
            .await
            .unwrap()
    }

    /// 同 [`request_preview_token`]，要求成功，返回令牌。
    pub async fn preview_token(&self, device: &Seeded, node: &Seeded, label: &str) -> String {
        let res = self.request_preview_token(&device.token, &node.id, label).await;
        assert_eq!(res.status(), 200, "preview token: {}", res.text().await.unwrap());
        let body: serde_json::Value = res.json().await.unwrap();
        body["token"].as_str().expect("token").to_string()
    }

    /// 起一个带记录型发信实现的 hub（邮箱登录 / 绑定 / 邮件通知可用），返回 hub 和那个 mailer。
    pub async fn start_mail() -> (Self, Arc<RecordingMailer>) {
        Self::start_mail_with(|_| {}).await
    }

    pub async fn start_mail_with(tweak: impl FnOnce(&mut Config)) -> (Self, Arc<RecordingMailer>) {
        let mailer = Arc::new(RecordingMailer::default());
        let m = mailer.clone();
        let hub = Self::start_with(move |c| {
            c.mailer_override = Some(m);
            tweak(c);
        })
        .await;
        (hub, mailer)
    }

    /// 起一个 `multi` 模式的 hub：开放注册，配额取默认值，邮箱验证码登录可用（记录型发信实现）。返回 hub 和那个 mailer。
    /// 没有 bootstrap 管理员——`admin_token` 是空串，管理员靠 `admin_emails`（`ANYCON_ADMIN_EMAILS`）。
    pub async fn start_multi() -> (Self, Arc<RecordingMailer>) {
        Self::start_multi_with(|_| {}).await
    }

    pub async fn start_multi_with(tweak: impl FnOnce(&mut Config)) -> (Self, Arc<RecordingMailer>) {
        Self::start_mail_with(move |c| {
            c.enable_multi(QuotaLimits::default());
            tweak(c);
        })
        .await
    }

    /// 等后台的发信 / 推送任务都做完（它们不阻塞 HTTP 响应和信封转发）。
    pub async fn settle(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.handle.state.bg.idle())
            .await
            .expect("background tasks finish");
    }

    /// 用邀请码建一个新账号并登录，返回（账号 id，cookie）。要求调用方已经是管理员。
    pub async fn invite_account(&self, admin_cookie: &str) -> (String, String) {
        let inv: serde_json::Value = self
            .send(reqwest::Method::POST, "/api/invites", admin_cookie, serde_json::json!({}))
            .await
            .json()
            .await
            .unwrap();
        let res = self
            .http
            .post(self.url("/auth/invite"))
            .header("x-anycon", "1")
            .json(&serde_json::json!({ "code": inv["code"], "display_name": "guest" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204, "invite login failed");
        let cookie = cookie_pair(&res);
        let me: serde_json::Value = self.get("/api/me", &cookie).await.json().await.unwrap();
        (me["id"].as_str().unwrap().to_string(), cookie)
    }

    /// 直接往库里塞一个已配对的 daemon / 设备，返回它的 id 和 token。
    pub async fn seed_device(&self, kind: DeviceKind, name: &str) -> Seeded {
        self.seed_device_in(&self.admin_account_id(), kind, name).await
    }

    /// 同 [`seed_device`]，但设备属于指定账号。
    pub async fn seed_device_in(&self, account_id: &str, kind: DeviceKind, name: &str) -> Seeded {
        let id = new_id();
        let token = new_secret();
        self.handle
            .store()
            .create_device(
                NewDevice {
                    id: id.clone(),
                    account_id: account_id.to_string(),
                    kind,
                    name: name.into(),
                    platform: "test".into(),
                    origin: None,
                    token_hash: hash_token(&token),
                },
                now_ms(),
            )
            .await
            .unwrap();
        Seeded { id, token }
    }

    pub async fn seed_paired(&self, a: &Seeded, b: &Seeded) {
        self.handle.store().insert_pairing(&a.id, &b.id, now_ms()).await.unwrap();
    }

    /// 同 [`seed_paired`]，并且让已经在线的连接也立刻认得这个新对端（连接的配对缓存是认证时从库里读的）。
    pub async fn pair_live(&self, a: &Seeded, b: &Seeded) {
        self.seed_paired(a, b).await;
        if let Some(c) = self.handle.state.registry.get(&a.id) {
            c.add_peer(&b.id);
        }
        if let Some(c) = self.handle.state.registry.get(&b.id) {
            c.add_peer(&a.id);
        }
    }
}

#[derive(Clone)]
pub struct Seeded {
    pub id: String,
    pub token: String,
}

/// 从响应里取 `Set-Cookie` 的 `name=value` 部分。
pub fn cookie_pair(res: &reqwest::Response) -> String {
    let raw = res.headers().get("set-cookie").expect("set-cookie header").to_str().unwrap();
    raw.split(';').next().unwrap().to_string()
}

pub fn set_cookie_raw(res: &reqwest::Response) -> String {
    res.headers().get("set-cookie").expect("set-cookie header").to_str().unwrap().to_string()
}

pub fn client_info(kind: DeviceKind, name: &str) -> ClientInfo {
    ClientInfo { name: name.into(), kind, platform: "test".into(), version: "0.0.0".into(), origin: None }
}

/// 收到的东西：信封、或连接被关闭（带 close code）。
#[derive(Debug)]
pub enum Recv {
    Env(Envelope),
    Closed(Option<u16>),
}

pub struct FakeDevice {
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub id: String,
    pub seq: u64,
    pub peers_online: Vec<String>,
}

impl FakeDevice {
    /// 连上并完成 auth，要求成功。`token = None` 是临时（配对）连接。
    pub async fn connect(hub: &TestHub, token: Option<&str>, kind: DeviceKind, name: &str) -> Self {
        Self::connect_with(hub, token, kind, name, &[]).await
    }

    pub async fn connect_with(
        hub: &TestHub,
        token: Option<&str>,
        kind: DeviceKind,
        name: &str,
        headers: &[(&str, &str)],
    ) -> Self {
        let mut raw = Self::open_with(&hub.ws_url(), headers).await.expect("ws upgrade");
        raw.send_auth(token, kind, name).await;
        match raw.recv_control().await {
            Control::AuthOk { device_id, peers_online, .. } => {
                raw.id = device_id;
                raw.peers_online = peers_online;
                raw
            }
            other => panic!("expected auth_ok, got {other:?}"),
        }
    }

    /// 在预览域名上连接并用预览令牌认证，要求成功（`Origin` 是这个预览页的 origin）。
    pub async fn connect_preview(hub: &TestHub, label: &str, token: &str) -> Self {
        let mut raw = Self::open_preview(hub, label).await.expect("preview ws upgrade");
        raw.send_preview_auth(Some(token)).await;
        match raw.recv_control().await {
            Control::AuthOk { device_id, peers_online, .. } => {
                raw.id = device_id;
                raw.peers_online = peers_online;
                raw
            }
            other => panic!("expected auth_ok, got {other:?}"),
        }
    }

    /// 只做预览域名上的 WS 升级（`Host` 和 `Origin` 都是这个预览页的），不发 auth。
    pub async fn open_preview(hub: &TestHub, label: &str) -> Result<Self, tokio_tungstenite::tungstenite::Error> {
        let (host, origin) = (TestHub::preview_host(label), TestHub::preview_origin(label));
        Self::open_with(&hub.ws_url(), &[("host", host.as_str()), ("origin", origin.as_str())]).await
    }

    /// 预览连接的 auth：不带 `device_token`，`client.kind = web`。
    pub async fn send_preview_auth(&mut self, token: Option<&str>) {
        self.send_control(&Control::Auth {
            device_token: None,
            protocol_version: PROTOCOL_VERSION,
            client: client_info(DeviceKind::Web, "preview"),
            preview_token: token.map(str::to_string),
        })
        .await;
    }

    /// 只做 WS 升级，不发 auth。
    pub async fn open_with(url: &str, headers: &[(&str, &str)]) -> Result<Self, tokio_tungstenite::tungstenite::Error> {
        let mut req = url.into_client_request().unwrap();
        for (k, v) in headers {
            req.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        Ok(Self { ws, id: String::new(), seq: 0, peers_online: Vec::new() })
    }

    pub async fn send_auth(&mut self, token: Option<&str>, kind: DeviceKind, name: &str) {
        self.send_control(&Control::Auth {
            device_token: token.map(str::to_string),
            protocol_version: PROTOCOL_VERSION,
            client: client_info(kind, name),
            preview_token: None,
        })
        .await;
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub async fn send_control(&mut self, ctl: &Control) {
        let seq = self.next_seq();
        let env = Envelope::control(self.id.clone(), seq, ctl).unwrap();
        self.send_raw(env.encode().unwrap()).await;
    }

    /// 发一个转发类信封，返回 seq。
    pub async fn send_env(&mut self, to: &str, kind: Kind, body: Vec<u8>) -> u64 {
        let seq = self.next_seq();
        let env = Envelope::new(self.id.clone(), to, kind, seq, body);
        self.send_raw(env.encode().unwrap()).await;
        seq
    }

    /// 同 [`send_env`]，但连接已经被对端关了时返回 `false` 而不是 panic（灌爆类测试用）。
    pub async fn try_send_env(&mut self, to: &str, kind: Kind, body: Vec<u8>) -> bool {
        let seq = self.next_seq();
        let env = Envelope::new(self.id.clone(), to, kind, seq, body);
        self.ws.send(Message::binary(env.encode().unwrap())).await.is_ok()
    }

    pub async fn send_env_as(&mut self, from: &str, to: &str, kind: Kind, body: Vec<u8>) -> u64 {
        let seq = self.next_seq();
        let env = Envelope::new(from, to, kind, seq, body);
        self.send_raw(env.encode().unwrap()).await;
        seq
    }

    pub async fn send_raw(&mut self, bytes: Vec<u8>) {
        self.ws.send(Message::binary(bytes)).await.expect("ws send");
    }

    /// 同 [`send_raw`]，但对端已经关了连接时返回 `false` 而不是 panic（灌爆类测试用）。
    pub async fn try_send_control(&mut self, ctl: &Control) -> bool {
        let seq = self.next_seq();
        let env = Envelope::control(self.id.clone(), seq, ctl).unwrap();
        self.ws.send(Message::binary(env.encode().unwrap())).await.is_ok()
    }

    /// 取下一个信封或关闭事件（跳过 ping / pong），超时 panic。
    pub async fn recv(&mut self) -> Recv {
        self.recv_within(RECV_TIMEOUT).await.expect("timed out waiting for a frame")
    }

    pub async fn recv_within(&mut self, dur: Duration) -> Option<Recv> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return None,
                Ok(None) => return Some(Recv::Closed(None)),
                Ok(Some(Err(_))) => return Some(Recv::Closed(None)),
                Ok(Some(Ok(Message::Binary(b)))) => {
                    return Some(Recv::Env(Envelope::decode(&b).expect("valid envelope")))
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Some(Recv::Closed(frame.map(|f| u16::from(f.code))));
                }
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    pub async fn recv_env(&mut self) -> Envelope {
        match self.recv().await {
            Recv::Env(e) => e,
            Recv::Closed(c) => panic!("connection closed ({c:?}) while waiting for an envelope"),
        }
    }

    pub async fn recv_control(&mut self) -> Control {
        let env = self.recv_env().await;
        assert_eq!(env.kind, Kind::Control, "expected a control envelope, got {env:?}");
        env.control_body().expect("valid control body")
    }

    /// 期望的下一个事件是关闭，返回 close code。
    pub async fn expect_close(&mut self) -> Option<u16> {
        match self.recv().await {
            Recv::Closed(code) => code,
            Recv::Env(e) => panic!("expected close, got {e:?}"),
        }
    }

    /// 在 `dur` 内什么都不该收到（连接保持打开）。
    pub async fn expect_silence(&mut self, dur: Duration) {
        if let Some(r) = self.recv_within(dur).await {
            panic!("expected silence, got {r:?}");
        }
    }

    /// 用 `match_id` 发起配对，要求收到 `PairRegistered`，返回 `expires_at_ms`。
    pub async fn pair_begin(&mut self, match_id: &str) -> u64 {
        self.send_control(&Control::PairBegin { match_id: match_id.to_string() }).await;
        match self.recv_control().await {
            Control::PairRegistered { expires_at_ms } => expires_at_ms,
            other => panic!("expected pair_registered, got {other:?}"),
        }
    }

    /// 取下一帧的原始字节（不解码），超时 panic。跳过 ping / pong。
    pub async fn recv_raw(&mut self) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => panic!("timed out waiting for a frame"),
                Ok(Some(Ok(Message::Binary(b)))) => return b.to_vec(),
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                Ok(other) => panic!("expected a binary frame, got {other:?}"),
            }
        }
    }

    /// 在 `dur` 内收下所有控制消息（出现别的东西 panic）；一直读到安静下来 / 时间到。
    pub async fn drain_controls(&mut self, dur: Duration) -> Vec<Control> {
        let deadline = tokio::time::Instant::now() + dur;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.recv_within(left).await {
                None => return out,
                Some(Recv::Env(e)) => {
                    assert_eq!(e.kind, Kind::Control, "unexpected non-control frame: {e:?}");
                    out.push(e.control_body().expect("valid control body"));
                }
                Some(Recv::Closed(c)) => panic!("connection closed ({c:?}) while draining"),
            }
        }
    }
}

/// 随机短码和对应的 `match_id`——hub 只会看到后者。
pub fn new_code() -> (String, String) {
    let code = anycon_crypto::generate_pair_code();
    let match_id = anycon_crypto::derive_match_id(&code).expect("generated codes are valid");
    (code, match_id)
}

pub fn close_code_num(c: CloseCode) -> u16 {
    u16::from(c)
}

// ---------------------------------------------------------------------------
// 邮件
// ---------------------------------------------------------------------------

/// 从验证码邮件的正文里取出 6 位码（正文里写成 `123 456`）。
pub fn code_in(mail: &OutgoingMail) -> String {
    let b = mail.body.as_bytes();
    for i in 0..b.len().saturating_sub(6) {
        let w = &b[i..i + 7];
        if w[..3].iter().all(u8::is_ascii_digit) && w[3] == b' ' && w[4..].iter().all(u8::is_ascii_digit) {
            // 前后不能还是数字（不是更长数字串的一部分）
            let before_ok = i == 0 || !b[i - 1].is_ascii_digit();
            let after_ok = b.get(i + 7).is_none_or(|c| !c.is_ascii_digit());
            if before_ok && after_ok {
                return format!("{}{}", &mail.body[i..i + 3], &mail.body[i + 4..i + 7]);
            }
        }
    }
    panic!("no 6-digit code in the mail body: {:?}", mail.body);
}

/// 管理员绑定一个邮箱（走完发码 → 验证），返回规范化后的邮箱。
pub async fn bind_email(hub: &TestHub, mailer: &RecordingMailer, cookie: &str, email: &str) {
    let before = mailer.sent().len();
    let res = hub.send(reqwest::Method::POST, "/api/me/email", cookie, serde_json::json!({ "email": email })).await;
    assert_eq!(res.status(), 200, "bind send: {}", res.text().await.unwrap());
    hub.settle().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), before + 1, "one code mail");
    let code = code_in(sent.last().unwrap());
    let res = hub
        .send(
            reqwest::Method::POST,
            "/api/me/email/verify",
            cookie,
            serde_json::json!({ "email": email, "code": code }),
        )
        .await;
    assert_eq!(res.status(), 200, "bind verify: {}", res.text().await.unwrap());
}

// ---------------------------------------------------------------------------
// 假推送服务和假浏览器订阅
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PushRequest {
    pub method: String,
    pub path: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

impl PushRequest {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
            .unwrap_or_else(|| panic!("no {name} header: {:?}", self.headers))
    }
}

/// 本机的假推送服务：记下收到的请求，按设定的状态码回应。
pub struct FakePush {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<PushRequest>>>,
    status: Arc<std::sync::atomic::AtomicU16>,
    delay_ms: Arc<std::sync::atomic::AtomicU64>,
    max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

impl FakePush {
    pub async fn start() -> Self {
        use axum::body::Bytes;
        use axum::extract::{Request, State};
        use axum::http::StatusCode;

        use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};

        #[derive(Clone)]
        struct Shared {
            requests: Arc<Mutex<Vec<PushRequest>>>,
            status: Arc<AtomicU16>,
            delay_ms: Arc<AtomicU64>,
            in_flight: Arc<AtomicUsize>,
            max_in_flight: Arc<AtomicUsize>,
        }
        let shared = Shared {
            requests: Arc::default(),
            status: Arc::new(AtomicU16::new(201)),
            delay_ms: Arc::default(),
            in_flight: Arc::default(),
            max_in_flight: Arc::default(),
        };
        let app = axum::Router::new().fallback(|State(s): State<Shared>, req: Request| async move {
            let now = s.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            s.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let (parts, body) = req.into_parts();
            let body: Bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
            let headers = parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
                .collect();
            s.requests.lock().unwrap().push(PushRequest {
                method: parts.method.to_string(),
                path: parts.uri.path().to_string(),
                headers,
                body: body.to_vec(),
            });
            let delay = s.delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            s.in_flight.fetch_sub(1, Ordering::SeqCst);
            StatusCode::from_u16(s.status.load(Ordering::SeqCst)).unwrap()
        });
        let app = app.with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            requests: shared.requests,
            status: shared.status,
            delay_ms: shared.delay_ms,
            max_in_flight: shared.max_in_flight,
        }
    }

    /// 之后的每个请求先等这么久再回应（模拟慢的推送服务）。
    pub fn set_delay(&self, ms: u64) {
        self.delay_ms.store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// 同时在途的请求数的最大值。
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 订阅用的 endpoint（`http://127.0.0.1:<port>/<path>`）。
    pub fn endpoint(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{path}", self.addr.port())
    }

    pub fn set_status(&self, status: u16) {
        self.status.store(status, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<PushRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 等到至少收到 `n` 个请求（最多 5 秒）。
    pub async fn wait_for(&self, n: usize) -> Vec<PushRequest> {
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            let got = self.requests();
            if got.len() >= n {
                return got;
            }
            assert!(tokio::time::Instant::now() < deadline, "expected {n} push request(s), got {}", got.len());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// 假浏览器：一对订阅密钥（p256dh 私钥 + auth 密钥）。
pub struct Browser {
    pub secret: p256::SecretKey,
    pub auth: [u8; 16],
}

impl Browser {
    pub fn new() -> Self {
        use rand::RngCore;
        let mut auth = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut auth);
        Self { secret: anycon_hub::push::webpush::random_secret_key(), auth }
    }

    pub fn p256dh(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(anycon_hub::push::webpush::uncompressed(&self.secret.public_key()))
    }

    pub fn auth_b64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.auth)
    }

    /// `POST /api/push/subscriptions` 的请求体。
    pub fn subscription(&self, device_id: &str, endpoint: &str) -> serde_json::Value {
        serde_json::json!({
            "device_id": device_id,
            "endpoint": endpoint,
            "keys": { "p256dh": self.p256dh(), "auth": self.auth_b64() },
        })
    }

    /// 解开推送请求体，得到载荷 JSON。
    pub fn decrypt(&self, req: &PushRequest) -> serde_json::Value {
        let plain = anycon_hub::push::webpush::decrypt(&req.body, &self.secret, &self.auth).expect("payload decrypts");
        serde_json::from_slice(&plain).expect("payload is JSON")
    }
}

/// 一封 `notify` 信封的 body（明文外层，`detail` 是个不该被 hub 读的占位）。
pub fn notify_body(category: anycon_protocol::NotifyCategory, session: Option<&str>) -> Vec<u8> {
    rmp_serde::to_vec_named(&anycon_protocol::NotifyBody {
        category,
        session_id: session.map(str::to_string),
        detail: Some("opaque-noise-ciphertext".into()),
    })
    .unwrap()
}
