//! 隧道里一个 stream 的上游一侧（docs/spec/daemon.md § 19.5）：连本机端口，转发 HTTP 请求或 WebSocket。
//!
//! 每个 stream 一个任务，由 `tunnel.rs` 起、也由它在 stream 结束 / `RESET` / 隧道断开时 `abort`（连接随之关闭）。
//! 任务和隧道之间只有两条路：
//!
//! - 隧道 → 任务：[`ToUp`]（请求体、WebSocket 消息、关闭），走无界 channel——数量受 stream 窗口约束；
//! - 任务 → 隧道：[`UpEvent`]（响应头、响应体、WebSocket 消息……），经 [`Emit`] 回调。
//!
//! **发送方向的流控**在这里：往预览页发 `DATA` / `WS_MESSAGE` 之前先向 [`Credit`] 要窗口（预览页用 `WINDOW` 帧归还），
//! 窗口不够就不读上游，上游的 TCP 缓冲满了自然反压到上游服务器。
//!
//! 连接：每个 HTTP stream 一条新的 TCP 连接，先 `127.0.0.1` 再 `[::1]`，5 秒连接超时；用 HTTP/1.1（hyper 的 client 连接，
//! 带 `connection: close`）；等响应头最多 120 秒（从请求发完算起），超时 `RESET { timeout }`。

use std::io::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anycon_protocol::tunnel;
use anycon_protocol::{PreviewBodyEncoding, PreviewRequestHead, PreviewResetCode, PreviewResponseHead};
use bytes::Bytes;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures::{SinkExt, StreamExt};
use http_body::Frame;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};

use super::ledger::DATA_CHUNK;
use super::rewrite::{self, BodyPlan};

/// 超时设置。生产里用 [`Timeouts::default`]；测试缩短。
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// 连本机端口（先 IPv4 再 IPv6，合计）。
    pub connect: Duration,
    /// 请求发完之后等响应头（WebSocket：等上游的握手响应）。
    pub head: Duration,
    /// 预览页要关 WebSocket 之后等上游回 Close 的时间，到点就替上游回了。
    pub ws_close: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { connect: Duration::from_secs(5), head: Duration::from_secs(120), ws_close: Duration::from_secs(5) }
    }
}

// ---------------------------------------------------------------- 发送窗口

/// 往预览页发送的窗口（`DATA` / `WS_MESSAGE` 的数据字节）。初始 256 KiB，预览页用 `WINDOW` 帧增加。
pub struct Credit {
    state: Mutex<CreditState>,
    notify: Notify,
}

struct CreditState {
    avail: u64,
    closed: bool,
}

/// 窗口的上限（和 HTTP/2 一样 2^31 - 1）：预览页给的窗口加起来超过它就是流控错误。
pub const CREDIT_MAX: u64 = (1 << 31) - 1;

impl Credit {
    pub fn new(initial: u32) -> Arc<Credit> {
        Arc::new(Credit {
            state: Mutex::new(CreditState { avail: u64::from(initial), closed: false }),
            notify: Notify::new(),
        })
    }

    /// 预览页给了 `n` 字节窗口。超过 [`CREDIT_MAX`] 返回 `false`（流控错误，窗口不变）。
    pub fn add(&self, n: u32) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.avail + u64::from(n) > CREDIT_MAX {
            return false;
        }
        s.avail += u64::from(n);
        drop(s);
        self.notify.notify_waiters();
        true
    }

    pub fn available(&self) -> u64 {
        self.state.lock().unwrap().avail
    }

    /// stream 结束了：唤醒等窗口的任务，让它们退出。
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_waiters();
    }

    /// 等到窗口里有 `n` 字节并取走它。stream 已经结束返回 `false`。`n` 不能大于窗口上限，调用方保证。
    pub async fn acquire(&self, n: usize) -> bool {
        let n = n as u64;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // 先登记再检查，避免检查之后、等待之前错过 add
            notified.as_mut().enable();
            {
                let mut s = self.state.lock().unwrap();
                if s.closed {
                    return false;
                }
                if s.avail >= n {
                    s.avail -= n;
                    return true;
                }
            }
            notified.await;
        }
    }
}

// ---------------------------------------------------------------- 与隧道之间的消息

/// 隧道 → 上游任务。
#[derive(Debug)]
pub enum ToUp {
    /// 请求体的一段。
    Body(Vec<u8>),
    /// 请求体结束。
    End,
    /// 预览页发来的一条完整 WebSocket 消息。
    WsMessage { binary: bool, data: Vec<u8> },
    /// 预览页要关 WebSocket。
    WsClose { code: u16, reason: String },
}

