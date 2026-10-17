//! 到 hub 的出站 WebSocket：认证、信封收发、指数退避重连、睡眠唤醒检测（docs/spec/daemon.md § 15）。
//!
//! 第一帧是 `Control::Auth`。已有设备 token 时带 token；首次启动没有 token，以 `device_token: None` 的临时身份连接，
//! 只用来配对——第一次 `PairDone` 带回正式 `device_id` / `device_token`，写进 `keys/`，随后用 token 重连（docs/protocol.md § 4.1「daemon 的首次身份」）。
//!
//! 本模块也提供给 CLI 复用的小工具：[`connect_ws`]、[`http_get`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use anycon_protocol::envelope::frame;
use anycon_protocol::{close_code, AuthFailReason, ClientInfo, Control, DeviceKind, Envelope, Kind, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::agents::Agents;
use crate::config::ConfigStore;
use crate::flow::{RatePause, Retained, SendFlow};
use crate::identity::Identity;
use crate::peers::Peers;
use crate::rpc::Rpc;
use crate::util::now_ms;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------- 连接工具

/// `http(s)://host[:port][/base]` -> (`ws(s)://host[:port][/base]/v1/ws`, origin)。
pub fn ws_url(hub_url: &str) -> Result<(String, String), String> {
    let u = hub_url.trim().trim_end_matches('/');
    let (scheme, rest) = u.split_once("://").ok_or_else(|| format!("invalid hub url: {hub_url}"))?;
    let ws_scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(format!("unsupported hub url scheme {other:?}")),
    };
    if rest.is_empty() {
        return Err(format!("invalid hub url: {hub_url}"));
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    let http_scheme = if ws_scheme == "wss" { "https" } else { "http" };
    Ok((format!("{ws_scheme}://{rest}/v1/ws"), format!("{http_scheme}://{authority}")))
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// 连接 hub 的 WebSocket（15 秒超时）。带上 hub 自己的 origin，通过 hub 的 Origin 校验。
pub async fn connect_ws(hub_url: &str) -> Result<WsStream, String> {
    connect_ws_with_cookie(hub_url, None).await
}

/// [`connect_ws`]，升级请求里再带一个 `Cookie` 头（`name=value`）。账号内接入（ADR-36）：CLI 临时登录拿到的会话 cookie 放在这里，
/// hub 据此知道这条临时连接属于哪个账号。cookie 只在内存里，不写日志。
pub async fn connect_ws_with_cookie(hub_url: &str, cookie: Option<&str>) -> Result<WsStream, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (url, origin) = ws_url(hub_url)?;
    let mut req = url.as_str().into_client_request().map_err(|e| e.to_string())?;
    if let Ok(v) = origin.parse() {
        req.headers_mut().insert("Origin", v);
    }
    if let Some(c) = cookie {
        req.headers_mut().insert("Cookie", c.parse().map_err(|_| "invalid cookie".to_string())?);
    }
    let fut = tokio_tungstenite::connect_async_tls_with_config(req, None, true, Some(Connector::Rustls(tls_config())));
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Err(_) => Err("connection timed out".into()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok((ws, _))) => Ok(ws),
    }
}

