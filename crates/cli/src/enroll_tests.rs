//! `enroll.rs` 的测试：进程内的**假 hub**（HTTP 登录 / 设备列表 / WebSocket 撮合）+ 真 daemon 的接入引擎（responder）。
//!
//! 假 hub 做的事和真的一样少：认证、`enroll_begin` → `enroll_request` / `enroll_started`、在 CLI 和 daemon 之间转发 `handshake`
//! 和 `pair.ack`、两边 `pair_confirm` 之后发 `pair_done`（临时连接签发设备 id 和 token）、`enroll_cancel`。
//! 经真实 hub 的端到端见 `crates/cli/tests/enroll_real_hub.rs` 和 `e2e/tests/enroll.spec.ts`。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_daemon::config::Paths;
use anycon_daemon::enroll_initiator::parse_scan_qr;
use anycon_daemon::testutil::{FakeHubLink, LinkOut, TestDaemon};
use anycon_protocol::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use zeroize::Zeroizing;

use crate::cli::ViaArg;
use crate::enroll::*;
use crate::error::{CliError, Result, EXIT_DENIED, EXIT_ERROR, EXIT_INTERRUPTED, EXIT_OFFLINE, EXIT_USAGE};
use crate::remote::HubState;
use anycon_daemon::hub::HttpResponse;

const PW: &str = "correct horse battery";
const GOOD_TOKEN: &str = "good-account-token";
const GOOD_EMAIL_CODE: &str = "246810";
const SESSION: &str = "anycon_session=sess-abc";
const DEVICE_TOKEN: &str = "device-token-xyz";
const T: Duration = Duration::from_secs(20);

// ====================================================================== 脚本化的终端

enum Answer {
    Text(String),
    /// 测试在别处才知道答案（比如批准之后才有的码）：阻塞等。
    Wait(Mutex<std::sync::mpsc::Receiver<String>>),
    Interrupt,
}

#[derive(Default)]
struct ScriptedUi {
    said: Mutex<Vec<String>>,
    qr: Mutex<Vec<String>>,
    /// （提示里应有的子串，答案）：按顺序，第一个子串匹配的取走。
    answers: Mutex<VecDeque<(&'static str, Answer)>>,
    asked: Mutex<Vec<String>>,
    unexpected: Mutex<Vec<String>>,
}

impl ScriptedUi {
    fn script(self, key: &'static str, a: Answer) -> Self {
        self.answers.lock().unwrap().push_back((key, a));
        self
    }
    fn text(self, key: &'static str, t: &str) -> Self {
        self.script(key, Answer::Text(t.into()))
    }
    fn answer(&self, prompt: &str) -> Result<String> {
        self.asked.lock().unwrap().push(prompt.to_string());
        let mut a = self.answers.lock().unwrap();
        let Some(i) = a.iter().position(|(k, _)| prompt.contains(k)) else {
            self.unexpected.lock().unwrap().push(prompt.to_string());
            return Err(CliError::general(format!("unexpected prompt {prompt:?}")));
        };
        let (_, ans) = a.remove(i).unwrap();
        drop(a);
        match ans {
            Answer::Text(t) => Ok(t),
            Answer::Wait(rx) => {
                rx.lock().unwrap().recv_timeout(Duration::from_secs(30)).map_err(|e| CliError::general(e.to_string()))
            }
            Answer::Interrupt => Err(CliError::interrupted()),
        }
    }
}

impl Ui for ScriptedUi {
    fn say(&self, msg: &str) {
        self.said.lock().unwrap().push(msg.to_string());
    }
    fn ask(&self, prompt: &str) -> Result<Zeroizing<String>> {
        self.answer(prompt).map(Zeroizing::new)
    }
    fn ask_secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        self.answer(prompt).map(Zeroizing::new)
    }
    fn show_qr(&self, payload: &str) {
        self.qr.lock().unwrap().push(payload.to_string());
    }
}

// ====================================================================== 假 hub

struct Conn {
    /// 发给这条连接的字节（已编码的信封）。
    tx: mpsc::UnboundedSender<Vec<u8>>,
    is_temp: bool,
    client_confirmed: bool,
    enroll_id: Option<String>,
}

#[derive(Default)]
struct Shared {
    /// 连接 id（临时 id 或正式 id）→ 连接。
    conns: Mutex<HashMap<String, Conn>>,
    /// 每条 WebSocket 升级请求带的 Cookie 头。
    ws_cookies: Mutex<Vec<Option<String>>>,
    /// HTTP 请求记录："POST /auth/token" 之类，登出带上 cookie。
    http: Mutex<Vec<String>>,
    cancels: Mutex<Vec<String>>,
    begins: Mutex<Vec<String>>,
    daemon_confirmed: Mutex<bool>,
    login_methods: Mutex<Vec<String>>,
    /// 账号里除了这台节点还有别的 daemon（选节点 / 歧义的用例才开）。
    extra_nodes: Mutex<bool>,
    /// hub 给 CLI 签发的设备 id（之后带 token 重连得到的还是它）。
    issued: Mutex<Option<String>>,
    /// `enroll_started` 里的有效期（毫秒）；默认 5 分钟。
    started_ttl_ms: Mutex<Option<u64>>,
    /// 非空时 `enroll_begin` 直接以这个原因失败。
    begin_failure: Mutex<Option<EnrollFailReason>>,
    /// 为真时 `GET /api/devices` 不认设备 token（像吊销了，或者 hub 太旧）。
    reject_bearer: Mutex<bool>,
    /// `POST /auth/email/send` 收到的邮箱（原样）。
    email_sends: Mutex<Vec<String>>,
    /// `POST /auth/email/verify` 收到的（邮箱，码）。
    email_verifies: Mutex<Vec<(String, String)>>,
    /// 非空时 `/auth/email/send` 以这个状态码失败（429 带 `Retry-After: 37`，形状和真 hub 一样）。
    email_send_status: Mutex<Option<u16>>,
    seq: AtomicU32,
}

struct FakeHub {
    url: String,
    shared: Arc<Shared>,
    node_id: String,
}

fn envelope_bytes(from: &str, to: &str, kind: Kind, seq: u64, body: Vec<u8>) -> Vec<u8> {
    Envelope::new(from, to, kind, seq, body).encode().unwrap()
}

fn control_bytes(seq: u64, c: &Control) -> Vec<u8> {
    Envelope::control("", seq, c).unwrap().encode().unwrap()
}

