//! 账号内接入的 initiator（新设备一侧）状态机（ADR-36，docs/protocol.md § 4.4）。
//!
//! 放在 daemon crate 里，CLI 的 `anycon connect --node` 和测试（进程内和 responder 对跑）共用一份实现。
//! **sans-IO**：不碰网络、不读终端，只管「收到一帧 [`EnrollFrame`] → 该发哪些帧 / 出了什么结果」。
//! 控制面（`enroll_begin` / `enroll_started` / `pair_confirm` / `pair_done` / `pair.ack` 的校验与落盘）由调用方负责。
//!
//! 用法：
//! 1. [`Initiator::new`]；`password` 方法先 [`set_password`](Initiator::set_password)，`code` 且不请求批准的先
//!    [`set_code`](Initiator::set_code)；`scan` 用 [`scan_secret`](Initiator::scan_secret) 取出要放进二维码的密钥；
//! 2. [`hello`](Initiator::hello) 得到第一帧，发给 daemon；
//! 3. daemon 的每一帧交给 [`handle`](Initiator::handle)，按返回的 [`Event`] 行事：`Send` 就把帧发出去，`WaitApproval` 等批准，
//!    `NeedCode` 向用户要码再 `set_code`，`Rejected` 给用户看原因（`code` 还有次数时可以 `set_code` + `hello` 重试），
//!    `Finished` 说明 Noise 握手已经走完（第 3 条在 `frames` 里，要发给 daemon）；
//! 4. `Finished` 之后调用方发 `pair_confirm`、等 `pair_done` 和 `pair.ack`——用 [`Finished::transport`] 解 `pair.ack`，
//!    用 [`Finished::node_pub`] 核对 `pair.ack` 里的指纹，**通过才落盘**。
//!
//! 口令的 Argon2id（约 0.1–0.3 s，64 MiB）在 `handle` 里同步完成；异步调用方把 `handle` 放进 `spawn_blocking`。
//!
//! 二维码内容 `anycon-enroll:1:<node_id>:<enroll_id>:<base32(r)>`：base32 是 RFC 4648 的小写字母表
//! `abcdefghijklmnopqrstuvwxyz234567`，无填充（和 Web 一致），见 [`base32_encode`] / [`scan_qr_payload`]。

