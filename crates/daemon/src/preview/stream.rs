//! 隧道里的 stream 表（docs/protocol.md § 11.4 / § 11.5 的 stream 部分）：纯状态机，不碰 I/O。
//!
//! 入站：[`StreamTable::on_frame`] 吃预览页发来的帧，产出要做的事（[`Action`]）：打开上游、转发请求体、回 `RESET`、归还窗口……
//! 出站：[`StreamTable::on_up`] 对上游任务产出的每个事件做状态迁移，决定要不要发给预览页、这个 stream 是不是因此结束。
//! stream 的 I/O（上游连接、流量窗口的等待）在 `upstream.rs`，把这些动作接到线路上的是 `tunnel.rs`。
//!
//! 规则：
//! - stream id 由预览页分配，非零、严格递增；不递增 → `RESET { protocol }`；同时打开的至多 256 个，超出 `RESET { limit }`；
//! - `RESET` 之后（或正常结束之后）到达的同一 stream 的帧一律忽略，包括 `WINDOW`——id 不会复用，所以「id 不大于见过的最大 id
//!   而又不在表里」就是这种情况；
//! - 预览页发给 daemon 的 `DATA` / `WS_MESSAGE` 的数据字节占 stream 窗口（初始 256 KiB），超出 `RESET { flow_control }`；
//! - 两个方向都 `END` 了（HTTP）、两边都发过 `WS_CLOSE` 了（WebSocket）stream 就结束。

use std::collections::HashMap;

use anycon_protocol::tunnel::{self, Frame, FrameType, TunnelError};
use anycon_protocol::{PreviewRequestHead, PreviewResetCode};

/// stream 的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Http,
    WebSocket,
}

#[derive(Debug)]
struct Stream {
    kind: StreamKind,
    /// 预览页还可以发给我们多少数据字节（我们给出的窗口）。
    recv_window: u32,
    /// HTTP：预览页发过 `END` 了。
    req_ended: bool,
    /// 我们发过响应头了。
    resp_started: bool,
    /// 我们发过 `END` 了。
    resp_ended: bool,
    /// WebSocket：上游接受了升级（我们发过 `RESPONSE { 101 }`）。
    ws_open: bool,
    /// WebSocket：上游拒绝了升级，后面是普通的 `RESPONSE` + `DATA` + `END`。
    ws_rejected: bool,
    ws_close_sent: bool,
    ws_close_recv: bool,
    /// 正在拼的那条 WebSocket 消息。
    ws_buf: Vec<u8>,
    ws_binary: bool,
}

/// 要做的事。
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// 打开一个 stream：起上游任务。
    Open { id: u32, head: PreviewRequestHead },
    /// 请求体的一段，交给上游。
    Body { id: u32, data: Vec<u8> },
    /// 请求体结束。
    BodyEnd { id: u32 },
    /// 一条完整的 WebSocket 消息，交给上游。
    WsMessage { id: u32, binary: bool, data: Vec<u8> },
    /// 预览页要关 WebSocket。
    WsClose { id: u32, code: u16, reason: String },
    /// 预览页给了这个 stream（我们发送方向）更多窗口。
    Credit { id: u32, n: u32 },
    /// WebSocket 消息的数据已经被表收下了，把这么多窗口还给预览页（HTTP 请求体要等上游真的读走了才还）。
    ReturnWindow { id: u32, n: u32 },
    /// 发 `RESET` 给预览页，并丢掉这个 stream 的一切（包括上游任务）。
    Reset { id: u32, code: PreviewResetCode, message: &'static str },
    /// stream 已经从表里去掉（对端 `RESET` 了、或正常结束）：丢掉上游任务。不发任何帧。
    Drop { id: u32 },
}

/// 上游任务产出的事件的种类（出站方向，只带状态迁移要看的信息）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpKind {
    Head { status: u16 },
    Data,
    End,
    WsMessage,
    WsClose,
    Reset,
}

/// [`StreamTable::on_up`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpOut {
    /// 发给预览页。
    Send,
    /// 发给预览页，之后这个 stream 就结束了（已经从表里去掉）。
    SendAndFinish,
    /// 不发（stream 已经没了，或者这个事件在当前状态下不合法）。
    Ignore,
}

#[derive(Debug, Default)]
pub struct StreamTable {
    streams: HashMap<u32, Stream>,
    /// 见过的最大 stream id。
    last_id: u32,
}