impl FakeHub {
    async fn start(td: Arc<TestDaemon>) -> FakeHub {
        let link = FakeHubLink::attach(&td);
        let node_id = link.node_id.clone();
        let shared = Arc::new(Shared { login_methods: Mutex::new(vec!["token".into()]), ..Default::default() });
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        // daemon 的出站：转给对应的 WebSocket
        {
            let (shared, td) = (shared.clone(), td.clone());
            let mut link = link;
            let node_id = node_id.clone();
            tokio::spawn(async move {
                while let Some(o) = link.next().await {
                    let seq = u64::from(shared.seq.fetch_add(1, Ordering::SeqCst)) + 1;
                    match o {
                        LinkOut::Frame { to, frame } => {
                            let body = rmp_serde::to_vec_named(&frame).unwrap();
                            send_to(&shared, &to, envelope_bytes(&node_id, &to, Kind::Handshake, seq, body));
                        }
                        LinkOut::Data { to, body } => {
                            send_to(&shared, &to, envelope_bytes(&node_id, &to, Kind::Data, seq, body));
                        }
                        LinkOut::Control(Control::PairConfirm { peer }) => {
                            *shared.daemon_confirmed.lock().unwrap() = true;
                            try_finish(&shared, &td, &peer).await;
                        }
                        LinkOut::Control(Control::EnrollCancel { enroll_id }) => {
                            // 另一端（CLI）收到 enroll_failed { cancelled }
                            let target = shared
                                .conns
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|(_, c)| c.enroll_id.as_deref() == Some(enroll_id.as_str()))
                                .map(|(id, _)| id.clone());
                            if let Some(id) = target {
                                let c = Control::EnrollFailed {
                                    enroll_id: Some(enroll_id),
                                    reason: EnrollFailReason::Cancelled,
                                };
                                send_to(&shared, &id, control_bytes(seq, &c));
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        let (s2, td2, node2) = (shared.clone(), td.clone(), node_id.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = l.accept().await else { return };
                let (s, td, node) = (s2.clone(), td2.clone(), node2.clone());
                tokio::spawn(async move { serve(stream, s, td, node).await });
            }
        });
        FakeHub { url, shared, node_id }
    }

    fn http_log(&self) -> Vec<String> {
        self.shared.http.lock().unwrap().clone()
    }
}

fn send_to(shared: &Shared, id: &str, bytes: Vec<u8>) {
    if let Some(c) = shared.conns.lock().unwrap().get(id) {
        let _ = c.tx.send(bytes);
    }
}

/// 两边都 pair_confirm 了：签发身份、发 pair_done。
async fn try_finish(shared: &Arc<Shared>, td: &Arc<TestDaemon>, peer: &str) {
    let (formal, tx, is_temp) = {
        let mut conns = shared.conns.lock().unwrap();
        let Some(c) = conns.get(peer) else { return };
        if !c.client_confirmed || !*shared.daemon_confirmed.lock().unwrap() {
            return;
        }
        let tx = c.tx.clone();
        let is_temp = c.is_temp;
        let formal = if is_temp {
            format!("cli-formal-{:0>20}", shared.seq.fetch_add(1, Ordering::SeqCst))
        } else {
            peer.to_string()
        };
        if is_temp {
            *shared.issued.lock().unwrap() = Some(formal.clone());
        }
        if let Some(mut c) = conns.remove(peer) {
            c.is_temp = false;
            conns.insert(formal.clone(), c);
        }
        (formal, tx, is_temp)
    };
    *shared.daemon_confirmed.lock().unwrap() = false;
    let node_id = td_node_id(td);
    let done = Control::PairDone {
        peer: node_id,
        device_id: is_temp.then(|| formal.clone()),
        device_token: is_temp.then(|| DEVICE_TOKEN.to_string()),
    };
    let _ = tx.send(control_bytes(900, &done));
    td.ctx.peers.on_control(Control::PairDone { peer: formal, device_id: None, device_token: None }).await;
}

fn td_node_id(td: &TestDaemon) -> String {
    td.ctx.outbox.device_id().unwrap_or_default()
}

async fn read_head(s: &mut TcpStream) -> Option<(String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            let mut lines = head.split("\r\n");
            let request_line = lines.next()?.to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len: usize =
                headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
            let mut body = buf[i + 4..].to_vec();
            while body.len() < len {
                let n = s.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            return Some((request_line, headers, body));
        }
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn serve(mut stream: TcpStream, shared: Arc<Shared>, td: Arc<TestDaemon>, node_id: String) {
    // 看一眼请求行，WebSocket 升级交给 tungstenite（它自己读请求），其余自己答
    let mut peek = [0u8; 32];
    let n = stream.peek(&mut peek).await.unwrap_or(0);
    if String::from_utf8_lossy(&peek[..n]).starts_with("GET /v1/ws") {
        return ws_session(stream, shared, td, node_id).await;
    }
    let Some((line, headers, body)) = read_head(&mut stream).await else { return };
    let header = |k: &str| headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.clone());
    let (status, extra, payload) = match line.split(' ').take(2).collect::<Vec<_>>()[..] {
        ["GET", "/api/config"] => {
            shared.http.lock().unwrap().push("GET /api/config".into());
            let methods = shared.login_methods.lock().unwrap().clone();
            (200, String::new(), json!({ "mode": "multi", "login_methods": methods }).to_string())
        }
        ["POST", "/auth/token"] => {
            shared.http.lock().unwrap().push("POST /auth/token".into());
            let csrf = header("x-anycon").as_deref() == Some("1");
            let ok = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["token"].as_str().map(str::to_string))
                .as_deref()
                == Some(GOOD_TOKEN);
            if !csrf {
                (403, String::new(), json!({ "error": "csrf", "message": "missing X-Anycon" }).to_string())
            } else if ok {
                (204, format!("Set-Cookie: {SESSION}; Path=/; HttpOnly\r\n"), String::new())
            } else {
                (401, String::new(), json!({ "error": "invalid_token", "message": "invalid token" }).to_string())
            }
        }
        ["POST", "/auth/email/send"] => {
            shared.http.lock().unwrap().push("POST /auth/email/send".into());
            let csrf = header("x-anycon").as_deref() == Some("1");
            let email = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["email"].as_str().map(str::to_string))
                .unwrap_or_default();
            shared.email_sends.lock().unwrap().push(email);
            let forced = *shared.email_send_status.lock().unwrap();
            if !csrf {
                (403, String::new(), json!({ "error": { "code": "csrf", "message": "missing X-Anycon" } }).to_string())
            } else if forced == Some(429) {
                (
                    429,
                    "Retry-After: 37\r\n".into(),
                    json!({ "error": { "code": "rate_limited", "message": "too many requests", "retry_after": 37 } })
                        .to_string(),
                )
            } else {
                // 和真 hub 一样：绑定没绑定都是这个回答
                (200, String::new(), json!({ "ok": true, "resend_after_secs": 60 }).to_string())
            }
        }
        ["POST", "/auth/email/verify"] => {
            shared.http.lock().unwrap().push("POST /auth/email/verify".into());
            let csrf = header("x-anycon").as_deref() == Some("1");
            let v = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
            let email = v["email"].as_str().unwrap_or_default().to_string();
            let code = v["code"].as_str().unwrap_or_default().to_string();
            shared.email_verifies.lock().unwrap().push((email, code.clone()));
            if !csrf {
                (403, String::new(), json!({ "error": { "code": "csrf", "message": "missing X-Anycon" } }).to_string())
            } else if code == GOOD_EMAIL_CODE {
                (204, format!("Set-Cookie: {SESSION}; Path=/; HttpOnly\r\n"), String::new())
            } else {
                (
                    401,
                    String::new(),
                    json!({ "error": { "code": "invalid_code", "message": "invalid or expired code" } }).to_string(),
                )
            }
        }
        ["GET", "/api/devices"] => {
            // 和真 hub 一样：带了 Authorization 就只看它（设备 token），否则看登录 cookie
            let bearer = header("authorization");
            let authed = match &bearer {
                Some(b) => b == &format!("Bearer {DEVICE_TOKEN}") && !*shared.reject_bearer.lock().unwrap(),
                None => header("cookie").as_deref() == Some(SESSION),
            };
            let tag = if bearer.is_some() { " bearer" } else { "" };
            shared.http.lock().unwrap().push(format!("GET /api/devices{tag}"));
            if !authed {
                (401, String::new(), json!({ "error": "unauthorized", "message": "sign in" }).to_string())
            } else {
                let mut list = vec![
                    json!({ "id": node_id, "name": "mac-mini", "kind": "daemon", "online": true }),
                    json!({ "id": "d".repeat(32), "name": "Chrome", "kind": "web", "online": true }),
                ];
                if *shared.extra_nodes.lock().unwrap() {
                    list.push(json!({ "id": "b".repeat(32), "name": "build-box", "kind": "daemon", "online": false }));
                    list.push(json!({ "id": "c".repeat(32), "name": "build-box", "kind": "daemon", "online": false }));
                }
                (200, String::new(), Value::Array(list).to_string())
            }
        }
        ["POST", "/auth/logout"] => {
            shared
                .http
                .lock()
                .unwrap()
                .push(format!("POST /auth/logout cookie={}", header("cookie").unwrap_or_default()));
            (204, "Set-Cookie: anycon_session=; Path=/; Max-Age=0\r\n".into(), String::new())
        }
        _ => (404, String::new(), String::new()),
    };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Not Found",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
}

async fn ws_session(stream: TcpStream, shared: Arc<Shared>, td: Arc<TestDaemon>, node_id: String) {
    let mut cookie: Option<String> = None;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    #[allow(clippy::result_large_err)]
    let cb = |req: &Request, resp: Response| -> std::result::Result<Response, ErrorResponse> {
        cookie = req.headers().get("cookie").and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(resp)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else { return };
    shared.ws_cookies.lock().unwrap().push(cookie.clone());
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut my_id = String::new();
    let rpc = td.rpc();
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(b) = out else { return };
                if sink.send(Message::Binary(b.into())).await.is_err() { return; }
            }
            msg = source.next() => {
                let Some(Ok(Message::Binary(b))) = msg else { return };
                let env = Envelope::decode(&b).unwrap();
                match env.kind {
                    Kind::Control => match env.control_body().unwrap() {
                        Control::Auth { device_token, .. } => {
                            let (id, is_temp) = match device_token.as_deref() {
                                Some(DEVICE_TOKEN) => {
                                    (shared.issued.lock().unwrap().clone().unwrap_or_else(|| "cli-formal-existing".to_string()), false)
                                }
                                Some(_) => {
                                    let _ = sink.send(Message::Binary(control_bytes(1, &Control::AuthFail { reason: AuthFailReason::Invalid }).into())).await;
                                    return;
                                }
                                None => (format!("temp-{}", shared.seq.fetch_add(1, Ordering::SeqCst)), true),
                            };
                            my_id = id.clone();
                            shared.conns.lock().unwrap().insert(id.clone(), Conn { tx: tx.clone(), is_temp, client_confirmed: false, enroll_id: None });
                            let ok = Control::AuthOk { device_id: id, server_time_ms: 0, peers_online: vec![], quota: None };
                            let _ = tx.send(control_bytes(1, &ok));
                        }
                        Control::EnrollBegin { node } => {
                            shared.begins.lock().unwrap().push(node.clone());
                            // 临时连接要靠 cookie 证明账号；正式设备（带 token）自带账号
                            let is_temp = shared.conns.lock().unwrap().get(&my_id).is_some_and(|c| c.is_temp);
                            let forced = shared.begin_failure.lock().unwrap().take();
                            let reply = if let Some(reason) = forced {
                                Control::EnrollFailed { enroll_id: None, reason }
                            } else if is_temp && cookie.as_deref() != Some(SESSION) {
                                Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::Forbidden }
                            } else if node != node_id {
                                Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::NotFound }
                            } else {
                                let enroll_id = format!("{:032x}", shared.seq.fetch_add(1, Ordering::SeqCst) + 1000);
                                if let Some(c) = shared.conns.lock().unwrap().get_mut(&my_id) { c.enroll_id = Some(enroll_id.clone()); }
                                td.ctx.enroll.on_request(enroll_id.clone(), my_id.clone()).await;
                                let ttl = shared.started_ttl_ms.lock().unwrap().unwrap_or(300_000);
                                Control::EnrollStarted { enroll_id, node, expires_at_ms: anycon_daemon::util::now_ms() + ttl }
                            };
                            let _ = tx.send(control_bytes(2, &reply));
                        }
                        Control::PairConfirm { .. } => {
                            if let Some(c) = shared.conns.lock().unwrap().get_mut(&my_id) { c.client_confirmed = true; }
                            try_finish(&shared, &td, &my_id).await;
                            // try_finish 可能把连接改键成正式 id
                            let moved = shared.conns.lock().unwrap().iter().find(|(_, c)| c.tx.same_channel(&tx)).map(|(k, _)| k.clone());
                            if let Some(k) = moved { my_id = k; }
                        }
                        Control::EnrollCancel { enroll_id } => {
                            shared.cancels.lock().unwrap().push(enroll_id.clone());
                            td.ctx.enroll.on_failed(Some(enroll_id), EnrollFailReason::Cancelled).await;
                        }
                        _ => {}
                    },
                    Kind::Handshake => td.ctx.peers.on_handshake(&my_id, &env.body, &rpc).await,
                    _ => {}
                }
            }
        }
    }
}