use anycon_crypto::{
    derive_enroll_password_prs, derive_enroll_scan_psk, enroll_channel_id, enroll_sid, generate_enroll_nonce,
    generate_enroll_secret, normalize_enroll_code, Cpace, Handshake, Keypair, Role, Transport, ENROLL_NONCE_LEN,
    ENROLL_SECRET_LEN,
};
use anycon_protocol::{
    ClientInfo, EnrollFrame, EnrollHint, EnrollMethod, EnrollRejectReason, EnrollReplyStatus, ENROLL_FRAME_VERSION,
};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum InitiatorError {
    /// 对端的帧不合协议（顺序不对、字段缺失 / 长度不对…）。
    #[error("enroll protocol error: {0}")]
    Protocol(String),
    /// 接入码的格式不对（要 6 位数字）。
    #[error("the join code must be 6 digits")]
    InvalidCode,
    /// 这一步需要口令 / 接入码，调用方还没给。
    #[error("the {0} has not been provided")]
    MissingSecret(&'static str),
    #[error("crypto: {0}")]
    Crypto(String),
}

type Result<T> = std::result::Result<T, InitiatorError>;

fn proto(s: impl Into<String>) -> InitiatorError {
    InitiatorError::Protocol(s.into())
}

fn crypto(e: impl std::fmt::Display) -> InitiatorError {
    InitiatorError::Crypto(e.to_string())
}

pub struct InitiatorConfig {
    /// daemon 的正式 device id（来自账号的设备列表）。CPace 的信道标识和扫码 PSK 都绑定它：hub 撒谎就握不上手。
    pub node_id: String,
    pub method: EnrollMethod,
    /// 仅 `code`：`true` = 请求已配对设备批准（码要等批准后才知道），`false` = 用已有的接入码。
    pub approval: bool,
    /// 自报的提示信息，未经认证，只用于批准界面。
    pub hint: EnrollHint,
    /// 握手第 3 条里带给 daemon 的、认证过的设备信息。
    pub client_info: ClientInfo,
    /// 本设备的 Noise 静态密钥。
    pub keypair: Keypair,
}

/// 握手走完时给调用方的东西。
pub struct Finished {
    /// 要发给 daemon 的帧（第 3 条 Noise 消息）。
    pub frames: Vec<EnrollFrame>,
    /// daemon 的静态公钥（握手得到的）：`pair.ack` 里的指纹要和它对上。
    pub node_pub: [u8; 32],
    /// 配对用的传输：只用来解 `pair.ack`（daemon 在 `pair_done` 之后经它发的第一条消息）。
    pub transport: Transport,
    /// 握手第 2 条的载荷（`{"name","version"}` JSON），**未经认证**，只能当提示。
    pub node_info: Vec<u8>,
}

pub enum Event {
    /// 把这些帧按顺序发给 daemon。
    Send(Vec<EnrollFrame>),
    /// daemon 在等已配对设备批准（`scan` / 请求批准的 `code`）。
    WaitApproval,
    /// `ready` 来了，但接入码还没给：向用户要，再 [`Initiator::set_code`]。
    NeedCode,
    /// daemon 拒绝。`code` 且 `attempts_left` > 0 时可以 [`Initiator::set_code`] + [`Initiator::hello`] 重试。
    Rejected { reason: EnrollRejectReason, retry_after_ms: Option<u64>, attempts_left: Option<u32> },
    /// Noise 握手走完（见 [`Finished`]）。注意：第 3 条在 daemon 那里校验失败时（码 / 口令不对）之后还会收到 `Rejected { wrong_secret }`。
    Finished(Box<Finished>),
}

enum State {
    /// 还没发 hello，或上一轮被拒绝、等重试。
    Idle,
    /// 已发 hello，等 daemon 的 `reply`。
    AwaitReply,
    /// 已发第 1 条，等第 2 条。
    AwaitMsg2(Box<Handshake>),
    /// 第 3 条已写出，握手结束。
    Done,
}

struct PendingReady {
    d_nonce: [u8; ENROLL_NONCE_LEN],
    cpace: [u8; 32],
}

pub struct Initiator {
    cfg: InitiatorConfig,
    secret: Option<Zeroizing<[u8; ENROLL_SECRET_LEN]>>,
    password: Option<Zeroizing<String>>,
    code: Option<Zeroizing<String>>,
    nonce: [u8; ENROLL_NONCE_LEN],
    state: State,
    pending: Option<PendingReady>,
}

impl Initiator {
    pub fn new(cfg: InitiatorConfig) -> Initiator {
        let secret = (cfg.method == EnrollMethod::Scan).then(|| Zeroizing::new(generate_enroll_secret()));
        Initiator {
            cfg,
            secret,
            password: None,
            code: None,
            nonce: generate_enroll_nonce(),
            state: State::Idle,
            pending: None,
        }
    }

    pub fn method(&self) -> EnrollMethod {
        self.cfg.method
    }

    /// `scan`：要放进二维码的 32 字节密钥 r（构造时生成）。
    pub fn scan_secret(&self) -> Option<&[u8; ENROLL_SECRET_LEN]> {
        self.secret.as_deref()
    }

    pub fn set_password(&mut self, password: &str) {
        self.password = Some(Zeroizing::new(password.to_string()));
    }

    /// 设置接入码（规范化：去掉空白和 `-`，必须是 6 位数字）。如果 `ready` 已经到了、正等着码，立刻算出该发的帧。
    pub fn set_code(&mut self, code: &str) -> Result<Option<Event>> {
        let code = normalize_enroll_code(code).map_err(|_| InitiatorError::InvalidCode)?;
        self.code = Some(Zeroizing::new(code));
        match self.pending.take() {
            Some(p) => self.start_handshake(p.d_nonce, None, Some(p.cpace)).map(Some),
            None => Ok(None),
        }
    }

    /// 一轮新的 `hello`（新 nonce）。第一次发，或 `code` 输错后重试。
    pub fn hello(&mut self) -> EnrollFrame {
        self.nonce = generate_enroll_nonce();
        self.state = State::AwaitReply;
        self.pending = None;
        EnrollFrame::Hello {
            v: ENROLL_FRAME_VERSION,
            method: self.cfg.method,
            approval: self.cfg.method == EnrollMethod::Code && self.cfg.approval,
            nonce: hex::encode(self.nonce),
            hint: self.cfg.hint.clone(),
        }
    }

    /// 处理 daemon 的一帧。
    pub fn handle(&mut self, frame: EnrollFrame) -> Result<Event> {
        match frame {
            EnrollFrame::Reply { status: EnrollReplyStatus::WaitApproval, .. } => {
                if !matches!(self.state, State::AwaitReply) {
                    return Err(proto("unexpected wait_approval"));
                }
                Ok(Event::WaitApproval)
            }
            EnrollFrame::Reply {
                status: EnrollReplyStatus::Rejected, reason, retry_after_ms, attempts_left, ..
            } => {
                self.state = State::Idle;
                self.pending = None;
                // 没带原因的拒绝当作格式错
                Ok(Event::Rejected {
                    reason: reason.unwrap_or(EnrollRejectReason::Invalid),
                    retry_after_ms,
                    attempts_left,
                })
            }
            EnrollFrame::Reply { status: EnrollReplyStatus::Ready, nonce, salt, cpace, .. } => {
                if !matches!(self.state, State::AwaitReply) {
                    return Err(proto("unexpected ready"));
                }
                let d_nonce = nonce
                    .as_deref()
                    .and_then(decode_hex::<ENROLL_NONCE_LEN>)
                    .ok_or_else(|| proto("ready without a 16-byte nonce"))?;
                let d_share = match self.cfg.method {
                    EnrollMethod::Scan => None,
                    _ => Some(
                        cpace
                            .as_deref()
                            .and_then(decode_hex::<32>)
                            .ok_or_else(|| proto("ready without a CPace share"))?,
                    ),
                };
                if self.cfg.method == EnrollMethod::Code && self.code.is_none() {
                    // 批准之后才知道码：先记下 daemon 的份额，等 set_code
                    self.pending = Some(PendingReady { d_nonce, cpace: d_share.expect("code has a share") });
                    return Ok(Event::NeedCode);
                }
                let salt = match self.cfg.method {
                    EnrollMethod::Password => Some(
                        salt.as_deref()
                            .and_then(decode_hex::<16>)
                            .ok_or_else(|| proto("ready without the password salt"))?,
                    ),
                    _ => None,
                };
                self.start_handshake(d_nonce, salt, d_share)
            }
            EnrollFrame::Noise { msg } => {
                let State::AwaitMsg2(mut hs) = std::mem::replace(&mut self.state, State::Idle) else {
                    return Err(proto("unexpected noise message"));
                };
                let m2 = hex::decode(msg).map_err(|_| proto("noise message is not hex"))?;
                let node_info = hs.read(&m2).map_err(|e| proto(format!("handshake failed: {e}")))?;
                let info = serde_json::to_vec(&self.cfg.client_info).map_err(crypto)?;
                let m3 = hs.write(&info).map_err(crypto)?;
                if !hs.is_finished() {
                    return Err(proto("the handshake did not finish after the third message"));
                }
                let node_pub = hs.remote_public().ok_or_else(|| proto("handshake finished without a node key"))?;
                let transport = hs.into_transport().map_err(crypto)?;
                self.state = State::Done;
                Ok(Event::Finished(Box::new(Finished {
                    frames: vec![EnrollFrame::Noise { msg: hex::encode(m3) }],
                    node_pub,
                    transport,
                    node_info,
                })))
            }
            EnrollFrame::Hello { .. } | EnrollFrame::Cpace { .. } => Err(proto("unexpected frame from the node")),
        }
    }

    /// `ready` 收到、凭据齐了：算 PSK（`code` / `password` 先跑 CPace），写第 1 条 Noise 消息。
    fn start_handshake(
        &mut self,
        d_nonce: [u8; ENROLL_NONCE_LEN],
        salt: Option<[u8; 16]>,
        d_share: Option<[u8; 32]>,
    ) -> Result<Event> {
        let mut frames = Vec::new();
        let psk = match self.cfg.method {
            EnrollMethod::Scan => {
                let secret = self.secret.as_ref().ok_or(InitiatorError::MissingSecret("scan secret"))?;
                Zeroizing::new(derive_enroll_scan_psk(secret, &self.cfg.node_id))
            }
            EnrollMethod::Code | EnrollMethod::Password => {
                let (prs, method): (Zeroizing<Vec<u8>>, &str) = if self.cfg.method == EnrollMethod::Code {
                    let code = self.code.as_ref().ok_or(InitiatorError::MissingSecret("join code"))?;
                    (Zeroizing::new(code.as_bytes().to_vec()), "code")
                } else {
                    let pw = self.password.as_ref().ok_or(InitiatorError::MissingSecret("node password"))?;
                    let salt = salt.ok_or_else(|| proto("missing the password salt"))?;
                    (Zeroizing::new(derive_enroll_password_prs(pw, &salt).map_err(crypto)?.to_vec()), "password")
                };
                let ci = enroll_channel_id(&self.cfg.node_id, method);
                let sid = enroll_sid(&self.nonce, &d_nonce);
                let cpace = Cpace::new(&prs, &ci, &sid).map_err(crypto)?;
                frames.push(EnrollFrame::Cpace { share: hex::encode(cpace.share()) });
                let d_share = d_share.ok_or_else(|| proto("missing the node's CPace share"))?;
                Zeroizing::new(
                    cpace.finish(Role::Initiator, &d_share).map_err(|_| proto("the node's CPace share is invalid"))?,
                )
            }
        };
        let mut hs = Handshake::pairing(Role::Initiator, &self.cfg.keypair, &psk).map_err(crypto)?;
        let m1 = hs.write(&[]).map_err(crypto)?;
        frames.push(EnrollFrame::Noise { msg: hex::encode(m1) });
        self.state = State::AwaitMsg2(Box::new(hs));
        Ok(Event::Send(frames))
    }
}

fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    <[u8; N]>::try_from(hex::decode(s).ok()?.as_slice()).ok()
}