/// 上游任务 → 隧道。
#[derive(Debug)]
pub enum UpEvent {
    /// 响应头（`RESPONSE` 帧）。
    Head(PreviewResponseHead),
    /// 响应体的一段（`DATA` 帧，已经拿到窗口）。
    Data(Vec<u8>),
    /// 响应体结束。
    End,
    /// 出错 / 超时：`RESET`。
    Reset { code: PreviewResetCode, message: &'static str },
    /// WebSocket 升级成功（`RESPONSE { status: 101 }`）。
    WsOpen(PreviewResponseHead),
    /// WebSocket 消息的一段（已经拿到窗口）。
    WsSegment { binary: bool, fin: bool, data: Vec<u8> },
    /// 上游关闭了 WebSocket（或者出错断开）。
    WsClose { code: u16, reason: String },
    /// 上游读走了这么多字节的请求体 / WebSocket 消息：隧道据此把窗口还给预览页。
    Consumed(u32),
}

/// 把事件交给隧道。隧道已经不在了返回 `false`，任务该退出。
pub type Emit = Arc<dyn Fn(UpEvent) -> bool + Send + Sync>;

/// 一个 stream 的上游任务需要的一切。
pub struct StreamCtx {
    pub port: u16,
    /// 这条隧道的预览 label（改写 `origin` / `referer` 用）。
    pub label: String,
    pub head: PreviewRequestHead,
    pub rx: mpsc::UnboundedReceiver<ToUp>,
    pub credit: Arc<Credit>,
    pub emit: Emit,
    pub timeouts: Timeouts,
}

/// 任务入口。
pub async fn run(ctx: StreamCtx) {
    if ctx.head.websocket {
        run_ws(ctx).await
    } else {
        run_http(ctx).await
    }
}

fn reset(emit: &Emit, code: PreviewResetCode, message: &'static str) {
    emit(UpEvent::Reset { code, message });
}

// ---------------------------------------------------------------- 连接

/// 连本机端口：先 `127.0.0.1` 再 `[::1]`（Vite 在 macOS 上常常只监听 `::1`）。`timeout` 是两次尝试合计的时限。
pub async fn connect_local(port: u16, timeout: Duration) -> Result<TcpStream, std::io::Error> {
    let attempt = async {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        match TcpStream::connect(v4).await {
            Ok(s) => Ok(s),
            Err(first) => {
                let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
                TcpStream::connect(v6).await.map_err(|_| first)
            }
        }
    };
    match tokio::time::timeout(timeout, attempt).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")),
    }
}

/// drop 时 abort 的任务句柄。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------------------------------------------------------------- 响应体编码

/// 边读边 gzip（快速档）。每收到一段就 sync flush：事件流 / 长轮询之类的响应不会被压缩器攒住。
struct Gzip(GzEncoder<Vec<u8>>);

impl Gzip {
    fn new() -> Gzip {
        Gzip(GzEncoder::new(Vec::new(), Compression::fast()))
    }

    fn push(&mut self, data: &[u8]) -> Vec<u8> {
        // 写内存 Vec 不会失败
        let _ = self.0.write_all(data);
        let _ = self.0.flush();
        std::mem::take(self.0.get_mut())
    }

    fn finish(self) -> Vec<u8> {
        self.0.finish().unwrap_or_default()
    }
}

// ---------------------------------------------------------------- HTTP

/// 请求体：先是预览页已经发来的第一段，然后是 channel 里后续的。`End` 之后结束，channel 断开（隧道丢弃了 stream）报错。
struct ReqBody {
    first: Option<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<ToUp>,
    emit: Emit,
    done: Option<oneshot::Sender<()>>,
    finished: bool,
}

impl http_body::Body for ReqBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        if this.finished {
            return Poll::Ready(None);
        }
        let mut chunk = this.first.take();
        loop {
            if let Some(b) = chunk.take() {
                if b.is_empty() {
                    continue;
                }
                (this.emit)(UpEvent::Consumed(b.len() as u32));
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(b)))));
            }
            match this.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(ToUp::Body(b))) => chunk = Some(b),
                Poll::Ready(Some(ToUp::End)) => {
                    this.finished = true;
                    if let Some(d) = this.done.take() {
                        let _ = d.send(());
                    }
                    return Poll::Ready(None);
                }
                // 不属于 HTTP stream 的消息：忽略
                Poll::Ready(Some(_)) => {}
                Poll::Ready(None) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(std::io::Error::other("the stream was closed"))));
                }
            }
        }
    }
}