// ====================================================================== 夹具

struct Rig {
    td: Arc<TestDaemon>,
    hub: FakeHub,
    home: tempfile::TempDir,
}

impl Rig {
    async fn new() -> Rig {
        let td = Arc::new(TestDaemon::start_with(&[], false).await);
        let hub = FakeHub::start(td.clone()).await;
        Rig { td, hub, home: tempfile::tempdir().unwrap() }
    }

    fn env(&self) -> EnrollEnv {
        EnrollEnv {
            paths: Paths::new(self.home.path()),
            hub_url: self.hub.url.clone(),
            device_name: "cli-test".into(),
            ack_timeout: Duration::from_secs(5),
        }
    }

    fn req(&self, node: Option<&str>, via: Option<ViaArg>) -> EnrollRequest {
        EnrollRequest { node: node.map(str::to_string), via, email: None }
    }

    async fn set_password(&self) {
        let mut c = self.td.client("setup");
        c.call::<Value>(&self.td, methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": PW })).await.unwrap();
    }

    /// 以另一台已配对设备的身份调 daemon 的 RPC。
    async fn rpc<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<R, RpcError> {
        self.td.client("paired-phone").call(&self.td, method, params).await
    }

    fn state(&self) -> HubState {
        let paths = Paths::new(self.home.path());
        let dir = paths.client_dir(&crate::remote::host_of(&self.hub.url).unwrap());
        HubState::load(&dir, &self.hub.url)
    }
}

async fn run(rig: &Rig, ui: Arc<ScriptedUi>, req: EnrollRequest) -> Result<Joined> {
    let ui_dyn: Arc<dyn Ui> = ui;
    tokio::time::timeout(T, enroll(&rig.env(), &ui_dyn, req)).await.expect("enroll finishes in time")
}

fn err(r: Result<Joined>) -> CliError {
    match r {
        Ok(j) => panic!("expected an error, joined {}", j.node.name),
        Err(e) => e,
    }
}

// ====================================================================== 节点匹配

fn dev(id: &str, name: &str, kind: &str) -> AccountDevice {
    AccountDevice { id: id.into(), name: name.into(), kind: kind.into(), online: true }
}

#[test]
fn node_matching_prefers_ids_then_unique_names_then_unique_prefixes_and_only_daemons_count() {
    let list = vec![
        dev("aaaa1111", "mac-mini", "daemon"),
        dev("aaaa2222", "build-box", "daemon"),
        dev("bbbb1111", "build-box", "daemon"),
        dev("cccc1111", "Chrome", "web"),
        dev("dddd1111", "laptop-cli", "cli"),
    ];
    let one = |key: &str| match pick_node(&list, Some(key)).unwrap() {
        NodePick::One(d) => d.id.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(one("aaaa1111"), "aaaa1111", "an exact id");
    assert_eq!(one("mac-mini"), "aaaa1111", "a unique name");
    assert_eq!(one("aaaa2"), "aaaa2222", "a unique id prefix");
    assert_eq!(one("bbbb"), "bbbb1111");
    // 歧义：名字重复 / 前缀不唯一 -> 参数错误，列出候选
    let e = pick_node(&list, Some("build-box")).unwrap_err();
    assert_eq!((e.exit, e.code.as_str()), (EXIT_USAGE, "invalid"));
    assert!(e.message.contains("several nodes") && e.message.contains("aaaa2222"), "{}", e.message);
    assert_eq!(pick_node(&list, Some("aaaa")).unwrap_err().exit, EXIT_USAGE);
    // 找不到 / 不是 daemon
    for key in ["nope", "Chrome", "laptop-cli", "cccc1111", ""] {
        let e = pick_node(&list, Some(key)).unwrap_err();
        assert_eq!(e.exit, EXIT_USAGE, "{key:?}");
        assert!(e.message.contains("no node in this account"), "{}", e.message);
    }
    // 没给 --node：一台直接用，多台要选，没有是错误
    assert!(matches!(pick_node(&list[..1], None).unwrap(), NodePick::One(d) if d.id == "aaaa1111"));
    assert!(matches!(pick_node(&list, None).unwrap(), NodePick::Choose(v) if v.len() == 3));
    assert_eq!(pick_node(&list[3..], None).unwrap_err().exit, EXIT_USAGE);
    assert_eq!(pick_node(&[], None).unwrap_err().exit, EXIT_USAGE);
}

#[test]
fn enroll_failures_and_rejections_map_to_the_documented_exit_codes() {
    // hub 的 enroll_failed：找不到 = 2，离线 = 3，超时 / 限速 / 配额 / 忙 = 1
    assert_eq!(enroll_failed(EnrollFailReason::NotFound).exit, EXIT_USAGE);
    assert_eq!(enroll_failed(EnrollFailReason::Offline).exit, EXIT_OFFLINE);
    assert_eq!(enroll_failed(EnrollFailReason::PeerGone).exit, EXIT_OFFLINE);
    for r in [
        EnrollFailReason::Timeout,
        EnrollFailReason::RateLimited,
        EnrollFailReason::Quota,
        EnrollFailReason::Busy,
        EnrollFailReason::Cancelled,
    ] {
        assert_eq!(enroll_failed(r).exit, EXIT_ERROR, "{r:?}");
    }
    assert_eq!(enroll_failed(EnrollFailReason::Forbidden).exit, EXIT_DENIED);
    // daemon 的 rejected：码 / 口令不对、被拒绝、方式没开启、退避中、已锁定 = 5
    for r in [
        EnrollRejectReason::WrongSecret,
        EnrollRejectReason::Denied,
        EnrollRejectReason::MethodDisabled,
        EnrollRejectReason::PasswordNotSet,
        EnrollRejectReason::Backoff,
        EnrollRejectReason::Locked,
        EnrollRejectReason::NoCode,
    ] {
        assert_eq!(rejected_error(r, Some(61_000)).exit, EXIT_DENIED, "{r:?}");
    }
    // 退避：写明还要等多久；锁定：写明怎么解
    assert!(rejected_error(EnrollRejectReason::Backoff, Some(61_000)).message.contains("61 s"));
    assert!(rejected_error(EnrollRejectReason::Locked, None).message.contains("anycon passwd --enable"));
    for r in [EnrollRejectReason::Busy, EnrollRejectReason::Expired, EnrollRejectReason::Invalid] {
        assert_eq!(rejected_error(r, None).exit, EXIT_ERROR, "{r:?}");
    }
    assert_eq!(pretty_code("123456"), "123-456");
}

// ====================================================================== 四种方式的成功路径

fn assert_saved(rig: &Rig, joined: &Joined) {
    // 校验了 pair.ack 才落盘：节点 id / 公钥对得上，拿到了设备身份
    assert_eq!(joined.node.id, rig.hub.node_id);
    assert_eq!(joined.node.pubkey, hex::encode(rig.td.ctx.identity.public()));
    let st = rig.state();
    assert!(st.problem.is_none());
    assert_eq!(st.nodes.len(), 1);
    assert_eq!((st.nodes[0].id.as_str(), st.nodes[0].name.as_str()), (rig.hub.node_id.as_str(), "test-node"));
    assert_eq!(st.token.as_deref(), Some(DEVICE_TOKEN));
    assert!(st.device_id.as_deref().is_some_and(|d| d.starts_with("cli-formal-")));
    assert!(st.secret.is_some());
    // daemon 那边：设备登记在正式 id 下，用的是握手第 3 条里的 ClientInfo
    let devices = rig.td.ctx.peers.list();
    assert_eq!(devices.len(), 1);
    assert_eq!(
        (devices[0].device_id.as_str(), devices[0].name.as_str(), devices[0].kind),
        (st.device_id.as_deref().unwrap(), "cli-test", DeviceKind::Cli)
    );
}

#[tokio::test]
async fn password_enrollment_signs_in_picks_the_node_and_saves_it_after_the_ack() {
    let rig = Rig::new().await;
    rig.set_password().await;
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let joined = run(&rig, ui.clone(), rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    assert_eq!(joined.method, EnrollMethod::Password);
    assert_saved(&rig, &joined);
    // 登录态只用于这一次升级请求，结束时登出
    assert_eq!(rig.hub.shared.ws_cookies.lock().unwrap().clone(), vec![Some(SESSION.to_string())]);
    let log = rig.hub.http_log();
    assert_eq!(
        log,
        vec!["GET /api/config", "POST /auth/token", "GET /api/devices", &format!("POST /auth/logout cookie={SESSION}")],
        "{log:?}"
    );
    assert!(ui.unexpected.lock().unwrap().is_empty());
    // cookie 和口令都不在磁盘上
    let dir = Paths::new(rig.home.path()).client_dir(&crate::remote::host_of(&rig.hub.url).unwrap());
    for f in std::fs::read_dir(dir).unwrap().flatten() {
        let text = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).into_owned();
        assert!(!text.contains("sess-abc") && !text.contains(PW) && !text.contains(GOOD_TOKEN), "{:?}", f.path());
    }
    // 接入完成之后 daemon 有 enroll.completed 审计，没有口令
    let audit = rig.td.ctx.audit.query(None, None, 100).await;
    assert!(audit.iter().any(|e| e.action == "enroll.completed"));
    assert!(!serde_json::to_string(&audit).unwrap().contains(PW));
}

#[tokio::test]
async fn scan_enrollment_draws_the_qr_and_finishes_when_a_paired_device_approves() {
    let rig = Rig::new().await;
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN));
    let task = {
        let (env, ui2) = (rig.env(), ui.clone());
        let req = rig.req(None, Some(ViaArg::Scan));
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, req).await
        })
    };
    // 二维码出现（节点 id、接入会话 id、base32 的 r），已配对设备据此批准
    let payload = loop {
        if let Some(p) = ui.qr.lock().unwrap().first().cloned() {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let (node, enroll_id, secret) = parse_scan_qr(&payload).expect("a scannable payload");
    assert_eq!(node, rig.hub.node_id);
    assert!(payload.starts_with("anycon-enroll:1:"));
    assert!(ui.said.lock().unwrap().iter().any(|l| l.contains("Scan this")));
    // 请求在等批准
    let t = std::time::Instant::now();
    loop {
        let list: NodeEnrollListResult = rig.rpc(methods::NODE_ENROLL_LIST, json!({})).await.unwrap();
        if list.enrolls.iter().any(|e| e.enroll_id == enroll_id && e.status == EnrollSessionStatus::WaitApproval) {
            break;
        }
        assert!(t.elapsed() < T, "the request never reached the node");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _: NodeEnrollApproveResult = rig
        .rpc(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": enroll_id, "secret": hex::encode(secret) }))
        .await
        .unwrap();
    let joined = tokio::time::timeout(T, task).await.unwrap().unwrap().unwrap();
    assert_eq!(joined.method, EnrollMethod::Scan);
    assert_saved(&rig, &joined);
    // 账号里只有一台节点：不用选
    assert!(ui.asked.lock().unwrap().iter().all(|p| !p.contains("Choose")), "{:?}", ui.asked.lock().unwrap());
}

#[tokio::test]
async fn approve_enrollment_asks_for_the_code_only_after_a_paired_device_approved() {
    let rig = Rig::new().await;
    let (code_tx, code_rx) = std::sync::mpsc::channel::<String>();
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Account token", GOOD_TOKEN)
            .script("Code shown on the approving device", Answer::Wait(Mutex::new(code_rx))),
    );
    let task = {
        let (env, ui2) = (rig.env(), ui.clone());
        let req = rig.req(Some("node0"), Some(ViaArg::Approve));
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, req).await
        })
    };
    // 已配对设备收到请求（hint 是 CLI 自报的名字）、批准、拿到 6 位码，告诉人，人敲进 CLI
    let t = std::time::Instant::now();
    let enroll_id = loop {
        let list: NodeEnrollListResult = rig.rpc(methods::NODE_ENROLL_LIST, json!({})).await.unwrap();
        if let Some(e) = list.enrolls.iter().find(|e| e.status == EnrollSessionStatus::WaitApproval) {
            assert_eq!(
                (e.method, e.approval, e.hint.as_ref().map(|h| h.name.as_str())),
                (Some(EnrollMethod::Code), true, Some("cli-test"))
            );
            break e.enroll_id.clone();
        }
        assert!(t.elapsed() < T, "the request never reached the node");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let r: NodeEnrollApproveResult =
        rig.rpc(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": enroll_id })).await.unwrap();
    code_tx.send(format!("{}-{}", &r.code.as_ref().unwrap()[..3], &r.code.as_ref().unwrap()[3..])).unwrap();
    let joined = tokio::time::timeout(T, task).await.unwrap().unwrap().unwrap();
    assert_eq!(joined.method, EnrollMethod::Code);
    assert_saved(&rig, &joined);
    assert!(ui.said.lock().unwrap().iter().any(|l| l.contains("Waiting for a paired device")));
}

#[tokio::test]
async fn join_code_enrollment_retries_a_wrong_code_up_to_three_times() {
    let rig = Rig::new().await;
    let made: NodeEnrollCodeResult = rig.rpc(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let wrong = if made.code == "000000" { "000001" } else { "000000" };
    // 格式不对的（5 位）不发给 daemon、不占次数；输错一次之后重新问，第二次对
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Account token", GOOD_TOKEN)
            .text("Join code", "12345")
            .text("Join code", wrong)
            .text("Join code", &made.code),
    );
    let joined = run(&rig, ui.clone(), rig.req(Some(&rig.hub.node_id.clone()), Some(ViaArg::Code))).await.unwrap();
    assert_eq!(joined.method, EnrollMethod::Code);
    assert_saved(&rig, &joined);
    assert!(ui.said.lock().unwrap().iter().any(|l| l.contains("6 digits")), "{:?}", ui.said.lock().unwrap());
    assert!(
        ui.said.lock().unwrap().iter().any(|l| l.contains("Wrong code. 2 attempts left")),
        "{:?}",
        ui.said.lock().unwrap()
    );
    assert!(ui.unexpected.lock().unwrap().is_empty());
}

#[tokio::test]
async fn three_wrong_join_codes_end_with_exit_code_five() {
    let rig = Rig::new().await;
    let made: NodeEnrollCodeResult = rig.rpc(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let wrong = if made.code == "000000" { "000001" } else { "000000" };
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Account token", GOOD_TOKEN)
            .text("Join code", wrong)
            .text("Join code", wrong)
            .text("Join code", wrong),
    );
    let e = err(run(&rig, ui.clone(), rig.req(Some("mac-mini"), Some(ViaArg::Code))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "wrong_secret"), "{}", e.message);
    // 失败不落盘，会话收掉了
    assert!(rig.state().nodes.is_empty() && rig.state().token.is_none());
    assert!(rig.td.ctx.peers.list().is_empty());
    assert!(rig.hub.shared.cancels.lock().unwrap().len() <= 1);
}

// ====================================================================== 失败分支与退出码

#[tokio::test]
async fn a_wrong_password_is_exit_five_and_nothing_is_saved() {
    let rig = Rig::new().await;
    rig.set_password().await;
    let ui =
        Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", "not the password"));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "wrong_secret"));
    assert!(rig.state().nodes.is_empty() && rig.state().token.is_none());
    // 登出照样做了
    assert!(rig.hub.http_log().iter().any(|l| l.starts_with("POST /auth/logout")), "{:?}", rig.hub.http_log());
    // daemon 记了一次失败
    let st: NodeEnrollStatusResult = rig.rpc(methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
    assert_eq!(st.password.failures, 1);
    // 再来一次（退避之前）：同样的错，计数 2
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", "still wrong"));
    assert_eq!(err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await).exit, EXIT_DENIED);
}

#[tokio::test]
async fn backoff_locked_and_unset_password_say_what_to_do_next() {
    let rig = Rig::new().await;
    // 没设口令
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "password_not_set"));
    assert!(e.message.contains("anycon passwd"), "{}", e.message);
    // 退避中：写明还要等多久（直接把库里的状态设成第 4 次失败）
    rig.set_password().await;
    rig.td.ctx.store.enroll_password_record(4, anycon_daemon::util::now_ms() + 120_000, false).await.unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "backoff"));
    assert!(e.message.contains(" s"), "{}", e.message);
    // 锁定：写明怎么解
    rig.td.ctx.store.enroll_password_record(10, 0, true).await.unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "locked"));
    assert!(e.message.contains("anycon passwd --enable"), "{}", e.message);
    // 方式关了
    rig.td.ctx.store.enroll_password_record(0, 0, false).await.unwrap();
    let mut c = rig.td.client("admin");
    c.call::<Value>(&rig.td, methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": false, "password": false }))
        .await
        .unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "method_disabled"));
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Scan))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "method_disabled"));
    // 接入码：没有有效的码
    c.call::<Value>(&rig.td, methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": true })).await.unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Join code", "123456"));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Code))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "no_code"));
}