// ---------------------------------------------------------------- 二维码内容与 base32

/// RFC 4648 的 base32 小写字母表，无填充（和 Web 的 `base32` 一致）。
pub const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
        buf &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// [`base32_encode`] 的逆：大小写不敏感，不接受填充 `=`；非法字符 / 末尾多余的非零位返回 `None`。
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = BASE32_ALPHABET.iter().position(|&a| a == c.to_ascii_lowercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    // 规范编码末尾剩下的位不足一个字符（< 5 位）且都是 0
    (bits < 5 && buf == 0).then_some(out)
}

/// `scan` 的二维码内容：`anycon-enroll:1:<node_id>:<enroll_id>:<base32(r)>`。不是 URL，只有 Web UI 内的扫码能读。
pub fn scan_qr_payload(node_id: &str, enroll_id: &str, secret: &[u8; ENROLL_SECRET_LEN]) -> String {
    format!("anycon-enroll:1:{node_id}:{enroll_id}:{}", base32_encode(secret))
}

/// [`scan_qr_payload`] 的逆：`(node_id, enroll_id, r)`。前缀不对、字段数不对、r 不是 32 字节都返回 `None`。
pub fn parse_scan_qr(s: &str) -> Option<(String, String, [u8; ENROLL_SECRET_LEN])> {
    let rest = s.trim().strip_prefix("anycon-enroll:1:")?;
    let mut parts = rest.split(':');
    let (node, id, r) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || node.is_empty() || id.is_empty() {
        return None;
    }
    let r = <[u8; ENROLL_SECRET_LEN]>::try_from(base32_decode(r)?.as_slice()).ok()?;
    Some((node.to_string(), id.to_string(), r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc4648_vectors_in_lowercase_without_padding() {
        // RFC 4648 § 10 的测试向量
        for (plain, enc) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32_encode(plain.as_bytes()), enc, "{plain:?}");
            assert_eq!(base32_decode(enc).unwrap(), plain.as_bytes(), "{enc:?}");
            assert_eq!(base32_decode(&enc.to_ascii_uppercase()).unwrap(), plain.as_bytes());
        }
        // 32 字节密钥编码成 52 个字符
        let r: Vec<u8> = (0..32).collect();
        assert_eq!(base32_encode(&r).len(), 52);
        assert_eq!(base32_decode(&base32_encode(&r)).unwrap(), r);
        // 非法输入
        assert!(base32_decode("mzxw6ytb=").is_none(), "no padding is accepted");
        assert!(base32_decode("mzxw1").is_none(), "1 is not in the alphabet");
        assert!(base32_decode("my").is_some() && base32_decode("mz").is_none(), "trailing bits must be zero");
        assert!(base32_decode("a").is_none(), "a lone character is not a canonical encoding");
    }

    #[test]
    fn the_qr_payload_round_trips_and_rejects_foreign_content() {
        let r: [u8; 32] = std::array::from_fn(|i| (i * 7) as u8);
        let q = scan_qr_payload("nodeid", "0123456789abcdef0123456789abcdef", &r);
        assert!(q.starts_with("anycon-enroll:1:nodeid:0123456789abcdef0123456789abcdef:"), "{q}");
        assert_eq!(parse_scan_qr(&q), Some(("nodeid".into(), "0123456789abcdef0123456789abcdef".into(), r)));
        for bad in [
            "https://hub.example.com/pair#code=ABCD",
            "anycon-enroll:2:n:e:aaaa",
            "anycon-enroll:1:n:e",
            "anycon-enroll:1:n:e:aaaa",
            &format!("{q}:extra"),
            "anycon-enroll:1::e:aaaa",
            "",
        ] {
            assert!(parse_scan_qr(bad).is_none(), "{bad:?}");
        }
    }
}