type ReqBoxBody = BoxBody<Bytes, std::io::Error>;

/// 把改写过的请求头和路径装成 hyper 的请求。
fn build_request(
    head: &PreviewRequestHead,
    headers: &[(String, String)],
    body: ReqBoxBody,
) -> Result<hyper::Request<ReqBoxBody>, &'static str> {
    let method = http::Method::from_bytes(head.method.as_bytes()).map_err(|_| "invalid method")?;
    let mut b = hyper::Request::builder().method(method).uri(head.path.as_str());
    for (k, v) in headers {
        let name = http::HeaderName::from_bytes(k.as_bytes()).map_err(|_| "invalid header name")?;
        let value = http::HeaderValue::from_bytes(v.as_bytes()).map_err(|_| "invalid header value")?;
        b = b.header(name, value);
    }
    // 每个 stream 一条新连接，响应完了就关
    b = b.header(http::header::CONNECTION, "close");
    b.body(body).map_err(|_| "invalid request")
}

fn response_headers(map: &http::HeaderMap) -> Vec<(String, String)> {
    map.iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

/// 把 `data` 按 [`DATA_CHUNK`] 切开，逐段向窗口要额度再发出去。窗口关了（stream 结束）或隧道没了返回 `false`。
async fn send_data(credit: &Credit, emit: &Emit, data: &[u8]) -> bool {
    for chunk in data.chunks(DATA_CHUNK) {
        if !credit.acquire(chunk.len()).await {
            return false;
        }
        if !emit(UpEvent::Data(chunk.to_vec())) {
            return false;
        }
    }
    true
}

async fn run_http(mut ctx: StreamCtx) {
    let emit = ctx.emit.clone();
    let head_request = ctx.head.method.eq_ignore_ascii_case("HEAD");
    let tcp = match connect_local(ctx.port, ctx.timeouts.connect).await {
        Ok(s) => s,
        Err(_) => return reset(&emit, PreviewResetCode::Refused, "cannot connect to the local port"),
    };
    // 请求体：第一条消息是 END 就是没有请求体（不让 hyper 对 GET 发 chunked）；否则流式转发
    let (body, done_rx): (ReqBoxBody, Option<oneshot::Receiver<()>>) = match ctx.rx.recv().await {
        None => return,
        Some(ToUp::End) => (Empty::<Bytes>::new().map_err(|never| match never {}).boxed(), None),
        Some(first) => {
            let first = match first {
                ToUp::Body(b) => Some(b),
                _ => None,
            };
            let (done_tx, done_rx) = oneshot::channel();
            let body = ReqBody { first, rx: ctx.rx, emit: emit.clone(), done: Some(done_tx), finished: false };
            (BoxBody::new(body), Some(done_rx))
        }
    };
    let headers = rewrite::request_headers(&ctx.head.headers, &ctx.label, ctx.port, false);
    let req = match build_request(&ctx.head, &headers, body) {
        Ok(r) => r,
        Err(m) => return reset(&emit, PreviewResetCode::Protocol, m),
    };
    let (mut sender, conn) = match http1::handshake(TokioIo::new(tcp)).await {
        Ok(x) => x,
        Err(_) => return reset(&emit, PreviewResetCode::Internal, "cannot start the HTTP exchange"),
    };
    let _conn = AbortOnDrop(tokio::spawn(async move {
        let _ = conn.await;
    }));
    let send = sender.send_request(req);
    // 120 秒从请求发完算起：上传一个大文件的时间不算
    let head_deadline = async {
        if let Some(rx) = done_rx {
            let _ = rx.await;
        }
        tokio::time::sleep(ctx.timeouts.head).await;
    };
    tokio::pin!(send, head_deadline);
    let resp = tokio::select! {
        r = &mut send => match r {
            Ok(r) => r,
            Err(_) => return reset(&emit, PreviewResetCode::Internal, "the local server closed the connection"),
        },
        _ = &mut head_deadline => return reset(&emit, PreviewResetCode::Timeout, "the local server did not respond in time"),
    };

    let status = resp.status().as_u16();
    let upstream_headers = response_headers(resp.headers());
    let (out_headers, plan) = rewrite::response_headers(status, &upstream_headers, ctx.port, head_request);
    let head_ok = emit(UpEvent::Head(PreviewResponseHead { status, headers: out_headers, encoding: plan.encoding() }));
    if !head_ok {
        return;
    }
    let mut body = resp.into_body();
    let mut gzip = (plan == BodyPlan::Compress).then(Gzip::new);
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(_) => return reset(&emit, PreviewResetCode::Internal, "the local server closed the connection"),
        };
        let Ok(data) = frame.into_data() else { continue }; // trailers 不转
        let ok = match gzip.as_mut() {
            Some(g) => {
                let out = g.push(&data);
                send_data(&ctx.credit, &emit, &out).await
            }
            None => send_data(&ctx.credit, &emit, &data).await,
        };
        if !ok {
            return;
        }
    }
    if let Some(g) = gzip.take() {
        if !send_data(&ctx.credit, &emit, &g.finish()).await {
            return;
        }
    }
    emit(UpEvent::End);
}