#[tokio::test]
async fn login_failures_and_node_selection_failures_log_out_and_use_the_documented_codes() {
    let rig = Rig::new().await;
    *rig.hub.shared.extra_nodes.lock().unwrap() = true;
    // token 不对：5，没有 cookie 也就没有登出
    let ui = Arc::new(ScriptedUi::default().text("Account token", "wrong-token"));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!(e.exit, EXIT_DENIED, "{}", e.message);
    assert!(!rig.hub.http_log().iter().any(|l| l.starts_with("POST /auth/logout")));
    // 节点找不到 / 有歧义：2，已经登录了所以要登出
    for key in ["no-such-node", "build-box"] {
        let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN));
        let e = err(run(&rig, ui, rig.req(Some(key), Some(ViaArg::Password))).await);
        assert_eq!(e.exit, EXIT_USAGE, "{key}: {}", e.message);
    }
    let logouts = rig.hub.http_log().iter().filter(|l| l.starts_with("POST /auth/logout")).count();
    assert_eq!(logouts, 2, "{:?}", rig.hub.http_log());
    // hub 只开了 OAuth：2，提示改用短码
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["github".into()];
    let ui = Arc::new(ScriptedUi::default());
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
    assert_eq!(e.exit, EXIT_USAGE);
    assert!(e.message.contains("--code"), "{}", e.message);
    // 完整 id 但不是账号里的节点（hub 说 not_found）：2
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["token".into()];
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some(&"e".repeat(32)), Some(ViaArg::Password))).await);
    assert_eq!(e.exit, EXIT_USAGE, "{}", e.message);
}