/// 极简 HTTP GET（doctor 的 `/healthz` 用）。返回 (状态码, body 前 4 KiB)。
pub async fn http_get(url: &str, timeout: Duration) -> Result<(u16, String), String> {
    let work = async {
        let (scheme, rest) = url.split_once("://").ok_or("invalid url")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let tls = scheme == "https";
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let (h, p) = stripped.split_once(']').ok_or("invalid ipv6 host")?;
            (
                h.to_string(),
                p.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()).unwrap_or(if tls { 443 } else { 80 }),
            )
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            (h.to_string(), p.parse::<u16>().map_err(|_| "invalid port")?)
        } else {
            (authority.to_string(), if tls { 443 } else { 80 })
        };
        let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(|e| e.to_string())?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nUser-Agent: anycon-doctor\r\nAccept: */*\r\n\r\n");
        let mut buf = Vec::new();
        if tls {
            let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
            let mut s =
                tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp).await.map_err(|e| e.to_string())?;
            s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
            let _ = s.take(8192).read_to_end(&mut buf).await;
        } else {
            let mut s = tcp;
            s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
            let _ = (&mut s).take(8192).read_to_end(&mut buf).await;
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 =
            text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("malformed HTTP response")?;
        let body = text.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
        Ok::<_, String>((status, body))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r,
        Err(_) => Err("timed out".into()),
    }
}

/// [`http_request`] 的响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// 响应头，名字已转小写。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 第一个叫 `name` 的头（不分大小写）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    /// 所有叫 `name` 的头（`Set-Cookie` 可以有好几个）。
    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().filter(|(k, _)| *k == name).map(|(_, v)| v.as_str()).collect()
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("unexpected response body: {e}"))
    }
}

/// 响应体最多读这么多（CLI 只拿 `/api/config`、`/api/devices` 这类很小的 JSON）。
const HTTP_MAX_BODY: usize = 2 * 1024 * 1024;

/// 极简 HTTP/1.1 客户端（`Connection: close`，支持 `chunked`）：CLI 的账号内接入用它登录（`POST /auth/token`）和列设备。
/// `body` 有就按 JSON 发。整个请求有 `timeout`。
pub async fn http_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let work = async {
        let (scheme, rest) = url.split_once("://").ok_or("invalid url")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let tls = scheme == "https";
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let (h, p) = stripped.split_once(']').ok_or("invalid ipv6 host")?;
            (
                h.to_string(),
                p.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()).unwrap_or(if tls { 443 } else { 80 }),
            )
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            (h.to_string(), p.parse::<u16>().map_err(|_| "invalid port")?)
        } else {
            (authority.to_string(), if tls { 443 } else { 80 })
        };
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nUser-Agent: anycon-cli\r\nAccept: application/json\r\n");
        for (k, v) in headers {
            if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                return Err("invalid header".to_string());
            }
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
        } else if method != "GET" {
            req.push_str("Content-Length: 0\r\n");
        }
        req.push_str("\r\n");
        let mut bytes = req.into_bytes();
        if let Some(b) = body {
            bytes.extend_from_slice(b);
        }
        let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        if tls {
            let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
            let mut s =
                tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp).await.map_err(|e| e.to_string())?;
            s.write_all(&bytes).await.map_err(|e| e.to_string())?;
            // 服务端不发 close_notify 就关连接时 `read_to_end` 会报错，但数据已经读到了
            let _ = (&mut s).take(HTTP_MAX_BODY as u64 + 16 * 1024).read_to_end(&mut buf).await;
        } else {
            let mut s = tcp;
            s.write_all(&bytes).await.map_err(|e| e.to_string())?;
            let _ = (&mut s).take(HTTP_MAX_BODY as u64 + 16 * 1024).read_to_end(&mut buf).await;
        }
        parse_http_response(&buf)
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r,
        Err(_) => Err("timed out".into()),
    }
}

/// 解析一个读到 EOF 的 HTTP/1.x 响应（含 `chunked`）。
fn parse_http_response(buf: &[u8]) -> Result<HttpResponse, String> {
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed HTTP status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let raw = &buf[split + 4..];
    let chunked = headers.iter().any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        let mut out = Vec::new();
        let mut rest = raw;
        loop {
            let eol = rest.windows(2).position(|w| w == b"\r\n").ok_or("malformed chunked body")?;
            let size_text = String::from_utf8_lossy(&rest[..eol]).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| "malformed chunk size")?;
            rest = &rest[eol + 2..];
            if size == 0 {
                break;
            }
            if rest.len() < size {
                return Err("truncated chunked body".into());
            }
            out.extend_from_slice(&rest[..size]);
            if out.len() > HTTP_MAX_BODY {
                return Err("response body too large".into());
            }
            rest = rest.get(size + 2..).ok_or("malformed chunked body")?;
        }
        out
    } else {
        let content_length =
            headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok());
        let body = match content_length {
            Some(n) if n <= raw.len() => &raw[..n],
            _ => raw,
        };
        if body.len() > HTTP_MAX_BODY {
            return Err("response body too large".into());
        }
        body.to_vec()
    };
    Ok(HttpResponse { status, headers, body })
}

pub fn platform() -> String {
    let os = std::env::consts::OS;
    format!("{os}-{}", std::env::consts::ARCH)
}

// ---------------------------------------------------------------- 出站通道

pub use crate::flow::RETAIN_BYTES;

/// 重发时一批最多多少字节；批与批之间留一点时间，让这批里又被退回的回执先到。
const RESEND_BATCH_BYTES: usize = 2 * 1024 * 1024;
const RESEND_GAP: Duration = Duration::from_millis(10);

/// 发往一个对端的信封流 + 重发任务是否已经在跑。
struct Slot {
    flow: SendFlow,
    task: bool,
}

impl Slot {
    fn new(guard: Duration) -> Slot {
        Slot { flow: SendFlow::with_guard(guard), task: false }
    }
}

/// `on_backpressure` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bounce {
    /// 被退回的 seq 还在保留缓冲里：已排进重发。
    Scheduled,
    /// 它已经不在保留缓冲里（或根本不是我们保留的）：这个对端的传输少了一封，必须重置。
    /// 出站通道已经用一封 RESET 占住这个 seq，让 hub 的粘住状态解开。
    Lost,
}

struct OutboxInner {
    seq: u64,
    device_id: Option<String>,
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// 每次 `attach`（新的 hub 连接）加一。
    epoch: u64,
    attached_at: Option<Instant>,
    flows: HashMap<String, Slot>,
    /// 整条 hub 连接的出站暂停（`rate_limited`）。
    rate: RatePause,
    group: u64,
    /// 新建发送流用的「在途保护」时长（[`crate::flow::IN_FLIGHT_GUARD`]；测试里可以缩短）。
    guard: Duration,
}

/// 发往 hub 的出站通道。`seq` 在这里统一编号；没连上时发送返回 `None` / `false`。
///
/// **发送方规则（docs/protocol.md § 3.1，状态机在 [`crate::flow`]）**：`data` / `notify` 信封按目标对端各自保留原文
/// （[`RETAIN_BYTES`]）。hub 退回一封（`backpressure`）之后：暂停向该对端发送（新的信封只入保留缓冲、排队），等一会儿
/// （`rate_limited` 给的 `retry_after_ms`，没有就 100–300 ms 加抖动），把所有被退回的 seq **按升序原样**重发，再继续发新的；
/// 重发期间又被退回就再等一轮。`rate_limited` 暂停整条连接的出站。
pub struct HubOutbox {
    inner: Mutex<OutboxInner>,
    /// 已经交给出站通道、但连接任务还没写进 WebSocket 的字节数（发送方据此自己限速，见 [`backlog`](Self::backlog)）。
    backlog: AtomicUsize,
    /// 收到过多少条 `backpressure`（测试 / 诊断）。
    bounces: std::sync::atomic::AtomicU64,
    /// 其中有多少条退回的 seq 已经不在保留缓冲里（[`Bounce::Lost`]；测试 / 诊断）。
    lost: std::sync::atomic::AtomicU64,
    me: Weak<HubOutbox>,
}

impl HubOutbox {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| HubOutbox {
            inner: Mutex::new(OutboxInner {
                seq: 0,
                device_id: None,
                tx: None,
                epoch: 0,
                attached_at: None,
                flows: HashMap::new(),
                rate: RatePause::default(),
                group: 0,
                guard: crate::flow::IN_FLIGHT_GUARD,
            }),
            backlog: AtomicUsize::new(0),
            bounces: std::sync::atomic::AtomicU64::new(0),
            lost: std::sync::atomic::AtomicU64::new(0),
            me: me.clone(),
        })
    }

    /// 把一帧交给出站通道，并记入积压。
    fn push_tx(&self, tx: &mpsc::UnboundedSender<Vec<u8>>, bytes: Vec<u8>) -> bool {
        let n = bytes.len();
        self.backlog.fetch_add(n, Ordering::SeqCst);
        let ok = tx.send(bytes).is_ok();
        if !ok {
            self.wire_done(n);
        }
        ok
    }

    /// 连接任务把 `n` 字节写进了 WebSocket（或者放弃了它们）：积压减少。
    pub fn wire_done(&self, n: usize) {
        let _ = self.backlog.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| Some(b.saturating_sub(n)));
    }

    /// 出站通道里还没写出去的字节数。写任务据此限速：积压太多时先等线路腾出来——否则保留缓冲里「还没真正发出去」的
    /// 帧会被当成「已发出」逐出，之后 hub 退回它们时就重发不了。
    pub fn backlog(&self) -> usize {
        self.backlog.load(Ordering::SeqCst)
    }

    /// 等到积压降到 `limit` 以下（几毫秒轮询一次）。
    pub async fn wait_backlog_below(&self, limit: usize) {
        while self.backlog() > limit {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    pub fn attach(&self, device_id: &str, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut i = self.inner.lock().unwrap();
        i.device_id = Some(device_id.to_string());
        i.tx = Some(tx);
        i.epoch += 1;
        i.attached_at = Some(Instant::now());
        // 新连接上 hub 的状态是干净的，旧连接上没送出的东西不再有意义（对端会重新握手）
        i.flows.clear();
        i.rate.clear();
        self.backlog.store(0, Ordering::SeqCst);
    }

    /// 当前是第几条 hub 连接（每次重连加一）。
    pub fn epoch(&self) -> u64 {
        self.inner.lock().unwrap().epoch
    }

    /// 当前这条连接已经开了多久。
    pub fn connected_for(&self) -> Option<Duration> {
        let i = self.inner.lock().unwrap();
        i.tx.as_ref().and(i.attached_at).map(|t| t.elapsed())
    }

    pub fn detach(&self) {
        let mut i = self.inner.lock().unwrap();
        i.tx = None;
        i.flows.clear();
        i.rate.clear();
        self.backlog.store(0, Ordering::SeqCst);
    }

    /// `PairDone` 之后同一条连接就是正式设备：后续信封的 `from` 换成正式 id。
    pub fn set_device_id(&self, device_id: &str) {
        self.inner.lock().unwrap().device_id = Some(device_id.to_string());
    }

    pub fn connected(&self) -> bool {
        self.inner.lock().unwrap().tx.is_some()
    }

    pub fn device_id(&self) -> Option<String> {
        self.inner.lock().unwrap().device_id.clone()
    }

    pub fn send_control(&self, c: &Control) -> bool {
        let mut i = self.inner.lock().unwrap();
        let Some(tx) = i.tx.clone() else { return false };
        i.seq += 1;
        // control 帧的 from / to 都填空串（hub 不看）
        match Envelope::control("", i.seq, c).and_then(|e| e.encode()) {
            Ok(bytes) => self.push_tx(&tx, bytes),
            Err(_) => false,
        }
    }

    /// 新开一个分片组：一条消息的所有分片用同一个组号发送（[`send_in_group`](Self::send_in_group)），
    /// 这样保留缓冲不会把最新那条消息的前几片逐出。
    pub fn new_group(&self) -> u64 {
        let mut i = self.inner.lock().unwrap();
        i.group += 1;
        i.group
    }

    /// 发一封信封，返回它的 seq（`None` = 没连上 hub）。`data` / `notify` 会被保留，并受发送方规则约束：
    /// 目标对端被退回过 / 被限速时，信封只入保留缓冲，等重发时一起按序发出。
    pub fn send_envelope(&self, to: &str, kind: Kind, body: Vec<u8>) -> Option<u64> {
        self.send_in_group(to, kind, body, None)
    }

    /// 同 [`send_envelope`](Self::send_envelope)，但属于 [`new_group`](Self::new_group) 开的那一组。
    pub fn send_in_group(&self, to: &str, kind: Kind, body: Vec<u8>, group: Option<u64>) -> Option<u64> {
        let mut i = self.inner.lock().unwrap();
        let tx = i.tx.clone()?;
        i.seq += 1;
        let seq = i.seq;
        let from = i.device_id.clone().unwrap_or_default();
        if !matches!(kind, Kind::Data | Kind::Notify) {
            // 握手信封不受粘住规则管
            let bytes = Envelope::new(from, to, kind, seq, body).encode().ok()?;
            return self.push_tx(&tx, bytes).then_some(seq);
        }
        let now = Instant::now();
        let group = group.unwrap_or_else(|| {
            i.group += 1;
            i.group
        });
        let (rate, guard) = (i.rate, i.guard);
        let slot = i.flows.entry(to.to_string()).or_insert_with(|| Slot::new(guard));
        let blocked = slot.flow.blocked(now, &rate);
        let bytes = if blocked { None } else { Some(Envelope::new(from, to, kind, seq, body.clone()).encode().ok()?) };
        slot.flow.retain(Retained { seq, kind, body, group });
        match bytes {
            Some(b) => self.push_tx(&tx, b).then_some(seq),
            None => {
                slot.flow.defer(seq);
                self.ensure_task(&mut i, to);
                Some(seq)
            }
        }
    }

    /// 向 `to` 的发送现在是否被暂停（有被退回等着重发的、重发还没到点、或整条连接被限速）。
    pub fn is_blocked(&self, to: &str) -> bool {
        let i = self.inner.lock().unwrap();
        let rate = i.rate;
        i.flows.get(to).is_some_and(|s| s.flow.blocked(Instant::now(), &rate)) || rate.until(Instant::now()).is_some()
    }

    /// 等到向 `to` 的发送不再被暂停（轮询，几毫秒一次；只在被退回 / 限速时才会等）。
    pub async fn wait_clear(&self, to: &str) {
        while self.is_blocked(to) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 现在能不能开始向 `to` 写一条 `msg_bytes` 字节（密文）的新消息？不能 = 保留缓冲里全是可能还在路上的信封，
    /// 写下去只能逐出其中的一部分；写任务应该等一会儿再问（见 [`SendFlow::has_room`]）。
    pub fn has_room(&self, to: &str, msg_bytes: usize) -> bool {
        let mut i = self.inner.lock().unwrap();
        i.flows.get_mut(to).is_none_or(|s| s.flow.has_room(Instant::now(), msg_bytes))
    }

    /// 缩短「发出多久之内的信封不逐出」（默认 [`crate::flow::IN_FLIGHT_GUARD`]）。只给假 hub 的测试用：
    /// 假 hub 立刻处理每一封，不需要 10 秒的保守等待。
    pub fn set_in_flight_guard(&self, guard: Duration) {
        let mut i = self.inner.lock().unwrap();
        i.guard = guard;
        for s in i.flows.values_mut() {
            s.flow.set_guard(guard);
        }
    }

    /// 这个 seq 是发给谁的（还在保留缓冲里的话）。
    pub fn peer_of_seq(&self, seq: u64) -> Option<String> {
        let i = self.inner.lock().unwrap();
        i.flows.iter().find(|(_, s)| s.flow.contains(seq)).map(|(to, _)| to.clone())
    }

    /// 还没发出去（被退回的或暂停期间排队的）信封数量。
    pub fn outstanding(&self, to: &str) -> usize {
        self.inner.lock().unwrap().flows.get(to).map_or(0, |s| s.flow.outstanding().len())
    }

    /// 保留缓冲里有多少字节（测试 / 诊断用）。
    pub fn retained_bytes(&self, to: &str) -> usize {
        self.inner.lock().unwrap().flows.get(to).map_or(0, |s| s.flow.retained_bytes())
    }

    /// hub 退回了发给 `to` 的 `seq`（`backpressure`）。
    pub fn on_backpressure(&self, to: &str, seq: u64) -> Bounce {
        self.bounces.fetch_add(1, Ordering::Relaxed);
        let mut i = self.inner.lock().unwrap();
        let now = Instant::now();
        let hint = i.rate.hint(now);
        let ok = i.flows.get_mut(to).is_some_and(|s| s.flow.bounce(seq, now, hint));
        if !ok {
            self.lost.fetch_add(1, Ordering::Relaxed);
            // 保留缓冲里已经没有它了：用一封 RESET 顶替这个 seq，hub 才会解开粘住（对端的传输由调用方重置）。
            // 占位帧和别的信封一样走暂停 / 重发（再被退回也按退避等，不和 hub 来回狂发）
            i.group += 1;
            let (group, guard) = (i.group, i.guard);
            let slot = i.flows.entry(to.to_string()).or_insert_with(|| Slot::new(guard));
            slot.flow.adopt(Retained { seq, kind: Kind::Data, body: frame::reset_body(), group });
            slot.flow.bounce(seq, now, hint);
            self.ensure_task(&mut i, to);
            return Bounce::Lost;
        }
        self.ensure_task(&mut i, to);
        Bounce::Scheduled
    }

    /// hub 说发送方超过了速率预算（`rate_limited`）：整条连接的出站暂停 `retry_after_ms`。
    pub fn on_rate_limited(&self, retry_after_ms: u64) {
        self.inner.lock().unwrap().rate.limited(Instant::now(), Duration::from_millis(retry_after_ms));
    }

    /// 到目前为止收到过多少条 `backpressure`（测试 / 诊断用）。
    pub fn bounce_count(&self) -> u64 {
        self.bounces.load(Ordering::Relaxed)
    }

    /// 到目前为止有多少条 `backpressure` 退回的 seq 已经不在保留缓冲里（测试 / 诊断用；正常应该是 0）。
    pub fn lost_count(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// 整条连接的出站还要被暂停多久（0 = 没有）。
    pub fn rate_pause_left(&self) -> Duration {
        self.inner.lock().unwrap().rate.left(Instant::now())
    }

    /// 放掉空闲对端的陈旧保留（超过 [`crate::flow::RETAIN_MAX_AGE`]）：回执早就不会来了，不该一直占着内存。定期调用。
    pub fn trim(&self) {
        let now = Instant::now();
        let mut i = self.inner.lock().unwrap();
        i.flows.retain(|_, slot| slot.task || !slot.flow.expire(now, crate::flow::RETAIN_MAX_AGE));
    }

    /// 不再向 `to` 发了（会话作废 / 对端上下线 / 吊销）：丢掉它的保留缓冲。还有没发出去的 seq 时，hub 那边可能还
    /// 在等它们的重发（粘住），所以每个 seq 用一封 RESET 顶替发出去，把粘住状态解开（对端没有传输时会忽略它）。
    pub fn forget(&self, to: &str) {
        let mut i = self.inner.lock().unwrap();
        let Some(mut slot) = i.flows.remove(to) else { return };
        let pending = slot.flow.take_outstanding();
        let Some(tx) = i.tx.clone().filter(|_| !pending.is_empty()) else { return };
        let from = i.device_id.clone().unwrap_or_default();
        for seq in pending {
            if let Ok(b) = Envelope::new(from.clone(), to, Kind::Data, seq, frame::reset_body()).encode() {
                self.push_tx(&tx, b);
            }
        }
    }

    fn ensure_task(&self, i: &mut OutboxInner, to: &str) {
        let epoch = i.epoch;
        let Some(slot) = i.flows.get_mut(to) else { return };
        if slot.task {
            return;
        }
        let (Some(me), Ok(_)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        slot.task = true;
        let to = to.to_string();
        tokio::spawn(me.flow_task(to, epoch));
    }

    /// 重发任务：等到暂停结束（该对端的退回暂停、整条连接的限速暂停，取晚的），然后把待发的按 seq 升序、
    /// 原样、分批发出去；发空就退出。
    async fn flow_task(self: Arc<Self>, to: String, epoch: u64) {
        loop {
            let wait = {
                let mut i = self.inner.lock().unwrap();
                let now = Instant::now();
                let rate = i.rate;
                let (tx, from, live) = (i.tx.clone(), i.device_id.clone().unwrap_or_default(), i.epoch == epoch);
                let Some(slot) = i.flows.get_mut(&to) else { return };
                let Some(tx) = tx.filter(|_| live) else {
                    slot.task = false;
                    return;
                };
                match slot.flow.ready_at(&rate, now) {
                    None => {
                        slot.task = false;
                        return;
                    }
                    Some(t) if t > now => Some(t - now),
                    Some(_) => {
                        for (seq, item) in slot.flow.take_batch(now, RESEND_BATCH_BYTES) {
                            // 保留缓冲里没有（不该发生）就顶替成 RESET，别让 hub 一直等
                            let (kind, body) = item.unwrap_or_else(|| (Kind::Data, frame::reset_body()));
                            if let Ok(b) = Envelope::new(from.clone(), &to, kind, seq, body).encode() {
                                self.push_tx(&tx, b);
                            }
                        }
                        if slot.flow.outstanding().is_empty() {
                            slot.task = false;
                            return;
                        }
                        Some(RESEND_GAP)
                    }
                }
            };
            if let Some(w) = wait {
                tokio::time::sleep(w).await;
            }
        }
    }
}

// ---------------------------------------------------------------- 状态

#[derive(Debug, Clone, Default)]
pub struct HubStatus {
    pub configured: bool,
    pub connected: bool,
    pub url: Option<String>,
    pub last_error: Option<String>,
    pub last_connect_ms: Option<u64>,
    pub auth_fail: Option<String>,
    /// hub 时间 - 本机时间（毫秒）
    pub clock_skew_ms: Option<i64>,
    pub device_id: Option<String>,
    pub attempts: u64,
    pub authenticated_with_token: bool,
}

#[derive(Default)]
pub struct HubState(RwLock<HubStatus>);

impl HubState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn get(&self) -> HubStatus {
        self.0.read().unwrap().clone()
    }
    fn update(&self, f: impl FnOnce(&mut HubStatus)) {
        f(&mut self.0.write().unwrap());
    }
}

// ---------------------------------------------------------------- 连接循环

enum Outcome {
    Closed {
        code: Option<u16>,
        authed: bool,
    },
    Error {
        msg: String,
        authed: bool,
    },
    AuthFail(AuthFailReason),
    /// 睡眠唤醒或拿到新 token：立即重连
    Reconnect,
    Shutdown,
}

pub struct HubLink {
    pub config: Arc<ConfigStore>,
    pub identity: Arc<Identity>,
    pub outbox: Arc<HubOutbox>,
    pub state: Arc<HubState>,
    pub peers: Arc<Peers>,
    pub agents: Arc<Agents>,
    pub wake: Arc<Notify>,
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::thread_rng().gen_range(0.75..1.25))
}

impl HubLink {
    pub fn spawn(self: Arc<Self>, rpc: Arc<Rpc>, shutdown: watch::Receiver<bool>) -> Vec<tokio::task::JoinHandle<()>> {
        let a = {
            let this = self.clone();
            let sd = shutdown.clone();
            tokio::spawn(async move { this.run(rpc, sd).await })
        };
        let b = {
            let this = self.clone();
            tokio::spawn(async move { this.sleep_detector().await })
        };
        vec![a, b]
    }

    /// 每 5 秒比较「墙钟经过的时间」和「单调时钟经过的时间」：睡眠期间单调时钟不走（macOS / Linux 都是），
    /// 墙钟走，所以两者之差超过 30 秒就是刚睡醒。
    async fn sleep_detector(self: Arc<Self>) {
        let mut last_mono = Instant::now();
        let mut last_wall = SystemTime::now();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let (mono, wall) = (Instant::now(), SystemTime::now());
            let dm = mono.duration_since(last_mono);
            let dw = wall.duration_since(last_wall).unwrap_or(Duration::ZERO);
            last_mono = mono;
            last_wall = wall;
            if dw > dm + Duration::from_secs(30) {
                tracing::info!(
                    slept_s = (dw - dm).as_secs(),
                    "wake from sleep detected; reconnecting and re-probing agents"
                );
                self.wake.notify_one();
                self.agents.reprobe_all_soon();
            }
        }
    }

    async fn run(self: Arc<Self>, rpc: Arc<Rpc>, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if *shutdown.borrow() {
                return;
            }
            let cfg = self.config.get();
            let Some(url) = cfg.hub.url.clone().filter(|u| !u.trim().is_empty()) else {
                self.state.update(|s| s.configured = false);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            };
            self.state.update(|s| {
                s.configured = true;
                s.url = Some(url.clone());
                s.attempts += 1;
            });
            let started = Instant::now();
            let outcome = self.session(&url, &rpc, &mut shutdown).await;
            self.outbox.detach();
            self.peers.on_hub_down().await;
            self.state.update(|s| s.connected = false);
            let delay = match outcome {
                Outcome::Shutdown => return,
                Outcome::Reconnect => Duration::ZERO,
                Outcome::AuthFail(reason) => {
                    let msg = format!("hub rejected authentication: {reason:?}");
                    tracing::error!("{msg}; retrying in 10 minutes");
                    self.state.update(|s| {
                        s.last_error = Some(msg);
                        s.auth_fail = Some(format!("{reason:?}").to_lowercase());
                    });
                    match reason {
                        AuthFailReason::Quota => Duration::from_secs(60),
                        AuthFailReason::Revoked => Duration::from_secs(3600),
                        _ => Duration::from_secs(600),
                    }
                }
                Outcome::Closed { code, authed } => {
                    tracing::info!(?code, "hub connection closed");
                    if authed && started.elapsed() > Duration::from_secs(30) {
                        backoff = Duration::from_secs(1);
                    }
                    if code == Some(close_code::RESTARTING) {
                        Duration::from_secs_f64(rand::thread_rng().gen_range(1.0..3.0))
                    } else if code == Some(close_code::SUPERSEDED) {
                        // 同一设备在别处连上了：别互相踢，退避久一点
                        let msg = "another daemon instance is using this device identity (hub closed the connection with 4001 superseded)";
                        tracing::warn!("{msg}");
                        self.state.update(|s| s.last_error = Some(msg.into()));
                        let d = jitter(backoff.max(Duration::from_secs(30)));
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        d
                    } else if code == Some(close_code::REVOKED) {
                        let msg = "this device was revoked on the hub; re-pair it (`anycon init --force`, then `anycon pair`)";
                        tracing::error!("{msg}");
                        self.state.update(|s| {
                            s.last_error = Some(msg.into());
                            s.auth_fail = Some("revoked".into());
                        });
                        Duration::from_secs(3600)
                    } else {
                        let d = jitter(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        d
                    }
                }
                Outcome::Error { msg, authed } => {
                    tracing::warn!(error = %msg, "hub connection failed");
                    self.state.update(|s| s.last_error = Some(msg));
                    if authed && started.elapsed() > Duration::from_secs(30) {
                        backoff = Duration::from_secs(1);
                    }
                    let d = jitter(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    d
                }
            };
            if delay.is_zero() {
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.changed() => {}
                _ = self.wake.notified() => { backoff = Duration::from_secs(1); }
                _ = self.peers.reconnect.notified() => { backoff = Duration::from_secs(1); }
            }
        }
    }

    async fn session(&self, url: &str, rpc: &Arc<Rpc>, shutdown: &mut watch::Receiver<bool>) -> Outcome {
        let mut ws = match connect_ws(url).await {
            Ok(w) => w,
            Err(e) => return Outcome::Error { msg: e, authed: false },
        };
        let token = self.identity.device_token();
        let cfg = self.config.get();
        let auth = Control::Auth {
            device_token: token.clone(),
            protocol_version: PROTOCOL_VERSION,
            client: ClientInfo {
                name: cfg.node_name(),
                kind: DeviceKind::Daemon,
                platform: platform(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                origin: None,
            },
        };
        let first = match Envelope::control("", 0, &auth).and_then(|e| e.encode()) {
            Ok(b) => b,
            Err(e) => return Outcome::Error { msg: e.to_string(), authed: false },
        };
        if let Err(e) = ws.send(Message::Binary(first.into())).await {
            return Outcome::Error { msg: e.to_string(), authed: false };
        }
        // 等 AuthOk / AuthFail
        let deadline = Instant::now() + Duration::from_secs(10);
        let device_id = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, ws.next()).await {
                Err(_) => return Outcome::Error { msg: "timed out waiting for auth_ok".into(), authed: false },
                Ok(None) => return Outcome::Error { msg: "connection closed during auth".into(), authed: false },
                Ok(Some(Err(e))) => return Outcome::Error { msg: e.to_string(), authed: false },
                Ok(Some(Ok(Message::Binary(b)))) => {
                    let Ok(env) = Envelope::decode(&b) else { continue };
                    if env.kind != Kind::Control {
                        continue;
                    }
                    match env.control_body() {
                        Ok(Control::AuthOk { device_id, server_time_ms, peers_online, .. }) => {
                            self.state.update(|s| {
                                s.clock_skew_ms = Some(server_time_ms as i64 - now_ms() as i64);
                                s.device_id = Some(device_id.clone());
                                s.connected = true;
                                s.last_error = None;
                                s.auth_fail = None;
                                s.last_connect_ms = Some(now_ms());
                                s.authenticated_with_token = token.is_some();
                            });
                            if token.is_some() {
                                self.identity.note_device_id(&device_id);
                            }
                            self.peers.set_online_list(&peers_online);
                            break device_id;
                        }
                        Ok(Control::AuthFail { reason }) => return Outcome::AuthFail(reason),
                        _ => continue,
                    }
                }
                Ok(Some(Ok(Message::Close(f)))) => {
                    return Outcome::Closed { code: f.map(|f| u16::from(f.code)), authed: false }
                }
                Ok(Some(Ok(_))) => continue,
            }
        };
        tracing::info!(device_id = %device_id, token = token.is_some(), "connected to hub");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.outbox.attach(&device_id, out_tx);
        let reconnect = self.peers.reconnect.clone();
        loop {
            tokio::select! {
                frame = ws.next() => {
                    match frame {
                        None => return Outcome::Closed { code: None, authed: true },
                        Some(Err(e)) => return Outcome::Error { msg: e.to_string(), authed: true },
                        Some(Ok(Message::Binary(b))) => self.on_frame(&b, rpc).await,
                        Some(Ok(Message::Ping(_))) => { let _ = ws.flush().await; }
                        Some(Ok(Message::Close(f))) => return Outcome::Closed { code: f.map(|f| u16::from(f.code)), authed: true },
                        Some(Ok(_)) => {}
                    }
                }
                out = out_rx.recv() => {
                    let Some(bytes) = out else { return Outcome::Closed { code: None, authed: true } };
                    let n = bytes.len();
                    if let Err(e) = ws.send(Message::Binary(bytes.into())).await {
                        return Outcome::Error { msg: e.to_string(), authed: true };
                    }
                    self.outbox.wire_done(n);
                    // 批量发：把队列里已有的一并写出去再 flush
                    while let Ok(more) = out_rx.try_recv() {
                        let n = more.len();
                        if let Err(e) = ws.feed(Message::Binary(more.into())).await {
                            return Outcome::Error { msg: e.to_string(), authed: true };
                        }
                        self.outbox.wire_done(n);
                    }
                    if let Err(e) = ws.flush().await {
                        return Outcome::Error { msg: e.to_string(), authed: true };
                    }
                }
                _ = shutdown.changed() => {
                    let _ = ws.close(None).await;
                    return Outcome::Shutdown;
                }
                _ = self.wake.notified() => {
                    let _ = ws.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame { code: CloseCode::Normal, reason: "wake".into() })).await;
                    return Outcome::Reconnect;
                }
                _ = reconnect.notified() => {
                    let _ = ws.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame { code: CloseCode::Normal, reason: "re-auth".into() })).await;
                    return Outcome::Reconnect;
                }
            }
        }
    }

    async fn on_frame(&self, bytes: &[u8], rpc: &Arc<Rpc>) {
        let env = match Envelope::decode(bytes) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "undecodable envelope");
                return;
            }
        };
        match env.kind {
            Kind::Control => match env.control_body() {
                Ok(c) => self.on_control(c).await,
                Err(e) => tracing::debug!(error = %e, "undecodable control message"),
            },
            Kind::Handshake => self.peers.on_handshake(&env.from, &env.body, rpc).await,
            Kind::Data => self.peers.on_data(&env.from, &env.body).await,
            Kind::Notify => {}
        }
    }

    async fn on_control(&self, c: Control) {
        match c {
            Control::PeerOnline { device } => self.peers.set_peer_online(&device, true),
            Control::PeerOffline { device } => self.peers.set_peer_online(&device, false),
            Control::Undeliverable { to, .. } => self.peers.on_undeliverable(&to),
            Control::Backpressure { to, seq } => self.peers.on_backpressure(&to, seq),
            Control::Dropped { seq, reason } => self.peers.on_dropped(seq, reason),
            Control::RateLimited { retry_after_ms } => {
                // 真的暂停整条连接的出站（不只是记一条日志）：被退回的信封随后按序重发
                tracing::warn!(retry_after_ms, "hub rate limit hit; pausing outbound envelopes");
                self.outbox.on_rate_limited(retry_after_ms);
            }
            Control::TokenRotated { device_token } => {
                if let Err(e) = self.identity.set_device(None, Some(&device_token)) {
                    tracing::error!(error = %e, "failed to store the rotated token");
                }
            }
            // 账号内接入：有设备请求接入这台 daemon / 接入会话被对端取消、断线或超时
            Control::EnrollRequest { enroll_id, peer, .. } => self.peers.enroll.on_request(enroll_id, peer).await,
            Control::EnrollFailed { enroll_id, reason } => self.peers.enroll.on_failed(enroll_id, reason).await,
            c @ (Control::PairRegistered { .. }
            | Control::PairMatched { .. }
            | Control::PairDone { .. }
            | Control::PairFailed { .. }) => self.peers.on_control(c).await,
            Control::QuotaUpdate { .. } | Control::QueueOverflow { .. } => {}
            other => tracing::debug!(?other, "ignored control message"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_conversion() {
        assert_eq!(
            ws_url("http://127.0.0.1:8080").unwrap(),
            ("ws://127.0.0.1:8080/v1/ws".to_string(), "http://127.0.0.1:8080".to_string())
        );
        assert_eq!(
            ws_url("https://hub.example.com/").unwrap(),
            ("wss://hub.example.com/v1/ws".to_string(), "https://hub.example.com".to_string())
        );
        assert_eq!(ws_url("https://example.com/base").unwrap().0, "wss://example.com/base/v1/ws");
        assert_eq!(ws_url("ws://localhost:9").unwrap().0, "ws://localhost:9/v1/ws");
        assert!(ws_url("ftp://x").is_err());
        assert!(ws_url("nonsense").is_err());
        assert!(ws_url("http://").is_err());
    }

    #[test]
    fn outbox_seq_and_detach() {
        let ob = HubOutbox::new();
        assert!(!ob.connected());
        assert!(ob.send_envelope("peer", Kind::Data, vec![1]).is_none());
        assert!(!ob.send_control(&Control::PairBegin { match_id: "0123456789abcdef".into() }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(&"d".repeat(32), tx);
        assert_eq!(ob.send_envelope("peer", Kind::Data, vec![1]), Some(1));
        assert!(ob.send_control(&Control::PairBegin { match_id: "0123456789abcdef".into() }));
        assert_eq!(ob.send_envelope("peer", Kind::Data, vec![2]), Some(3));
        let e1 = Envelope::decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!((e1.seq, e1.kind, e1.from.len()), (1, Kind::Data, 32));
        let e2 = Envelope::decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(e2.kind, Kind::Control);
        assert_eq!(e2.control_body().unwrap(), Control::PairBegin { match_id: "0123456789abcdef".into() });
        let e3 = Envelope::decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!((e3.seq, e3.body), (3, vec![2]));
        ob.detach();
        assert!(ob.send_envelope("peer", Kind::Data, vec![]).is_none());
    }

    fn attached() -> (Arc<HubOutbox>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let ob = HubOutbox::new();
        let (tx, rx) = mpsc::unbounded_channel();
        ob.attach(&"d".repeat(32), tx);
        (ob, rx)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Envelope {
        let b = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("an envelope").unwrap();
        Envelope::decode(&b).unwrap()
    }

    #[tokio::test]
    async fn bounced_envelopes_are_resent_in_ascending_order_after_a_pause_and_new_ones_queue_behind_them() {
        let (ob, mut rx) = attached();
        let seqs: Vec<u64> = (1..=4u8).map(|b| ob.send_envelope("p", Kind::Data, vec![b; 10]).unwrap()).collect();
        for _ in 0..4 {
            next(&mut rx).await;
        }
        // hub 退回 3 和 4（粘住：3 之后的都被退回），回执顺序反着到
        let t = Instant::now();
        assert_eq!(ob.on_backpressure("p", seqs[3]), Bounce::Scheduled);
        assert_eq!(ob.on_backpressure("p", seqs[2]), Bounce::Scheduled);
        assert!(ob.is_blocked("p"));
        // 暂停期间发的新信封不会立刻发出，而是排在被退回的后面
        let fresh = ob.send_envelope("p", Kind::Data, vec![9; 10]).unwrap();
        assert!(rx.try_recv().is_err(), "nothing leaves while paused");
        let order: Vec<(u64, Vec<u8>)> = vec![
            {
                let e = next(&mut rx).await;
                (e.seq, e.body)
            },
            {
                let e = next(&mut rx).await;
                (e.seq, e.body)
            },
            {
                let e = next(&mut rx).await;
                (e.seq, e.body)
            },
        ];
        assert!(t.elapsed() >= Duration::from_millis(90), "default pause is 100-300 ms: {:?}", t.elapsed());
        assert_eq!(order, vec![(seqs[2], vec![3; 10]), (seqs[3], vec![4; 10]), (fresh, vec![9; 10])]);
        assert!(!ob.is_blocked("p"), "the flow is clear once everything is resent");
        // 之后恢复直接发送
        let after = ob.send_envelope("p", Kind::Data, vec![5]).unwrap();
        assert_eq!(next(&mut rx).await.seq, after);
    }

    #[tokio::test]
    async fn a_resend_that_is_bounced_again_waits_another_round_and_the_wait_grows() {
        let (ob, mut rx) = attached();
        let s1 = ob.send_envelope("p", Kind::Data, vec![1]).unwrap();
        next(&mut rx).await;
        let t = Instant::now();
        ob.on_backpressure("p", s1);
        let first = next(&mut rx).await;
        let d1 = t.elapsed();
        assert_eq!(first.seq, s1);
        // 重发又被退回：再等一轮（默认等待加倍，下限 200 ms）
        let t2 = Instant::now();
        ob.on_backpressure("p", s1);
        let second = next(&mut rx).await;
        let d2 = t2.elapsed();
        assert_eq!((second.seq, second.body), (s1, vec![1]));
        assert!(d1 >= Duration::from_millis(90) && d2 >= Duration::from_millis(190), "{d1:?} {d2:?}");
        assert!(!ob.is_blocked("p"));
    }

    #[tokio::test]
    async fn rate_limited_gives_the_wait_and_pauses_everything() {
        let (ob, mut rx) = attached();
        let s1 = ob.send_envelope("p", Kind::Data, vec![1]).unwrap();
        next(&mut rx).await;
        // 速率超限：hub 退回这封，同时说停 350 ms；另一个对端的新信封也要等
        ob.on_backpressure("p", s1);
        ob.on_rate_limited(350);
        let t = Instant::now();
        let other = ob.send_envelope("q", Kind::Data, vec![7]).unwrap();
        assert!(rx.try_recv().is_err());
        let a = next(&mut rx).await;
        let b = next(&mut rx).await;
        assert!(t.elapsed() >= Duration::from_millis(300), "{:?}", t.elapsed());
        let mut got = vec![(a.to.clone(), a.seq), (b.to.clone(), b.seq)];
        got.sort();
        assert_eq!(got, vec![("p".to_string(), s1), ("q".to_string(), other)]);
        assert_eq!(ob.rate_pause_left(), Duration::ZERO);
        // 握手信封不受影响
        let (ob, mut rx) = attached();
        ob.on_rate_limited(10_000);
        let h = ob.send_envelope("p", Kind::Handshake, vec![1]).unwrap();
        assert_eq!(next(&mut rx).await.seq, h);
    }

    #[test]
    fn a_full_window_makes_the_writer_wait_it_never_evicts_what_may_still_be_in_flight() {
        let ob = HubOutbox::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        ob.attach(&"d".repeat(32), tx);
        ob.set_in_flight_guard(Duration::from_millis(150));
        // 20 MiB 刚发出：都在路上，一个都不逐出
        let chunk = vec![0u8; 100 * 1024];
        let seqs: Vec<u64> = (0..200).map(|_| ob.send_envelope("p", Kind::Data, chunk.clone()).unwrap()).collect();
        assert!(ob.retained_bytes("p") > RETAIN_BYTES);
        assert!(seqs.iter().all(|s| ob.peer_of_seq(*s).is_some()), "nothing was evicted");
        assert!(!ob.has_room("p", 4 * 1024 * 1024), "the writer has to wait before the next big message");
        assert!(ob.has_room("p", 1024), "a tiny message (a permission request, say) still goes");
        // 一条 12 MiB 的消息（60 片，同组）照样整条保留：写它的时候不能为了守上限逐出它自己的前几片
        let g = ob.new_group();
        let frag = vec![1u8; 200 * 1024];
        let big: Vec<u64> =
            (0..60).map(|_| ob.send_in_group("p", Kind::Data, frag.clone(), Some(g)).unwrap()).collect();
        assert!(big.iter().all(|s| ob.peer_of_seq(*s).is_some()), "every fragment of the newest message is kept");
        // 保护期过了才腾得出位置
        std::thread::sleep(Duration::from_millis(250));
        assert!(ob.has_room("p", 4 * 1024 * 1024));
        assert!(ob.retained_bytes("p") <= RETAIN_BYTES);
        assert!(ob.peer_of_seq(seqs[0]).is_none(), "the oldest went first");
        // 被退回、还在等重发的信封，再老也不会被逐出
        assert_eq!(ob.on_backpressure("p", big[0]), Bounce::Scheduled);
        for _ in 0..80 {
            ob.send_envelope("p", Kind::Data, frag.clone());
        }
        std::thread::sleep(Duration::from_millis(250));
        ob.has_room("p", 4 * 1024 * 1024);
        assert!(ob.peer_of_seq(big[0]).is_some(), "a pending resend is never evicted");
        assert_eq!(ob.lost_count(), 0);
    }

    #[test]
    fn forgetting_a_flow_with_unsent_seqs_fills_them_with_resets_so_the_hub_is_not_left_waiting() {
        let (ob, mut rx) = {
            let ob = HubOutbox::new();
            let (tx, rx) = mpsc::unbounded_channel();
            ob.attach(&"d".repeat(32), tx);
            (ob, rx)
        };
        let s1 = ob.send_envelope("p", Kind::Data, vec![1]).unwrap();
        let s2 = ob.send_envelope("p", Kind::Data, vec![2]).unwrap();
        while rx.try_recv().is_ok() {}
        ob.on_backpressure("p", s2);
        ob.on_backpressure("p", s1);
        ob.forget("p");
        let r1 = Envelope::decode(&rx.try_recv().unwrap()).unwrap();
        let r2 = Envelope::decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            [(r1.seq, r1.body.clone()), (r2.seq, r2.body.clone())],
            [(s1, frame::reset_body()), (s2, frame::reset_body())]
        );
        assert!(!ob.is_blocked("p") && ob.outstanding("p") == 0 && ob.retained_bytes("p") == 0);
        // 没有等重发的 seq 时静悄悄
        ob.send_envelope("p", Kind::Data, vec![3]);
        while rx.try_recv().is_ok() {}
        ob.forget("p");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn http_responses_parse_with_content_length_chunks_or_close_delimited_bodies() {
        let r = parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\n\r\nhello-and-more").unwrap();
        assert_eq!((r.status, r.body.as_slice(), r.header("x-a")), (200, &b"hello"[..], Some("1")));
        // chunked，分了好几块，还带 chunk 扩展
        let r = parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n6;ext=1\r\n:[1,2]\r\n1\r\n}\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.json::<serde_json::Value>().unwrap(), serde_json::json!({ "a": [1, 2] }));
        // 没有长度：读到连接关闭
        let r = parse_http_response(b"HTTP/1.1 204 No Content\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n").unwrap();
        assert_eq!((r.status, r.body.len(), r.headers_named("Set-Cookie")), (204, 0, vec!["a=1", "b=2"]));
        // 损坏的
        assert!(parse_http_response(b"nonsense").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nshort\r\n").is_err());
    }

    #[tokio::test]
    async fn http_request_posts_json_with_headers_and_returns_status_headers_and_body() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            // 读到请求头和 body（Content-Length 个字节）
            loop {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..i]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if got.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            s.write_all(
                b"HTTP/1.1 201 Created\r\nSet-Cookie: sid=abc; HttpOnly\r\nContent-Length: 11\r\n\r\n{\"ok\":true}\n",
            )
            .await
            .unwrap();
            String::from_utf8(got).unwrap()
        });
        let r = http_request(
            "POST",
            &format!("http://127.0.0.1:{port}/auth/token"),
            &[("X-Anycon", "1"), ("Cookie", "a=b")],
            Some(br#"{"token":"t"}"#),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!((r.status, r.header("set-cookie")), (201, Some("sid=abc; HttpOnly")));
        assert_eq!(r.json::<serde_json::Value>().unwrap(), serde_json::json!({ "ok": true }));
        let req = server.await.unwrap();
        assert!(req.starts_with("POST /auth/token HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("X-Anycon: 1\r\n") && req.contains("Cookie: a=b\r\n"), "{req}");
        assert!(req.ends_with(r#"{"token":"t"}"#) && req.contains("Content-Length: 13"), "{req}");
        // 头里不能夹带换行
        assert!(http_request("GET", "http://127.0.0.1:1/", &[("X", "a\r\nb: c")], None, Duration::from_secs(1))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn http_get_plain() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.unwrap();
        });
        let (status, body) =
            http_get(&format!("http://127.0.0.1:{port}/healthz"), Duration::from_secs(5)).await.unwrap();
        assert_eq!((status, body.as_str()), (200, "ok"));
        assert!(http_get("http://127.0.0.1:1/x", Duration::from_secs(2)).await.is_err());
    }
}