// ---------------------------------------------------------------- WebSocket

fn ws_request(
    head: &PreviewRequestHead,
    port: u16,
    headers: &[(String, String)],
) -> Result<http::Request<()>, &'static str> {
    // tungstenite 要求绝对 URI（`ws://`）；请求行里只写路径和查询串，`Host` 用我们改写过的头
    let mut b = http::Request::builder().method("GET").uri(format!("ws://localhost:{port}{}", head.path));
    for (k, v) in headers {
        let name = http::HeaderName::from_bytes(k.as_bytes()).map_err(|_| "invalid header name")?;
        let value = http::HeaderValue::from_bytes(v.as_bytes()).map_err(|_| "invalid header value")?;
        b = b.header(name, value);
    }
    // tungstenite 要求这几个头都在请求里；key 由 daemon 自己生成
    b = b
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", tungstenite::handshake::client::generate_key());
    b.body(()).map_err(|_| "invalid request")
}

/// 一条 WebSocket 消息拆成 `WS_MESSAGE` 段发出去（每段拿到窗口再发）。
async fn send_segments(credit: &Credit, emit: &Emit, binary: bool, data: &[u8]) -> bool {
    let mut chunks: Vec<&[u8]> = data.chunks(DATA_CHUNK).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (i, c) in chunks.into_iter().enumerate() {
        if !credit.acquire(c.len()).await {
            return false;
        }
        if !emit(UpEvent::WsSegment { binary, fin: i == last, data: c.to_vec() }) {
            return false;
        }
    }
    true
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(tunnel::MAX_WS_MESSAGE))
        .max_frame_size(Some(tunnel::MAX_WS_MESSAGE))
}