fn email_req(email: Option<&str>, node: &str) -> EnrollRequest {
    EnrollRequest { node: Some(node.into()), via: Some(ViaArg::Password), email: email.map(str::to_string) }
}

fn email_log(rig: &Rig) -> Vec<String> {
    rig.hub.http_log().into_iter().filter(|l| l.contains("/auth/")).collect()
}

#[tokio::test]
async fn email_sign_in_sends_the_address_as_typed_asks_for_the_code_and_does_not_claim_the_address_has_an_account() {
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["token".into(), "email".into()];
    // `--email` 给了就不再问用哪种方式；码按 `123 456` / `123-456` 的写法也行（hub 也容忍，CLI 先清成 6 位数字）
    let ui = Arc::new(ScriptedUi::default().text("Code from the email", "246 810").text("Node password", PW));
    let joined = run(&rig, ui.clone(), email_req(Some("  Me@Example.Test "), "mac-mini")).await.unwrap();
    assert_saved(&rig, &joined);
    assert_eq!(*rig.hub.shared.email_sends.lock().unwrap(), vec!["Me@Example.Test"], "trimmed, case left to the hub");
    assert_eq!(
        *rig.hub.shared.email_verifies.lock().unwrap(),
        vec![("Me@Example.Test".to_string(), GOOD_EMAIL_CODE.to_string())]
    );
    assert_eq!(
        email_log(&rig),
        vec![
            "POST /auth/email/send".to_string(),
            "POST /auth/email/verify".to_string(),
            format!("POST /auth/logout cookie={SESSION}")
        ],
        "no token login, and the session is signed out at the end"
    );
    assert_eq!(rig.hub.shared.ws_cookies.lock().unwrap().clone(), vec![Some(SESSION.to_string())]);
    assert!(!ui.asked.lock().unwrap().iter().any(|p| p.contains("[email/token]") || p.contains("Account token")));
    // hub 对绑定和没绑定的邮箱回答一样：提示不能说「已发送到你的邮箱」
    let said = ui.said.lock().unwrap().join("\n");
    assert!(said.contains("If that email is bound to an account"), "{said}");
    assert!(!said.contains("was sent to your email"), "{said}");
    // 登录 cookie 和验证码不落盘
    let dir = Paths::new(rig.home.path()).client_dir(&crate::remote::host_of(&rig.hub.url).unwrap());
    for f in std::fs::read_dir(dir).unwrap().flatten() {
        let text = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).into_owned();
        assert!(!text.contains("sess-abc") && !text.contains(GOOD_EMAIL_CODE), "{:?}", f.path());
    }
}

#[tokio::test]
async fn without_email_the_cli_asks_which_sign_in_method_to_use_when_the_hub_offers_both() {
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["token".into(), "email".into()];
    // 选 email（默认），再问邮箱
    let ui = Arc::new(
        ScriptedUi::default()
            .text("[email/token]", "")
            .text("Email address", "me@example.test")
            .text("Code from the email", GOOD_EMAIL_CODE)
            .text("Node password", PW),
    );
    run(&rig, ui, email_req(None, "mac-mini")).await.unwrap();
    assert_eq!(*rig.hub.shared.email_sends.lock().unwrap(), vec!["me@example.test"]);
    // 选 token：走 /auth/token，不碰邮箱
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["token".into(), "email".into()];
    let ui = Arc::new(
        ScriptedUi::default()
            .text("[email/token]", "token")
            .text("Account token", GOOD_TOKEN)
            .text("Node password", PW),
    );
    run(&rig, ui, email_req(None, "mac-mini")).await.unwrap();
    assert!(rig.hub.shared.email_sends.lock().unwrap().is_empty());
    // hub 只开了邮箱：不问，直接问邮箱
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["email".into()];
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Email address", "only@example.test")
            .text("Code from the email", GOOD_EMAIL_CODE)
            .text("Node password", PW),
    );
    run(&rig, ui.clone(), email_req(None, "mac-mini")).await.unwrap();
    assert!(!ui.asked.lock().unwrap().iter().any(|p| p.contains("[email/token]")));
}

#[tokio::test]
async fn github_and_google_in_login_methods_are_ignored_and_never_contacted() {
    // multi hub with SMTP and both OAuth providers: only the email code is usable, so the CLI uses it without asking
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["github".into(), "google".into(), "email".into()];
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Email address", "multi@example.test")
            .text("Code from the email", GOOD_EMAIL_CODE)
            .text("Node password", PW),
    );
    run(&rig, ui.clone(), email_req(None, "mac-mini")).await.unwrap();
    assert!(!ui.asked.lock().unwrap().iter().any(|p| p.contains("[email/token]")), "{:?}", ui.asked.lock().unwrap());
    assert_eq!(*rig.hub.shared.email_sends.lock().unwrap(), vec!["multi@example.test"]);
    assert!(
        !rig.hub.http_log().iter().any(|l| l.contains("/oauth/") || l.contains("/callback/")),
        "{:?}",
        rig.hub.http_log()
    );

    // single hub that offers everything: the usual email-or-token choice, OAuth is not part of it
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() =
        vec!["github".into(), "google".into(), "email".into(), "token".into()];
    let ui = Arc::new(
        ScriptedUi::default()
            .text("[email/token]", "token")
            .text("Account token", GOOD_TOKEN)
            .text("Node password", PW),
    );
    run(&rig, ui.clone(), email_req(None, "mac-mini")).await.unwrap();
    assert!(rig.hub.shared.email_sends.lock().unwrap().is_empty());
    assert!(ui.asked.lock().unwrap().iter().any(|p| p.contains("[email/token]")));

    // a hub with only OAuth (several providers): exit 2, suggest a short code, nothing was sent anywhere
    let rig = Rig::new().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["github".into(), "google".into()];
    let ui = Arc::new(ScriptedUi::default());
    let e = err(run(&rig, ui.clone(), email_req(None, "mac-mini")).await);
    assert_eq!(e.exit, EXIT_USAGE, "{}", e.message);
    assert!(e.message.contains("--code"), "{}", e.message);
    assert!(ui.asked.lock().unwrap().is_empty());
    assert!(email_log(&rig).is_empty(), "no sign-in request of any kind: {:?}", email_log(&rig));
    assert!(rig.hub.shared.email_sends.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_wrong_email_code_is_asked_again_and_the_third_wrong_one_is_exit_five_without_a_session() {
    let rig = Rig::new().await;
    rig.set_password().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["email".into()];
    // 格式不对的（不是 6 位数字）本地就挡掉，不发给 hub，也不算一次尝试；错一次之后再输对的就能继续
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Code from the email", "12ab")
            .text("Code from the email", "111111")
            .text("Code from the email", GOOD_EMAIL_CODE)
            .text("Node password", PW),
    );
    run(&rig, ui.clone(), email_req(Some("me@example.test"), "mac-mini")).await.unwrap();
    let verifies = rig.hub.shared.email_verifies.lock().unwrap().clone();
    assert_eq!(
        verifies.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
        vec!["111111", GOOD_EMAIL_CODE],
        "the malformed code never reached the hub"
    );
    let said = ui.said.lock().unwrap().join("\n");
    assert!(said.contains("6 digits") && said.contains("wrong or has expired"), "{said}");
    // 只发了一次码（重问码不会再发信）
    assert_eq!(rig.hub.shared.email_sends.lock().unwrap().len(), 1);

    // 连错三次：退出码 5、`invalid_code`，说明要重新来；没有登录成功所以没有登出，也没有连 WebSocket
    let rig = Rig::new().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["email".into()];
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Code from the email", "111111")
            .text("Code from the email", "222222")
            .text("Code from the email", "333333"),
    );
    let e = err(run(&rig, ui, email_req(Some("me@example.test"), "mac-mini")).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "invalid_code"), "{}", e.message);
    assert!(e.message.contains("run the command again"), "{}", e.message);
    assert_eq!(rig.hub.shared.email_verifies.lock().unwrap().len(), 3);
    assert!(!rig.hub.http_log().iter().any(|l| l.starts_with("POST /auth/logout")));
    assert!(rig.hub.shared.ws_cookies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn email_sign_in_rate_limits_and_bad_addresses_are_reported_before_anything_else_happens() {
    let rig = Rig::new().await;
    *rig.hub.shared.login_methods.lock().unwrap() = vec!["email".into()];
    // 地址里没有 @：参数错误 2，什么都没发
    let ui = Arc::new(ScriptedUi::default());
    let e = err(run(&rig, ui, email_req(Some("not-an-address"), "mac-mini")).await);
    assert_eq!(e.exit, EXIT_USAGE, "{}", e.message);
    assert!(rig.hub.shared.email_sends.lock().unwrap().is_empty());
    // hub 限速（429 + Retry-After）：退出码 1、`rate_limited`，写明要等多少秒；不问码
    *rig.hub.shared.email_send_status.lock().unwrap() = Some(429);
    let ui = Arc::new(ScriptedUi::default());
    let e = err(run(&rig, ui.clone(), email_req(Some("me@example.test"), "mac-mini")).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_ERROR, "rate_limited"), "{}", e.message);
    assert!(e.message.contains("37 seconds") && e.message.contains("too many requests"), "{}", e.message);
    assert!(ui.asked.lock().unwrap().is_empty(), "no code prompt after a failed send");
}