/// HTTP 方法是 token（RFC 9110）。
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn reset(id: u32, code: PreviewResetCode, message: &'static str) -> Vec<Action> {
    vec![Action::Reset { id, code, message }]
}

impl StreamTable {
    pub fn new() -> StreamTable {
        StreamTable::default()
    }

    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    pub fn contains(&self, id: u32) -> bool {
        self.streams.contains_key(&id)
    }

    pub fn kind(&self, id: u32) -> Option<StreamKind> {
        self.streams.get(&id).map(|s| s.kind)
    }

    /// 去掉 stream（由调用方在自己发了 `RESET` 之后调用，或整条隧道丢弃时不必调用）。
    pub fn remove(&mut self, id: u32) -> bool {
        self.streams.remove(&id).is_some()
    }

    /// 这个 id 是不是「已经结束的 stream」（见过、但不在表里）。
    fn is_closed(&self, id: u32) -> bool {
        id != 0 && id <= self.last_id && !self.streams.contains_key(&id)
    }

    /// 处理预览页发来的一帧。
    pub fn on_frame(&mut self, f: &Frame) -> Vec<Action> {
        match f.ty {
            FrameType::Request => self.on_request(f.stream, &f.payload),
            FrameType::Data => self.on_data(f.stream, &f.payload),
            FrameType::End => self.on_end(f.stream),
            FrameType::Reset => self.on_reset(f.stream),
            FrameType::Window => self.on_window(f.stream, &f.payload),
            FrameType::WsMessage => self.on_ws_message(f.stream, &f.payload),
            FrameType::WsClose => self.on_ws_close(f.stream, &f.payload),
            // 只有 daemon 发这两种
            FrameType::Response | FrameType::Goaway => Vec::new(),
        }
    }

    /// 终止一个存在的 stream：从表里去掉，返回要发的 `RESET`。
    fn kill(&mut self, id: u32, code: PreviewResetCode, message: &'static str) -> Vec<Action> {
        self.streams.remove(&id);
        reset(id, code, message)
    }

    fn on_request(&mut self, id: u32, payload: &[u8]) -> Vec<Action> {
        if id == 0 {
            // stream 0 只给 GOAWAY 用；没有可以 RESET 的对象
            return Vec::new();
        }
        if id <= self.last_id {
            // 不递增（含重复使用）：已有的同 id stream 一并终止
            return self.kill(id, PreviewResetCode::Protocol, "stream ids must increase");
        }
        self.last_id = id;
        if self.streams.len() >= tunnel::MAX_STREAMS {
            return reset(id, PreviewResetCode::Limit, "too many open streams");
        }
        let head: PreviewRequestHead = match tunnel::decode_head(payload) {
            Ok(h) => h,
            Err(TunnelError::TooLarge) => return reset(id, PreviewResetCode::TooLarge, "request head too large"),
            Err(_) => return reset(id, PreviewResetCode::Protocol, "malformed request head"),
        };
        if !head.path.starts_with('/') || !is_token(&head.method) {
            return reset(id, PreviewResetCode::Protocol, "invalid method or path");
        }
        if head.websocket && !head.method.eq_ignore_ascii_case("GET") {
            return reset(id, PreviewResetCode::Protocol, "a WebSocket upgrade must be a GET");
        }
        let kind = if head.websocket { StreamKind::WebSocket } else { StreamKind::Http };
        self.streams.insert(
            id,
            Stream {
                kind,
                recv_window: tunnel::STREAM_WINDOW,
                req_ended: false,
                resp_started: false,
                resp_ended: false,
                ws_open: false,
                ws_rejected: false,
                ws_close_sent: false,
                ws_close_recv: false,
                ws_buf: Vec::new(),
                ws_binary: false,
            },
        );
        vec![Action::Open { id, head }]
    }