async fn run_ws(mut ctx: StreamCtx) {
    let emit = ctx.emit.clone();
    let tcp = match connect_local(ctx.port, ctx.timeouts.connect).await {
        Ok(s) => s,
        Err(_) => return reset(&emit, PreviewResetCode::Refused, "cannot connect to the local port"),
    };
    let headers = rewrite::request_headers(&ctx.head.headers, &ctx.label, ctx.port, true);
    let req = match ws_request(&ctx.head, ctx.port, &headers) {
        Ok(r) => r,
        Err(m) => return reset(&emit, PreviewResetCode::Protocol, m),
    };
    let handshake = tokio_tungstenite::client_async_with_config(req, tcp, Some(ws_config()));
    let mut ws = match tokio::time::timeout(ctx.timeouts.head, handshake).await {
        Err(_) => return reset(&emit, PreviewResetCode::Timeout, "the local server did not respond in time"),
        Ok(Ok((ws, resp))) => {
            // 子协议由上游选，预览页要知道选了哪个；别的响应头对 WebSocket 没有意义
            let headers: Vec<(String, String)> = resp
                .headers()
                .get("sec-websocket-protocol")
                .map(|v| {
                    vec![("sec-websocket-protocol".to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())]
                })
                .unwrap_or_default();
            let head = PreviewResponseHead { status: 101, headers, encoding: PreviewBodyEncoding::Identity };
            if !emit(UpEvent::WsOpen(head)) {
                return;
            }
            ws
        }
        // 上游拒绝升级：回普通的响应
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let status = resp.status().as_u16();
            let headers = rewrite::response_headers_verbatim(&response_headers(resp.headers()), ctx.port);
            if !emit(UpEvent::Head(PreviewResponseHead { status, headers, encoding: PreviewBodyEncoding::Identity })) {
                return;
            }
            if let Some(body) = resp.body() {
                if !send_data(&ctx.credit, &emit, body).await {
                    return;
                }
            }
            emit(UpEvent::End);
            return;
        }
        Ok(Err(tungstenite::Error::Io(_))) => {
            return reset(&emit, PreviewResetCode::Refused, "the local server closed the connection")
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "WebSocket handshake with the local server failed");
            return reset(&emit, PreviewResetCode::Internal, "the WebSocket handshake with the local server failed");
        }
    };

    // 预览页要关之后的期限；没要关就一直没有
    let mut closing: Option<(u16, String, tokio::time::Instant)> = None;
    loop {
        let close_deadline = async {
            match &closing {
                Some((_, _, at)) => tokio::time::sleep_until(*at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if !send_segments(&ctx.credit, &emit, false, t.as_bytes()).await {
                        return;
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if !send_segments(&ctx.credit, &emit, true, &b).await {
                        return;
                    }
                }
                // ping / pong 两边各自应答，不进隧道（tungstenite 自动回 pong）
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame.map_or((1005, String::new()), |f| (u16::from(f.code), f.reason.to_string()));
                    // 回 Close（tungstenite 已经排好了回复，冲出去）
                    let _ = tokio::time::timeout(Duration::from_secs(1), ws.flush()).await;
                    emit(UpEvent::WsClose { code, reason });
                    return;
                }
                Some(Err(tungstenite::Error::Capacity(_))) => {
                    emit(UpEvent::WsClose { code: 1009, reason: "message too big".into() });
                    return;
                }
                Some(Err(_)) | None => {
                    emit(UpEvent::WsClose { code: 1006, reason: String::new() });
                    return;
                }
            },
            cmd = ctx.rx.recv() => match cmd {
                None => return,
                Some(ToUp::WsMessage { binary, data }) => {
                    let n = data.len() as u32;
                    let m = if binary {
                        Message::Binary(Bytes::from(data))
                    } else {
                        match String::from_utf8(data) {
                            Ok(s) => Message::text(s),
                            Err(_) => return reset(&emit, PreviewResetCode::Protocol, "a text message is not valid UTF-8"),
                        }
                    };
                    if ws.send(m).await.is_err() {
                        emit(UpEvent::WsClose { code: 1006, reason: String::new() });
                        return;
                    }
                    emit(UpEvent::Consumed(n));
                }
                Some(ToUp::WsClose { code, reason }) => {
                    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.clone().into() };
                    if ws.send(Message::Close(Some(frame))).await.is_err() {
                        emit(UpEvent::WsClose { code, reason });
                        return;
                    }
                    closing = Some((code, reason, tokio::time::Instant::now() + ctx.timeouts.ws_close));
                }
                Some(_) => {}
            },
            _ = close_deadline => {
                // 上游一直不回 Close：替它回了，免得预览页的 WebSocket 一直悬着
                let (code, reason, _) = closing.take().expect("the deadline only exists while closing");
                emit(UpEvent::WsClose { code, reason });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[tokio::test]
    async fn credit_waits_for_the_window_and_wakes_on_add() {
        let c = Credit::new(100);
        assert!(c.acquire(60).await);
        assert_eq!(c.available(), 40);
        let c2 = c.clone();
        let waiter = tokio::spawn(async move { c2.acquire(50).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "only 40 available");
        assert!(c.add(10));
        assert!(waiter.await.unwrap());
        assert_eq!(c.available(), 0);
    }

    #[tokio::test]
    async fn closing_the_credit_releases_waiters_with_false() {
        let c = Credit::new(0);
        let c2 = c.clone();
        let waiter = tokio::spawn(async move { c2.acquire(1).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.close();
        assert!(!waiter.await.unwrap());
        assert!(!c.acquire(0).await, "closed is closed");
    }

    #[test]
    fn credit_cannot_grow_past_the_flow_control_maximum() {
        let c = Credit::new(256 * 1024);
        assert!(c.add(u32::MAX >> 1 >> 1));
        assert!(!c.add(u32::MAX));
        assert!(c.available() <= CREDIT_MAX);
    }

    #[test]
    fn gzip_chunks_decode_as_one_stream_and_are_flushed_per_push() {
        let mut g = Gzip::new();
        let a = g.push(b"hello ");
        assert!(!a.is_empty(), "sync flush: a chunk is available right away");
        let b = g.push(&vec![b'x'; 10_000]);
        let tail = g.finish();
        let mut all = a;
        all.extend_from_slice(&b);
        all.extend_from_slice(&tail);
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&all[..]).read_to_end(&mut out).unwrap();
        assert_eq!(&out[..6], b"hello ");
        assert_eq!(out.len(), 6 + 10_000);
        assert!(all.len() < 500, "10k of x compresses well: {}", all.len());
        // 空响应体也是合法的 gzip 流
        let mut empty = Vec::new();
        flate2::read::GzDecoder::new(&Gzip::new().finish()[..]).read_to_end(&mut empty).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn requests_are_built_from_the_rewritten_headers() {
        let head =
            PreviewRequestHead { method: "POST".into(), path: "/a/b?c=d".into(), headers: vec![], websocket: false };
        let body = Empty::<Bytes>::new().map_err(|n| match n {}).boxed();
        let h = vec![("host".to_string(), "localhost:80".to_string()), ("x-a".to_string(), "é".to_string())];
        let r = build_request(&head, &h, body).unwrap();
        assert_eq!(r.method(), "POST");
        assert_eq!(r.uri().path_and_query().unwrap().as_str(), "/a/b?c=d");
        assert_eq!(r.headers()["host"], "localhost:80");
        assert_eq!(r.headers()["connection"], "close");
        assert_eq!(r.headers()["x-a"].as_bytes(), "é".as_bytes(), "obs-text bytes pass through");
        // 非法的方法 / 头名 / 路径
        let bad = |method: &str, path: &str, hdr: (&str, &str)| {
            let head =
                PreviewRequestHead { method: method.into(), path: path.into(), headers: vec![], websocket: false };
            let body = Empty::<Bytes>::new().map_err(|n| match n {}).boxed();
            build_request(&head, &[(hdr.0.to_string(), hdr.1.to_string())], body).is_err()
        };
        assert!(bad("GE T", "/", ("a", "b")));
        assert!(bad("GET", "/", ("bad header", "b")));
        assert!(bad("GET", "/", ("a", "line\nbreak")));
        assert!(bad("GET", "/ space", ("a", "b")));
    }

    #[test]
    fn websocket_requests_carry_the_generated_handshake_headers() {
        let head =
            PreviewRequestHead { method: "GET".into(), path: "/ws?token=1".into(), headers: vec![], websocket: true };
        let r = ws_request(
            &head,
            5173,
            &[
                ("host".to_string(), "localhost:5173".to_string()),
                ("sec-websocket-protocol".to_string(), "vite-hmr".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(r.headers()["upgrade"], "websocket");
        assert_eq!(r.headers()["sec-websocket-version"], "13");
        assert!(!r.headers()["sec-websocket-key"].is_empty());
        assert_eq!(r.headers()["sec-websocket-protocol"], "vite-hmr");
        // 每次的 key 不一样
        let r2 = ws_request(&head, 5173, &[("host".to_string(), "localhost:5173".to_string())]).unwrap();
        assert_ne!(r.headers()["sec-websocket-key"], r2.headers()["sec-websocket-key"]);
    }

    #[tokio::test]
    async fn connecting_tries_ipv4_then_ipv6_and_reports_refusal() {
        // IPv4 上有人监听
        let l4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p4 = l4.local_addr().unwrap().port();
        assert!(connect_local(p4, Duration::from_secs(2)).await.is_ok());
        // 只有 IPv6 的（Vite 在 macOS 上的样子）；没有 IPv6 的机器跳过
        if let Ok(l6) = tokio::net::TcpListener::bind("[::1]:0").await {
            let p6 = l6.local_addr().unwrap().port();
            assert!(connect_local(p6, Duration::from_secs(2)).await.is_ok(), "falls back to [::1]");
        }
        // 没人监听
        let free = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(connect_local(free, Duration::from_secs(2)).await.is_err());
    }

    #[tokio::test]
    async fn the_http_task_resets_with_refused_when_nothing_listens() {
        let free = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let emit: Emit = Arc::new(move |e| events_tx.send(e).is_ok());
        let (_tx, rx) = mpsc::unbounded_channel();
        let ctx = StreamCtx {
            port: free,
            label: "l".into(),
            head: PreviewRequestHead { method: "GET".into(), path: "/".into(), headers: vec![], websocket: false },
            rx,
            credit: Credit::new(1000),
            emit,
            timeouts: Timeouts::default(),
        };
        run(ctx).await;
        match events.recv().await {
            Some(UpEvent::Reset { code: PreviewResetCode::Refused, .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(events.try_recv().is_err());
    }
}