#[test]
fn hub_error_bodies_are_read_in_the_real_nested_shape_and_the_older_flat_one() {
    let resp = |status: u16, headers: &[(&str, &str)], body: &str| HttpResponse {
        status,
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body: body.as_bytes().to_vec(),
    };
    // 真 hub：`{ "error": { code, message, retry_after? } }`
    let r = resp(401, &[], r#"{"error":{"code":"invalid_code","message":"invalid or expired code"}}"#);
    assert_eq!(error_fields(&r), ("invalid_code".into(), "invalid or expired code".into(), None));
    let e = api_error("signing in", &r);
    assert_eq!((e.exit, e.message.as_str()), (EXIT_DENIED, "signing in failed: invalid or expired code"));
    // 429：正文里的 retry_after，或者只有 Retry-After 头
    let r = resp(429, &[("retry-after", "12")], r#"{"error":{"code":"rate_limited","message":"too many requests"}}"#);
    assert_eq!(error_fields(&r).2, Some(12));
    assert!(api_error("sending the sign-in code", &r).message.contains("wait 12 seconds"));
    let r = resp(429, &[], r#"{"error":{"code":"rate_limited","message":"x","retry_after":5}}"#);
    assert_eq!(error_fields(&r).2, Some(5));
    assert!(api_error("x", &resp(429, &[], "")).message.contains("wait a minute"));
    // 旧形状和没有正文
    let r = resp(401, &[], r#"{"error":"invalid_token","message":"invalid token"}"#);
    assert_eq!(error_fields(&r), ("invalid_token".into(), "invalid token".into(), None));
    assert_eq!(api_error("signing in", &resp(500, &[], "<html>")).message, "signing in failed (HTTP 500)");
}

#[tokio::test]
async fn a_node_chosen_from_several_and_a_method_chosen_from_the_menu() {
    let rig = Rig::new().await;
    *rig.hub.shared.extra_nodes.lock().unwrap() = true;
    rig.set_password().await;
    // 没给 --node、没给 --via：先选节点（账号里有 3 台 daemon），再选方式
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Account token", GOOD_TOKEN)
            .text("Choose [1-3]", "1")
            .text("Choose [1-4]", "password")
            .text("Node password", PW),
    );
    // 账号里 3 台 daemon，顺序：mac-mini、build-box、build-box
    let joined = run(&rig, ui.clone(), rig.req(None, None)).await.unwrap();
    assert_eq!(joined.node.id, rig.hub.node_id);
    let said = ui.said.lock().unwrap().join("\n");
    assert!(said.contains("1) mac-mini") && said.contains("[offline]") && said.contains("4) password"), "{said}");
}

#[tokio::test]
async fn ctrl_c_while_waiting_cancels_the_request_and_logs_out() {
    let rig = Rig::new().await;
    // 批准之后敲码的提示被 Ctrl+C 打断（终端原始模式下 Ctrl+C 是字节 3，read_secret 返回「被中断」）
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).script("Code shown", Answer::Interrupt));
    let task = {
        let (env, ui2) = (rig.env(), ui.clone());
        let req = rig.req(Some("mac-mini"), Some(ViaArg::Approve));
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, req).await
        })
    };
    let t = std::time::Instant::now();
    let enroll_id = loop {
        let list: NodeEnrollListResult = rig.rpc(methods::NODE_ENROLL_LIST, json!({})).await.unwrap();
        if let Some(e) = list.enrolls.iter().find(|e| e.status == EnrollSessionStatus::WaitApproval) {
            break e.enroll_id.clone();
        }
        assert!(t.elapsed() < T);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let _: NodeEnrollApproveResult =
        rig.rpc(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": enroll_id })).await.unwrap();
    let e = tokio::time::timeout(T, task).await.unwrap().unwrap().err().expect("interrupted");
    assert_eq!(e.exit, EXIT_INTERRUPTED);
    // enroll_cancel 发给了 hub，daemon 的会话收掉了；登出了；什么都没落盘
    let t = std::time::Instant::now();
    while rig.td.ctx.enroll.live_count() > 0 {
        assert!(t.elapsed() < T, "the daemon still holds the session");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.hub.shared.cancels.lock().unwrap().clone(), vec![enroll_id]);
    assert!(rig.hub.http_log().iter().any(|l| l.starts_with("POST /auth/logout")));
    assert!(rig.state().nodes.is_empty());
}

#[tokio::test]
async fn a_cli_that_is_already_a_device_on_the_hub_skips_the_sign_in() {
    let rig = Rig::new().await;
    rig.set_password().await;
    // 第一次：登录 + 接入
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    let before = rig.hub.http_log().len();
    // 第二次：已经是这个 hub 上的设备，节点在本地记录里：不登录、不列设备，WebSocket 带 token、不带 cookie
    let ui = Arc::new(ScriptedUi::default().text("Node password", PW));
    let joined = run(&rig, ui.clone(), rig.req(Some("test-node"), Some(ViaArg::Password))).await.unwrap();
    assert_eq!(joined.node.id, rig.hub.node_id);
    assert_eq!(rig.hub.http_log().len(), before, "no HTTP request at all: {:?}", rig.hub.http_log());
    assert_eq!(rig.hub.shared.ws_cookies.lock().unwrap().last().unwrap(), &None);
    assert!(ui.asked.lock().unwrap().iter().all(|p| !p.contains("token")));
    // 没有 --node 又要从账号的列表里选：已经是设备，用设备 token 列（只读），不登录、不问账号 token、不带 cookie
    *rig.hub.shared.extra_nodes.lock().unwrap() = true;
    let ui = Arc::new(ScriptedUi::default().text("Choose [1-3]", "1").text("Node password", PW));
    run(&rig, ui.clone(), rig.req(None, Some(ViaArg::Password))).await.unwrap();
    let log = rig.hub.http_log();
    assert_eq!(&log[before..], ["GET /api/devices bearer"], "{log:?}");
    assert!(ui.asked.lock().unwrap().iter().all(|p| !p.contains("Account token")), "{:?}", ui.asked.lock().unwrap());
    assert_eq!(rig.hub.shared.ws_cookies.lock().unwrap().last().unwrap(), &None);
    // 按 hub 上的名字找（本地记录叫 test-node，所以要走列表）也不登录
    let before = rig.hub.http_log().len();
    let ui = Arc::new(ScriptedUi::default().text("Node password", PW));
    run(&rig, ui.clone(), rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    assert_eq!(&rig.hub.http_log()[before..], ["GET /api/devices bearer"]);
    // 同一个节点再次接入没有 key_mismatch（公钥没变），本地只有一条记录
    assert_eq!(rig.state().nodes.len(), 1);
}

#[tokio::test]
async fn when_the_hub_refuses_the_device_token_for_the_list_the_cli_signs_in_instead() {
    let rig = Rig::new().await;
    rig.set_password().await;
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    let before = rig.hub.http_log().len();
    // hub 不认设备 token 列设备（401：已吊销，或者 hub 还不支持）：回退到账号登录，登录后用 cookie 列
    *rig.hub.shared.reject_bearer.lock().unwrap() = true;
    *rig.hub.shared.extra_nodes.lock().unwrap() = true;
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    // 名字是 hub 上的名字，本地记录叫 test-node：走列表
    let joined = run(&rig, ui.clone(), rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    assert_eq!(joined.node.id, rig.hub.node_id);
    let log = rig.hub.http_log();
    assert_eq!(
        &log[before..],
        [
            "GET /api/devices bearer",
            "GET /api/config",
            "POST /auth/token",
            "GET /api/devices",
            &format!("POST /auth/logout cookie={SESSION}")
        ],
        "{log:?}"
    );
    assert!(ui.unexpected.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_saved_device_the_hub_no_longer_knows_is_dropped_and_the_cli_joins_as_a_new_device() {
    let rig = Rig::new().await;
    rig.set_password().await;
    // 本地存着一个 hub 不认的 token
    let paths = Paths::new(rig.home.path());
    let dir = paths.client_dir(&crate::remote::host_of(&rig.hub.url).unwrap());
    let mut st = HubState::load(&dir, &rig.hub.url);
    st.secret = Some([7u8; 32]);
    st.token = Some("stale-token".into());
    st.device_id = Some("stale-id".into());
    st.save().unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let joined = run(&rig, ui.clone(), rig.req(Some("mac-mini"), Some(ViaArg::Password))).await.unwrap();
    assert_eq!(joined.node.id, rig.hub.node_id);
    let st = rig.state();
    assert_eq!(st.token.as_deref(), Some(DEVICE_TOKEN), "the stale token was replaced");
    assert_eq!(st.secret, Some([7u8; 32]), "the key stays: it is this machine's identity");
    assert!(ui.said.lock().unwrap().iter().any(|l| l.contains("no longer accepts")));
}

#[tokio::test]
async fn an_existing_node_with_a_different_key_is_not_overwritten() {
    let rig = Rig::new().await;
    rig.set_password().await;
    // 本地已有同 id 但公钥不同的节点
    let paths = Paths::new(rig.home.path());
    let dir = paths.client_dir(&crate::remote::host_of(&rig.hub.url).unwrap());
    let mut st = HubState::load(&dir, &rig.hub.url);
    st.nodes.push(crate::remote::NodeEntry {
        id: rig.hub.node_id.clone(),
        name: "old".into(),
        pubkey: "00".repeat(32),
        paired_at_ms: 1,
    });
    st.save().unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
    let e = err(run(&rig, ui, rig.req(Some("old"), Some(ViaArg::Password))).await);
    assert_eq!((e.exit, e.code.as_str()), (4, "key_mismatch"), "{}", e.message);
    let st = rig.state();
    assert_eq!(st.nodes[0].pubkey, "00".repeat(32), "not replaced silently");
    assert!(st.token.is_none(), "nothing from the failed attempt is saved");
}

#[tokio::test]
async fn a_denied_request_is_exit_five_and_a_request_nobody_answers_times_out() {
    let rig = Rig::new().await;
    // 拒绝：已配对设备点了「拒绝」
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN));
    let task = {
        let (env, ui2) = (rig.env(), ui.clone());
        let req = rig.req(Some("mac-mini"), Some(ViaArg::Approve));
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, req).await
        })
    };
    let t = std::time::Instant::now();
    let enroll_id = loop {
        let list: NodeEnrollListResult = rig.rpc(methods::NODE_ENROLL_LIST, json!({})).await.unwrap();
        if let Some(e) = list.enrolls.iter().find(|e| e.status == EnrollSessionStatus::WaitApproval) {
            break e.enroll_id.clone();
        }
        assert!(t.elapsed() < T);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let _: Value = rig.rpc(methods::NODE_ENROLL_DENY, json!({ "enroll_id": enroll_id })).await.unwrap();
    let e = tokio::time::timeout(T, task).await.unwrap().unwrap().err().expect("denied");
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "denied"), "{}", e.message);
    assert!(rig.state().nodes.is_empty());
    // 没人理：hub 说 5 分钟，到点 CLI 自己放弃（这里把有效期缩成 1.5 秒），并告诉 hub 取消
    rig.hub.shared.cancels.lock().unwrap().clear();
    *rig.hub.shared.started_ttl_ms.lock().unwrap() = Some(1_500);
    let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN));
    let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Scan))).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_ERROR, "timeout"), "{}", e.message);
    assert_eq!(rig.hub.shared.cancels.lock().unwrap().len(), 1, "the unanswered request was cancelled");
    let t = std::time::Instant::now();
    while rig.td.ctx.enroll.live_count() > 0 {
        assert!(t.elapsed() < T, "the daemon still holds the request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_hub_refusing_to_start_the_request_maps_to_offline_or_a_general_error() {
    let rig = Rig::new().await;
    for (reason, exit, word) in [
        (EnrollFailReason::Offline, EXIT_OFFLINE, "offline"),
        (EnrollFailReason::RateLimited, EXIT_ERROR, "rate_limited"),
        (EnrollFailReason::Busy, EXIT_ERROR, "busy"),
        (EnrollFailReason::NotFound, EXIT_USAGE, "invalid"),
    ] {
        *rig.hub.shared.begin_failure.lock().unwrap() = Some(reason);
        let ui = Arc::new(ScriptedUi::default().text("Account token", GOOD_TOKEN).text("Node password", PW));
        let e = err(run(&rig, ui, rig.req(Some("mac-mini"), Some(ViaArg::Password))).await);
        assert_eq!((e.exit, e.code.as_str()), (exit, word), "{reason:?}: {}", e.message);
    }
    // 每次都登出了
    let logouts = rig.hub.http_log().iter().filter(|l| l.starts_with("POST /auth/logout")).count();
    assert_eq!(logouts, 4, "{:?}", rig.hub.http_log());
}

// ====================================================================== 真 hub（进程内）

struct RealHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
}