    fn on_data(&mut self, id: u32, payload: &[u8]) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&id) else {
            return if self.is_closed(id) {
                Vec::new()
            } else {
                reset(id, PreviewResetCode::Protocol, "unknown stream")
            };
        };
        if s.kind != StreamKind::Http || s.req_ended {
            return self.kill(id, PreviewResetCode::Protocol, "unexpected DATA");
        }
        if payload.len() > tunnel::MAX_DATA_FRAME {
            return self.kill(id, PreviewResetCode::Protocol, "DATA frame too large");
        }
        if payload.len() as u64 > u64::from(s.recv_window) {
            return self.kill(id, PreviewResetCode::FlowControl, "exceeded the stream window");
        }
        if payload.is_empty() {
            return Vec::new();
        }
        s.recv_window -= payload.len() as u32;
        vec![Action::Body { id, data: payload.to_vec() }]
    }

    /// 一个 `WINDOW` 帧（给预览页 `n` 字节窗口）要发出去了：记账。只在帧真的发出时加，这样表里的窗口永远不会比预览页
    /// 知道的大，守规矩的预览页不会被误判成超窗口。
    pub fn window_granted(&mut self, id: u32, n: u32) {
        if let Some(s) = self.streams.get_mut(&id) {
            s.recv_window = s.recv_window.saturating_add(n).min(tunnel::STREAM_WINDOW);
        }
    }

    fn on_end(&mut self, id: u32) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&id) else {
            return if self.is_closed(id) {
                Vec::new()
            } else {
                reset(id, PreviewResetCode::Protocol, "unknown stream")
            };
        };
        if s.kind != StreamKind::Http || s.req_ended {
            return self.kill(id, PreviewResetCode::Protocol, "unexpected END");
        }
        s.req_ended = true;
        let finished = s.resp_ended;
        let mut acts = vec![Action::BodyEnd { id }];
        if finished {
            self.streams.remove(&id);
            acts.push(Action::Drop { id });
        }
        acts
    }

    fn on_reset(&mut self, id: u32) -> Vec<Action> {
        if self.streams.remove(&id).is_some() {
            vec![Action::Drop { id }]
        } else {
            Vec::new()
        }
    }

    fn on_window(&mut self, id: u32, payload: &[u8]) -> Vec<Action> {
        if !self.streams.contains_key(&id) {
            return Vec::new();
        }
        match tunnel::parse_window(payload) {
            Ok(0) => Vec::new(),
            Ok(n) => vec![Action::Credit { id, n }],
            Err(_) => self.kill(id, PreviewResetCode::Protocol, "malformed WINDOW"),
        }
    }

    fn on_ws_message(&mut self, id: u32, payload: &[u8]) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&id) else {
            return if self.is_closed(id) {
                Vec::new()
            } else {
                reset(id, PreviewResetCode::Protocol, "unknown stream")
            };
        };
        if s.kind != StreamKind::WebSocket || !s.ws_open || s.ws_close_recv {
            return self.kill(id, PreviewResetCode::Protocol, "unexpected WS_MESSAGE");
        }
        let (flags, data) = match tunnel::parse_ws_message(payload) {
            Ok(x) => x,
            Err(_) => return self.kill(id, PreviewResetCode::Protocol, "malformed WS_MESSAGE"),
        };
        if data.len() > tunnel::MAX_DATA_FRAME {
            return self.kill(id, PreviewResetCode::Protocol, "WS_MESSAGE frame too large");
        }
        if data.len() as u64 > u64::from(s.recv_window) {
            return self.kill(id, PreviewResetCode::FlowControl, "exceeded the stream window");
        }
        if s.ws_buf.len() + data.len() > tunnel::MAX_WS_MESSAGE {
            return self.kill(id, PreviewResetCode::TooLarge, "WebSocket message too large");
        }
        s.recv_window -= data.len() as u32;
        // 数据一进表就（让调用方）还窗口：一条消息可以比窗口大（最多 16 MiB），要等它拼完才还的话预览页会卡死
        if s.ws_buf.is_empty() {
            s.ws_binary = flags & tunnel::WS_BINARY != 0;
        }
        s.ws_buf.extend_from_slice(data);
        let mut acts = Vec::new();
        if !data.is_empty() {
            acts.push(Action::ReturnWindow { id, n: data.len() as u32 });
        }
        if flags & tunnel::WS_FIN != 0 {
            let msg = std::mem::take(&mut s.ws_buf);
            acts.push(Action::WsMessage { id, binary: s.ws_binary, data: msg });
        }
        acts
    }

    fn on_ws_close(&mut self, id: u32, payload: &[u8]) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&id) else {
            return if self.is_closed(id) {
                Vec::new()
            } else {
                reset(id, PreviewResetCode::Protocol, "unknown stream")
            };
        };
        if s.kind != StreamKind::WebSocket || s.ws_close_recv {
            return self.kill(id, PreviewResetCode::Protocol, "unexpected WS_CLOSE");
        }
        let (code, reason) = match tunnel::parse_ws_close(payload) {
            Ok(x) => x,
            Err(_) => return self.kill(id, PreviewResetCode::Protocol, "malformed WS_CLOSE"),
        };
        if !s.ws_open {
            // 上游握手还没完成，预览页就不要了：和 `RESET { cancelled }` 一样处理
            self.streams.remove(&id);
            return vec![Action::Drop { id }];
        }
        s.ws_close_recv = true;
        let finished = s.ws_close_sent;
        let mut acts = vec![Action::WsClose { id, code, reason }];
        if finished {
            self.streams.remove(&id);
            acts.push(Action::Drop { id });
        }
        acts
    }

    /// 上游任务产出了一个事件：做状态迁移，告诉调用方要不要发给预览页。
    pub fn on_up(&mut self, id: u32, ev: UpKind) -> UpOut {
        let Some(s) = self.streams.get_mut(&id) else { return UpOut::Ignore };
        match ev {
            UpKind::Head { status } => {
                if s.resp_started || (s.kind == StreamKind::WebSocket && s.ws_open) {
                    return UpOut::Ignore;
                }
                s.resp_started = true;
                if s.kind == StreamKind::WebSocket {
                    if status == 101 {
                        s.ws_open = true;
                    } else {
                        s.ws_rejected = true;
                    }
                }
                UpOut::Send
            }
            UpKind::Data => {
                let body_ok = s.kind == StreamKind::Http || s.ws_rejected;
                if body_ok && s.resp_started && !s.resp_ended {
                    UpOut::Send
                } else {
                    UpOut::Ignore
                }
            }
            UpKind::End => {
                let body_ok = s.kind == StreamKind::Http || s.ws_rejected;
                if !(body_ok && s.resp_started && !s.resp_ended) {
                    return UpOut::Ignore;
                }
                s.resp_ended = true;
                // WebSocket 被拒绝时预览页不会再发 END：到这里两个方向都完了
                if s.ws_rejected || s.req_ended {
                    self.streams.remove(&id);
                    UpOut::SendAndFinish
                } else {
                    UpOut::Send
                }
            }
            UpKind::WsMessage => {
                if s.kind == StreamKind::WebSocket && s.ws_open && !s.ws_close_sent {
                    UpOut::Send
                } else {
                    UpOut::Ignore
                }
            }
            UpKind::WsClose => {
                if !(s.kind == StreamKind::WebSocket && s.ws_open) || s.ws_close_sent {
                    return UpOut::Ignore;
                }
                s.ws_close_sent = true;
                if s.ws_close_recv {
                    self.streams.remove(&id);
                    UpOut::SendAndFinish
                } else {
                    UpOut::Send
                }
            }
            UpKind::Reset => {
                self.streams.remove(&id);
                UpOut::SendAndFinish
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::tunnel::{window, ws_close, ws_message, WS_BINARY, WS_FIN};

    fn head(path: &str, ws: bool) -> Vec<u8> {
        tunnel::encode_head(&PreviewRequestHead {
            method: "GET".into(),
            path: path.into(),
            headers: vec![("accept".into(), "*/*".into())],
            websocket: ws,
        })
        .unwrap()
    }

    fn f(ty: FrameType, stream: u32, payload: Vec<u8>) -> Frame {
        Frame::new(ty, stream, payload)
    }

    fn open_http(t: &mut StreamTable, id: u32) {
        let acts = t.on_frame(&f(FrameType::Request, id, head("/x", false)));
        assert!(matches!(acts.as_slice(), [Action::Open { id: i, .. }] if *i == id), "{acts:?}");
    }

    fn open_ws(t: &mut StreamTable, id: u32) {
        let acts = t.on_frame(&f(FrameType::Request, id, head("/ws", true)));
        assert!(matches!(acts.as_slice(), [Action::Open { id: i, head }] if *i == id && head.websocket), "{acts:?}");
        assert_eq!(t.on_up(id, UpKind::Head { status: 101 }), UpOut::Send);
    }

    fn is_reset(acts: &[Action], id: u32, code: PreviewResetCode) -> bool {
        matches!(acts, [Action::Reset { id: i, code: c, .. }] if *i == id && *c == code)
    }

    #[test]
    fn a_request_opens_a_stream_with_its_head() {
        let mut t = StreamTable::new();
        let acts = t.on_frame(&f(FrameType::Request, 1, head("/a?b=1", false)));
        match acts.as_slice() {
            [Action::Open { id: 1, head }] => {
                assert_eq!((head.method.as_str(), head.path.as_str(), head.websocket), ("GET", "/a?b=1", false));
            }
            other => panic!("{other:?}"),
        }
        assert!(t.contains(1) && t.len() == 1);
        assert_eq!(t.kind(1), Some(StreamKind::Http));
    }

    #[test]
    fn ids_must_be_non_zero_and_strictly_increasing() {
        let mut t = StreamTable::new();
        assert!(t.on_frame(&f(FrameType::Request, 0, head("/", false))).is_empty(), "stream 0 is not a request");
        open_http(&mut t, 3);
        // 重复、倒退：RESET protocol，已有的同 id stream 也被终止
        let acts = t.on_frame(&f(FrameType::Request, 3, head("/", false)));
        assert!(is_reset(&acts, 3, PreviewResetCode::Protocol), "{acts:?}");
        assert!(!t.contains(3));
        let acts = t.on_frame(&f(FrameType::Request, 2, head("/", false)));
        assert!(is_reset(&acts, 2, PreviewResetCode::Protocol));
        // 之后更大的 id 照常
        open_http(&mut t, 4);
        // 跳号是允许的
        open_http(&mut t, 10);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn at_most_256_streams_are_open_at_once() {
        let mut t = StreamTable::new();
        for id in 1..=256u32 {
            open_http(&mut t, id);
        }
        let acts = t.on_frame(&f(FrameType::Request, 257, head("/", false)));
        assert!(is_reset(&acts, 257, PreviewResetCode::Limit));
        assert_eq!(t.len(), 256);
        // 其中一个结束后又能开
        assert_eq!(t.on_frame(&f(FrameType::Reset, 5, vec![])), vec![Action::Drop { id: 5 }]);
        open_http(&mut t, 258);
    }

    #[test]
    fn bad_request_heads_are_reset() {
        let mut t = StreamTable::new();
        let acts = t.on_frame(&f(FrameType::Request, 1, b"not msgpack".to_vec()));
        assert!(is_reset(&acts, 1, PreviewResetCode::Protocol));
        let acts = t.on_frame(&f(FrameType::Request, 2, head("no-slash", false)));
        assert!(is_reset(&acts, 2, PreviewResetCode::Protocol));
        let acts = t.on_frame(&f(FrameType::Request, 3, vec![0; tunnel::MAX_HEAD + 1]));
        assert!(is_reset(&acts, 3, PreviewResetCode::TooLarge));
        let bad_method = tunnel::encode_head(&PreviewRequestHead {
            method: "GE T".into(),
            path: "/".into(),
            headers: vec![],
            websocket: false,
        })
        .unwrap();
        assert!(is_reset(&t.on_frame(&f(FrameType::Request, 4, bad_method)), 4, PreviewResetCode::Protocol));
        let ws_post = tunnel::encode_head(&PreviewRequestHead {
            method: "POST".into(),
            path: "/".into(),
            headers: vec![],
            websocket: true,
        })
        .unwrap();
        assert!(is_reset(&t.on_frame(&f(FrameType::Request, 5, ws_post)), 5, PreviewResetCode::Protocol));
        assert!(t.is_empty());
        // 坏的头也用掉了这个 id：下一个必须更大
        open_http(&mut t, 6);
    }

    #[test]
    fn request_body_flows_to_upstream_within_the_window() {
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        assert_eq!(
            t.on_frame(&f(FrameType::Data, 1, vec![7; 1000])),
            vec![Action::Body { id: 1, data: vec![7; 1000] }]
        );
        assert!(t.on_frame(&f(FrameType::Data, 1, vec![])).is_empty(), "an empty DATA is a no-op");
        assert_eq!(t.on_frame(&f(FrameType::End, 1, vec![])), vec![Action::BodyEnd { id: 1 }]);
        // END 之后不能再有 DATA / END
        assert!(is_reset(&t.on_frame(&f(FrameType::Data, 1, vec![1])), 1, PreviewResetCode::Protocol));
        assert!(!t.contains(1));
    }

    #[test]
    fn data_beyond_the_stream_window_is_a_flow_control_error() {
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        // 256 KiB 的窗口，每帧 64 KiB：四帧正好用完
        for _ in 0..4 {
            let acts = t.on_frame(&f(FrameType::Data, 1, vec![0; tunnel::MAX_DATA_FRAME]));
            assert!(matches!(acts.as_slice(), [Action::Body { .. }]));
        }
        let acts = t.on_frame(&f(FrameType::Data, 1, vec![0; 1]));
        assert!(is_reset(&acts, 1, PreviewResetCode::FlowControl), "{acts:?}");
        assert!(!t.contains(1));
        // 给预览页的 WINDOW 帧发出之后窗口恢复
        open_http(&mut t, 2);
        for _ in 0..4 {
            t.on_frame(&f(FrameType::Data, 2, vec![0; tunnel::MAX_DATA_FRAME]));
        }
        t.window_granted(2, 100_000);
        assert!(matches!(t.on_frame(&f(FrameType::Data, 2, vec![0; 60_000])).as_slice(), [Action::Body { .. }]));
        // 一帧超过 64 KiB 本身就不合法
        open_http(&mut t, 3);
        assert!(is_reset(
            &t.on_frame(&f(FrameType::Data, 3, vec![0; tunnel::MAX_DATA_FRAME + 1])),
            3,
            PreviewResetCode::Protocol
        ));
    }

    #[test]
    fn frames_for_unknown_streams_are_protocol_errors_but_closed_ones_are_ignored() {
        let mut t = StreamTable::new();
        // 从没打开过的 stream
        assert!(is_reset(&t.on_frame(&f(FrameType::Data, 9, vec![1])), 9, PreviewResetCode::Protocol));
        assert!(is_reset(&t.on_frame(&f(FrameType::End, 9, vec![])), 9, PreviewResetCode::Protocol));
        // 打开又被 RESET 之后：DATA / END / WINDOW / WS_* 一律忽略
        open_http(&mut t, 1);
        assert_eq!(t.on_frame(&f(FrameType::Reset, 1, vec![])), vec![Action::Drop { id: 1 }]);
        for ty in [
            FrameType::Data,
            FrameType::End,
            FrameType::Window,
            FrameType::WsMessage,
            FrameType::WsClose,
            FrameType::Reset,
        ] {
            assert!(t.on_frame(&f(ty, 1, window(5))).is_empty(), "{ty:?}");
        }
        // 对我们已经 RESET 过的 stream 同理
        open_http(&mut t, 2);
        let _ = t.on_frame(&f(FrameType::Data, 2, vec![0; tunnel::MAX_DATA_FRAME + 1]));
        assert!(t.on_frame(&f(FrameType::Data, 2, vec![1])).is_empty());
        assert!(t.on_frame(&f(FrameType::Window, 2, window(1))).is_empty());
        // daemon 方向的帧类型从预览页来：忽略
        assert!(t.on_frame(&f(FrameType::Response, 3, vec![])).is_empty());
        assert!(t.on_frame(&f(FrameType::Goaway, 0, vec![])).is_empty());
    }

    #[test]
    fn window_frames_grant_credit_to_the_sending_direction() {
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        assert_eq!(t.on_frame(&f(FrameType::Window, 1, window(4096))), vec![Action::Credit { id: 1, n: 4096 }]);
        assert!(t.on_frame(&f(FrameType::Window, 1, window(0))).is_empty());
        assert!(is_reset(&t.on_frame(&f(FrameType::Window, 1, vec![1, 2])), 1, PreviewResetCode::Protocol));
    }

    #[test]
    fn an_http_stream_ends_when_both_directions_ended_in_either_order() {
        // 请求先结束，响应后结束
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        t.on_frame(&f(FrameType::End, 1, vec![]));
        assert_eq!(t.on_up(1, UpKind::Head { status: 200 }), UpOut::Send);
        assert_eq!(t.on_up(1, UpKind::Data), UpOut::Send);
        assert_eq!(t.on_up(1, UpKind::End), UpOut::SendAndFinish);
        assert!(!t.contains(1));
        // 响应先结束（上游没读完请求体就答了），请求后结束
        open_http(&mut t, 2);
        assert_eq!(t.on_up(2, UpKind::Head { status: 413 }), UpOut::Send);
        assert_eq!(t.on_up(2, UpKind::End), UpOut::Send);
        assert!(t.contains(2), "the request body is still coming");
        let acts = t.on_frame(&f(FrameType::End, 2, vec![]));
        assert_eq!(acts, vec![Action::BodyEnd { id: 2 }, Action::Drop { id: 2 }]);
        assert!(!t.contains(2));
    }

    #[test]
    fn upstream_events_in_the_wrong_order_or_for_gone_streams_are_ignored() {
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        assert_eq!(t.on_up(1, UpKind::Data), UpOut::Ignore, "data before the head");
        assert_eq!(t.on_up(1, UpKind::End), UpOut::Ignore);
        assert_eq!(t.on_up(1, UpKind::WsMessage), UpOut::Ignore);
        assert_eq!(t.on_up(1, UpKind::WsClose), UpOut::Ignore);
        assert_eq!(t.on_up(1, UpKind::Head { status: 200 }), UpOut::Send);
        assert_eq!(t.on_up(1, UpKind::Head { status: 200 }), UpOut::Ignore, "a second head");
        assert_eq!(t.on_up(1, UpKind::End), UpOut::Send);
        assert_eq!(t.on_up(1, UpKind::Data), UpOut::Ignore, "data after the end");
        assert_eq!(t.on_up(99, UpKind::Data), UpOut::Ignore, "unknown stream");
        // 上游失败：RESET，stream 没了；之后上游任务晚到的事件被忽略
        open_http(&mut t, 2);
        assert_eq!(t.on_up(2, UpKind::Reset), UpOut::SendAndFinish);
        assert_eq!(t.on_up(2, UpKind::Data), UpOut::Ignore);
        assert_eq!(t.on_up(2, UpKind::Reset), UpOut::Ignore);
    }

    #[test]
    fn websocket_messages_are_assembled_and_the_window_is_returned_at_once() {
        let mut t = StreamTable::new();
        open_ws(&mut t, 1);
        // 一条分三段的二进制消息，最后一段带 FIN
        let a = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_BINARY, b"abc")));
        assert_eq!(a, vec![Action::ReturnWindow { id: 1, n: 3 }]);
        let b = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_BINARY, b"def")));
        assert_eq!(b, vec![Action::ReturnWindow { id: 1, n: 3 }]);
        let c = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_BINARY | WS_FIN, b"g")));
        assert_eq!(
            c,
            vec![
                Action::ReturnWindow { id: 1, n: 1 },
                Action::WsMessage { id: 1, binary: true, data: b"abcdefg".to_vec() }
            ]
        );
        // 文本消息：类型取第一段的标志
        let d = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_FIN, b"hello")));
        assert_eq!(d[1], Action::WsMessage { id: 1, binary: false, data: b"hello".to_vec() });
        // 空消息也是消息
        let e = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_FIN, b"")));
        assert_eq!(e, vec![Action::WsMessage { id: 1, binary: false, data: vec![] }]);
    }

    #[test]
    fn a_websocket_message_has_a_size_limit_and_segments_respect_the_window() {
        let seg = vec![0u8; tunnel::MAX_DATA_FRAME];
        let mut t = StreamTable::new();
        open_ws(&mut t, 1);
        // 窗口 256 KiB：四段用完，没有还窗口时再来一个字节就是流控错误
        for _ in 0..4 {
            let acts = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_BINARY, &seg)));
            assert!(matches!(acts.as_slice(), [Action::ReturnWindow { .. }]), "{acts:?}");
        }
        let acts = t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_BINARY, b"x")));
        assert!(is_reset(&acts, 1, PreviewResetCode::FlowControl), "{acts:?}");
        // 一段本身超过 64 KiB 不合法
        open_ws(&mut t, 2);
        let acts = t.on_frame(&f(FrameType::WsMessage, 2, ws_message(0, &vec![0; tunnel::MAX_DATA_FRAME + 1])));
        assert!(is_reset(&acts, 2, PreviewResetCode::Protocol));
        // 16 MiB 的消息：窗口随收随还（调用方发出 WINDOW 帧时记账），所以可以一段段拼；超过 16 MiB 就 too_large
        open_ws(&mut t, 3);
        let n = tunnel::MAX_WS_MESSAGE / seg.len();
        for _ in 0..n {
            let acts = t.on_frame(&f(FrameType::WsMessage, 3, ws_message(WS_BINARY, &seg)));
            assert_eq!(acts.len(), 1, "{acts:?}");
            t.window_granted(3, seg.len() as u32);
        }
        let acts = t.on_frame(&f(FrameType::WsMessage, 3, ws_message(WS_BINARY, b"x")));
        assert!(is_reset(&acts, 3, PreviewResetCode::TooLarge));
    }

    #[test]
    fn websocket_frames_on_the_wrong_kind_of_stream_are_protocol_errors() {
        let mut t = StreamTable::new();
        open_http(&mut t, 1);
        assert!(is_reset(
            &t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_FIN, b"x"))),
            1,
            PreviewResetCode::Protocol
        ));
        open_http(&mut t, 2);
        assert!(is_reset(&t.on_frame(&f(FrameType::WsClose, 2, ws_close(1000, ""))), 2, PreviewResetCode::Protocol));
        // WebSocket 流上不能有 DATA / END
        open_ws(&mut t, 3);
        assert!(is_reset(&t.on_frame(&f(FrameType::Data, 3, vec![1])), 3, PreviewResetCode::Protocol));
        open_ws(&mut t, 4);
        assert!(is_reset(&t.on_frame(&f(FrameType::End, 4, vec![])), 4, PreviewResetCode::Protocol));
        // 升级还没成功就发数据
        let acts = t.on_frame(&f(FrameType::Request, 5, head("/ws", true)));
        assert!(matches!(acts.as_slice(), [Action::Open { .. }]));
        assert!(is_reset(
            &t.on_frame(&f(FrameType::WsMessage, 5, ws_message(WS_FIN, b"x"))),
            5,
            PreviewResetCode::Protocol
        ));
        // 格式不对
        open_ws(&mut t, 6);
        assert!(is_reset(&t.on_frame(&f(FrameType::WsMessage, 6, vec![])), 6, PreviewResetCode::Protocol));
        open_ws(&mut t, 7);
        assert!(is_reset(&t.on_frame(&f(FrameType::WsClose, 7, vec![1])), 7, PreviewResetCode::Protocol));
    }

    #[test]
    fn a_websocket_closes_after_both_sides_sent_close() {
        // 预览页先关
        let mut t = StreamTable::new();
        open_ws(&mut t, 1);
        assert_eq!(
            t.on_frame(&f(FrameType::WsClose, 1, ws_close(1000, "bye"))),
            vec![Action::WsClose { id: 1, code: 1000, reason: "bye".into() }]
        );
        assert!(t.contains(1));
        // 预览页关了之后不能再发消息
        assert!(is_reset(
            &t.on_frame(&f(FrameType::WsMessage, 1, ws_message(WS_FIN, b"x"))),
            1,
            PreviewResetCode::Protocol
        ));
        open_ws(&mut t, 2);
        let _ = t.on_frame(&f(FrameType::WsClose, 2, ws_close(1000, "")));
        assert_eq!(t.on_up(2, UpKind::WsMessage), UpOut::Send, "the upstream may still have messages in flight");
        assert_eq!(t.on_up(2, UpKind::WsClose), UpOut::SendAndFinish);
        assert!(!t.contains(2));

        // 上游先关
        open_ws(&mut t, 3);
        assert_eq!(t.on_up(3, UpKind::WsClose), UpOut::Send);
        assert_eq!(t.on_up(3, UpKind::WsMessage), UpOut::Ignore, "nothing after our close");
        assert_eq!(t.on_up(3, UpKind::WsClose), UpOut::Ignore);
        let acts = t.on_frame(&f(FrameType::WsClose, 3, ws_close(1000, "")));
        assert_eq!(acts, vec![Action::WsClose { id: 3, code: 1000, reason: String::new() }, Action::Drop { id: 3 }]);
        assert!(!t.contains(3));
        // 第二个 WS_CLOSE 是协议错误
        open_ws(&mut t, 4);
        let _ = t.on_frame(&f(FrameType::WsClose, 4, ws_close(1000, "")));
        assert!(is_reset(&t.on_frame(&f(FrameType::WsClose, 4, ws_close(1000, ""))), 4, PreviewResetCode::Protocol));
    }

    #[test]
    fn closing_before_the_upgrade_completes_just_cancels() {
        let mut t = StreamTable::new();
        let _ = t.on_frame(&f(FrameType::Request, 1, head("/ws", true)));
        assert_eq!(t.on_frame(&f(FrameType::WsClose, 1, ws_close(1001, ""))), vec![Action::Drop { id: 1 }]);
        assert!(!t.contains(1));
    }

    #[test]
    fn a_rejected_upgrade_is_an_ordinary_response() {
        let mut t = StreamTable::new();
        let _ = t.on_frame(&f(FrameType::Request, 1, head("/ws", true)));
        assert_eq!(t.on_up(1, UpKind::Head { status: 403 }), UpOut::Send);
        assert_eq!(t.on_up(1, UpKind::WsMessage), UpOut::Ignore);
        assert_eq!(t.on_up(1, UpKind::Data), UpOut::Send);
        // 预览页不会为 WebSocket 流发 END：响应结束 stream 就结束
        assert_eq!(t.on_up(1, UpKind::End), UpOut::SendAndFinish);
        assert!(!t.contains(1));
    }
}