impl RealHub {
    async fn start() -> RealHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = anycon_hub::Config::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        let handle = anycon_hub::run(c).await.expect("hub starts");
        RealHub { handle, _dir: dir }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }

    fn admin_token(&self) -> String {
        self.handle.generated_admin_token.clone().unwrap()
    }

    async fn admin_cookie(&self) -> String {
        let res = reqwest::Client::new()
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": self.admin_token() }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }
}

/// 一台已经连着真 hub、完成了首次配对（有设备身份和账号）的 daemon，和那台配对用的假 web。
async fn daemon_on_real_hub(hub: &RealHub) -> (TestDaemon, anycon_daemon::testutil::FakeWeb) {
    let td = TestDaemon::start_full(&[], false, Some(&hub.url())).await;
    assert!(anycon_daemon::testutil::eventually(T, || async { td.ctx.hub_state.get().connected }).await);
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = anycon_daemon::testutil::FakeWeb::connect(
        &hub.url(),
        None,
        Some(&cookie),
        "Setup Browser",
        anycon_crypto::Keypair::generate(),
    )
    .await;
    web.pair(&begin.code).await;
    assert!(anycon_daemon::testutil::eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    (td, web)
}

fn real_env(home: &std::path::Path, hub: &RealHub) -> EnrollEnv {
    EnrollEnv {
        paths: Paths::new(home),
        hub_url: hub.url(),
        device_name: "cli-real".into(),
        ack_timeout: Duration::from_secs(10),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn password_enrollment_against_the_real_hub_ends_with_a_working_connection() {
    let hub = RealHub::start().await;
    let (td, web) = daemon_on_real_hub(&hub).await;
    let node_id = web.node_id.clone().unwrap();
    let mut local = td.client("owner");
    local.call::<Value>(&td, methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": PW })).await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let ui: Arc<dyn Ui> =
        Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()).text("Node password", PW));
    let joined = tokio::time::timeout(
        T,
        enroll(
            &real_env(home.path(), &hub),
            &ui,
            EnrollRequest { node: Some("test-node".into()), via: Some(ViaArg::Password), email: None },
        ),
    )
    .await
    .expect("in time")
    .unwrap();
    assert_eq!(joined.node.id, node_id);
    // 落盘之后就是普通的已配对设备：IK 握手，加密 RPC
    let paths = Paths::new(home.path());
    let dir = paths.client_dir(&crate::remote::host_of(&hub.url()).unwrap());
    let st = HubState::load(&dir, &hub.url());
    assert!(st.token.is_some() && st.device_id.is_some());
    let conn = crate::remote::open(&st, &st.nodes[0], "cli-real").await.unwrap();
    let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.node_id, node_id);
    drop(web);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_and_approve_enrollments_against_the_real_hub_are_approved_from_a_paired_device() {
    let hub = RealHub::start().await;
    let (td, mut web) = daemon_on_real_hub(&hub).await;
    let node_id = web.node_id.clone().unwrap();
    web.ik().await.expect("the paired browser reconnects to the node");
    let home = tempfile::tempdir().unwrap();

    // ---- 请求批准：已配对的浏览器收到 node.enroll_requested，批准，拿到码，告诉人，人敲进 CLI
    let (code_tx, code_rx) = std::sync::mpsc::channel::<String>();
    let ui = Arc::new(
        ScriptedUi::default()
            .text("Account token", &hub.admin_token())
            .script("Code shown on the approving device", Answer::Wait(Mutex::new(code_rx))),
    );
    let task = {
        let (env, ui2, prefix) = (real_env(home.path(), &hub), ui.clone(), node_id[..8].to_string());
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, EnrollRequest { node: Some(prefix), via: Some(ViaArg::Approve), email: None }).await
        })
    };
    let note = web.next_note(|n| n.method == methods::NODE_ENROLL_REQUESTED).await;
    let info: EnrollInfo = serde_json::from_value(note.params).unwrap();
    assert_eq!(
        (info.method, info.approval, info.status),
        (Some(EnrollMethod::Code), true, EnrollSessionStatus::WaitApproval)
    );
    assert_eq!(info.hint.as_ref().map(|h| (h.name.as_str(), h.kind)), Some(("cli-real", DeviceKind::Cli)));
    let r: NodeEnrollApproveResult =
        web.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": info.enroll_id })).await.unwrap();
    code_tx.send(r.code.unwrap()).unwrap();
    let joined = tokio::time::timeout(T, task).await.unwrap().unwrap().unwrap();
    assert_eq!(joined.method, EnrollMethod::Code);
    // 批准的那台自己也收到 resolved（approved），最后是 completed 和 device_enrolled
    let resolved =
        web.next_note(|n| n.method == methods::NODE_ENROLL_RESOLVED && n.params["result"] == "completed").await;
    assert_eq!(resolved.params["enroll_id"], json!(info.enroll_id));
    let ev = web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT && n.params["kind"] == "device_enrolled").await;
    assert_eq!(ev.params["device_name"], json!("cli-real"));

    // ---- 扫码：另一台 CLI（另一个 home）画二维码，已配对的浏览器带着二维码里的密钥批准
    let home2 = tempfile::tempdir().unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()));
    let task = {
        let (env, ui2) = (real_env(home2.path(), &hub), ui.clone());
        let node = node_id.clone();
        tokio::spawn(async move {
            let ui_dyn: Arc<dyn Ui> = ui2;
            enroll(&env, &ui_dyn, EnrollRequest { node: Some(node), via: Some(ViaArg::Scan), email: None }).await
        })
    };
    let payload = loop {
        if let Some(p) = ui.qr.lock().unwrap().first().cloned() {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let (qr_node, enroll_id, secret) = parse_scan_qr(&payload).expect("a scannable payload");
    assert_eq!(qr_node, node_id);
    // 请求要先到 daemon（hello 之后才有 wait_approval）
    let t = std::time::Instant::now();
    loop {
        let list: NodeEnrollListResult = web.call(methods::NODE_ENROLL_LIST, json!({})).await.unwrap();
        if list.enrolls.iter().any(|e| e.enroll_id == enroll_id && e.status == EnrollSessionStatus::WaitApproval) {
            break;
        }
        assert!(t.elapsed() < T, "the scan request never reached the node");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _: NodeEnrollApproveResult = web
        .call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": enroll_id, "secret": hex::encode(secret) }))
        .await
        .unwrap();
    let joined = tokio::time::timeout(T, task).await.unwrap().unwrap().unwrap();
    assert_eq!(joined.method, EnrollMethod::Scan);
    let devices: NodeDevicesResult = web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.iter().filter(|d| d.name == "cli-real").count(), 2, "both CLIs joined");
    drop(td);
}

// ====================================================================== 真 hub：接入码、错口令 / 退避 / 锁定

/// 经真 hub 跑一次 `enroll`（账号 token = hub 的管理员 token），脚本里按需要给出节点口令 / 接入码。
async fn real_enroll(
    hub: &RealHub,
    home: &std::path::Path,
    ui: &Arc<ScriptedUi>,
    node: Option<&str>,
    via: ViaArg,
) -> Result<Joined> {
    let ui_dyn: Arc<dyn Ui> = ui.clone();
    tokio::time::timeout(
        T,
        enroll(
            &real_env(home, hub),
            &ui_dyn,
            EnrollRequest { node: node.map(str::to_string), via: Some(via), email: None },
        ),
    )
    .await
    .expect("in time")
}

/// 接入后的 CLI 就是一台普通的已配对设备：IK 握手之后 `node.info`、`session.list` 都能用。
async fn assert_device_works(hub: &RealHub, home: &std::path::Path, node_id: &str) {
    let dir = Paths::new(home).client_dir(&crate::remote::host_of(&hub.url()).unwrap());
    let st = HubState::load(&dir, &hub.url());
    assert!(st.token.is_some() && st.device_id.is_some());
    let conn = crate::remote::open(&st, &st.nodes[0], "cli-real").await.unwrap();
    let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert_eq!(info.node_id, node_id);
    let sessions: SessionListResult = conn.call(methods::SESSION_LIST, json!({})).await.unwrap();
    assert!(sessions.sessions.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_code_from_the_node_enrolls_a_cli_against_the_real_hub_once_and_the_device_token_lists_the_account() {
    let hub = RealHub::start().await;
    let (td, web) = daemon_on_real_hub(&hub).await;
    let node_id = web.node_id.clone().unwrap();
    let mut owner = td.client("owner");

    // `anycon pair --enroll` 做的事：node.enroll_code；人把 6 位码敲进新设备的 CLI
    let made: NodeEnrollCodeResult = owner.call(&td, methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    assert_eq!(made.code.len(), 6);
    let home = tempfile::tempdir().unwrap();
    let ui = Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()).text("Join code", &made.code));
    let joined = real_enroll(&hub, home.path(), &ui, Some("test-node"), ViaArg::Code).await.unwrap();
    assert_eq!((joined.method, joined.node.id.as_str()), (EnrollMethod::Code, node_id.as_str()));
    assert!(ui.unexpected.lock().unwrap().is_empty());
    assert_device_works(&hub, home.path(), &node_id).await;

    // 接入码一次性：同一个码再用（另一台 CLI）得到 no_code（退出码 5），什么都不落盘
    let home2 = tempfile::tempdir().unwrap();
    let ui2 = Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()).text("Join code", &made.code));
    let e = err(real_enroll(&hub, home2.path(), &ui2, Some("test-node"), ViaArg::Code).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "no_code"), "{}", e.message);
    let dir2 = Paths::new(home2.path()).client_dir(&crate::remote::host_of(&hub.url()).unwrap());
    assert!(HubState::load(&dir2, &hub.url()).nodes.is_empty());

    // 已经是这个 hub 上的设备：不给 --node、用新的码再接入一次，列账号里的节点用的是设备 token，
    // 不再问账号 token（脚本里没有 "Account token" 的答案，问了就是 unexpected）
    let made: NodeEnrollCodeResult = owner.call(&td, methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let ui3 = Arc::new(ScriptedUi::default().text("Join code", &made.code));
    let joined = real_enroll(&hub, home.path(), &ui3, None, ViaArg::Code).await.unwrap();
    assert_eq!(joined.node.id, node_id);
    assert!(ui3.unexpected.lock().unwrap().is_empty(), "{:?}", ui3.unexpected.lock().unwrap());
    assert!(ui3.asked.lock().unwrap().iter().all(|p| !p.contains("Account token")));

    // 吊销之后设备 token 不再能列（hub 401），回退到账号登录
    let cookie = hub.admin_cookie().await;
    let dir = Paths::new(home.path()).client_dir(&crate::remote::host_of(&hub.url()).unwrap());
    let device_id = HubState::load(&dir, &hub.url()).device_id.unwrap();
    let res = reqwest::Client::new()
        .delete(format!("{}/api/devices/{device_id}", hub.url()))
        .header("x-anycon", "1")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let made: NodeEnrollCodeResult = owner.call(&td, methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let ui4 = Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()).text("Join code", &made.code));
    let joined = real_enroll(&hub, home.path(), &ui4, None, ViaArg::Code).await.unwrap();
    assert_eq!(joined.node.id, node_id);
    assert!(ui4.said.lock().unwrap().iter().any(|l| l.contains("no longer accepts")), "{:?}", ui4.said.lock().unwrap());
    assert_device_works(&hub, home.path(), &node_id).await;

    // hub 的审计里每次接入一条 device_enrolled
    let audit: Value = reqwest::Client::new()
        .get(format!("{}/api/admin/audit?limit=100", hub.url()))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enrolled = audit.as_array().unwrap().iter().filter(|e| e["event"] == "device_enrolled").count();
    assert_eq!(enrolled, 3, "{audit}");
    drop(web);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_passwords_back_off_then_lock_and_re_enabling_the_password_lets_the_cli_in_through_the_real_hub() {
    let hub = RealHub::start().await;
    let (td, mut web) = daemon_on_real_hub(&hub).await;
    let node_id = web.node_id.clone().unwrap();
    web.ik().await.expect("the paired browser reconnects to the node");
    let mut owner = td.client("owner");
    owner.call::<Value>(&td, methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": PW })).await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let attempt = |pw: &'static str| {
        let ui = Arc::new(ScriptedUi::default().text("Account token", &hub.admin_token()).text("Node password", pw));
        let (hub, home) = (&hub, home.path().to_path_buf());
        async move { real_enroll(hub, &home, &ui, Some("test-node"), ViaArg::Password).await }
    };
    let mut status_client = td.client("status");

    // 前三次错：每次 wrong_secret（退出码 5），计数 1、2、3；第三次起进入退避
    for n in 1..=3u32 {
        let e = err(attempt("not the password").await);
        assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "wrong_secret"), "attempt {n}: {}", e.message);
        let t = std::time::Instant::now();
        loop {
            let st: NodeEnrollStatusResult =
                status_client.call(&td, methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
            if st.password.failures == n {
                break;
            }
            assert!(t.elapsed() < T, "the failure {n} was never counted");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let ev =
        web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT && n.params["kind"] == "password_failures").await;
    assert_eq!(ev.params["failures"], json!(3));
    // 退避中：连正确的口令也被挡住，写明还要等多久，失败数不变
    let e = err(attempt(PW).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "backoff"), "{}", e.message);
    assert!(e.message.contains(" s"), "{}", e.message);
    let st: NodeEnrollStatusResult = status_client.call(&td, methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
    assert_eq!((st.password.failures, st.password.state), (3, EnrollPasswordState::Enabled));
    assert!(st.password.retry_after_ms.is_some());
    assert!(home_has_no_node(&hub, home.path()));

    // 时间快进到第 10 次：把库里的状态改成「已失败 9 次、退避结束」，再错一次 -> 锁定
    td.ctx.store.enroll_password_record(9, 0, false).await.unwrap();
    let e = err(attempt("still not the password").await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "wrong_secret"), "{}", e.message);
    let ev = web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT && n.params["kind"] == "password_locked").await;
    assert_eq!(ev.params["failures"], json!(10));
    let e = err(attempt(PW).await);
    assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "locked"), "{}", e.message);
    assert!(e.message.contains("anycon passwd --enable"), "{}", e.message);
    let st: NodeEnrollStatusResult = status_client.call(&td, methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
    assert_eq!(st.password.state, EnrollPasswordState::Locked);
    assert!(home_has_no_node(&hub, home.path()));

    // 已配对的设备（这里是浏览器）重新启用：失败计数清零，之后正确的口令就能接入，接入后的设备能用
    let _: Value = web.call(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": true })).await.unwrap();
    let st: NodeEnrollStatusResult = status_client.call(&td, methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
    assert_eq!((st.password.failures, st.password.state), (0, EnrollPasswordState::Enabled));
    let joined = attempt(PW).await.unwrap();
    assert_eq!((joined.method, joined.node.id.as_str()), (EnrollMethod::Password, node_id.as_str()));
    assert_device_works(&hub, home.path(), &node_id).await;
    let ev = web.next_note(|n| n.method == methods::NODE_SECURITY_EVENT && n.params["kind"] == "device_enrolled").await;
    assert_eq!(ev.params["device_name"], json!("cli-real"));
}

/// 这个 home 里还没有任何节点（失败的尝试不落盘）。
fn home_has_no_node(hub: &RealHub, home: &std::path::Path) -> bool {
    let dir = Paths::new(home).client_dir(&crate::remote::host_of(&hub.url()).unwrap());
    let st = HubState::load(&dir, &hub.url());
    st.nodes.is_empty() && st.token.is_none()
}
