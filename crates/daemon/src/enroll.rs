//! 账号内接入引擎（ADR-36，docs/spec/daemon.md § 15.1，docs/protocol.md § 4.4）。
//!
//! daemon 是接入的 responder，也是**唯一的验证点**：hub 只确认发起方和本节点属于同一账号，它的限速只防滥用，
//! 所以在线猜测的次数由这里定（口令连续失败的退避与锁定、接入码每个 3 次、同时最多 3 个会话和 1 个口令尝试）。
//!
//! 职责：
//! - hub 的 `enroll_request` 建内存里的接入会话；`handshake` 信封的 body 是 [`EnrollFrame`]，按会话处理
//!   `hello` / `cpace` / `noise`（三种方法 `scan` / `code` / `password`，握手的后半段就是配对的 `Noise_XXpsk3`）；
//! - 握手成功后复用配对的收尾：暂存设备（[`Peers`] 里的 `staged` 规则）→ `pair_confirm` → `pair_done` → `pair.ack`；
//! - 节点口令（`enroll_password` 表，只存盐和 Argon2id 输出）、主动接入码、批准 / 拒绝、通知 / 安全事件 / 审计。
//!
//! 约定：
//! - **悲观计数**：口令尝试在发出自己的 CPace 份额**之前**先把连续失败次数 +1 写库，第 3 条消息校验通过后清零，
//!   所以错口令、超时、中途断开、`enroll_cancel`、崩溃都算一次失败；
//! - **接入码的次数在收到新设备的 CPace 份额时扣**（每个份额最多换来一次第 3 条的校验）：批准本身、只发 `hello`、
//!   发了 `ready` 之后用户慢慢找码都不扣；`ready` 之后等份额的阶段不计时（码自己有 5 分钟有效期和 3 次上限），
//!   份额之后到第 3 条限 [`ATTEMPT_TIMEOUT_MS`]；
//! - 接入码、扫码密钥、口令只在内存里，不进日志、审计和错误信息；
//! - 时间一律走 [`Clock`]（测试可以拨快），后台 [`Enroll::sweep`] 每秒收一次过期的会话 / 尝试 / 接入码。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anycon_crypto::{
    derive_enroll_password_prs, derive_enroll_scan_psk, enroll_channel_id, enroll_sid, generate_enroll_code,
    generate_enroll_nonce, generate_enroll_salt, validate_enroll_password, Cpace, Handshake, Role, Transport,
    ENROLL_NONCE_LEN, ENROLL_SECRET_LEN,
};
use anycon_protocol::{
    methods, Control, DeviceKind, EnrollFailReason, EnrollFrame, EnrollHint, EnrollInfo, EnrollMethod,
    EnrollPasswordState, EnrollPasswordStatus, EnrollRejectReason, EnrollReplyStatus, EnrollResult,
    EnrollSessionStatus, ErrorCode, Kind, NodeEnrollApproveParams, NodeEnrollApproveResult, NodeEnrollCodeResult,
    NodeEnrollListResult, NodeEnrollResolved, NodeEnrollStatusResult, NodeSecurityEvent, NodeSetEnrollEnabledParams,
    RpcError, SecurityEventKind, ENROLL_FRAME_VERSION,
};
use serde::Serialize;
use zeroize::{Zeroize, Zeroizing};

use crate::audit::Audit;
use crate::clients::{Caller, Clients};
use crate::config::{ConfigStore, EnrollConfig, Paths};
use crate::hub::HubOutbox;
use crate::identity::Identity;
use crate::notify::Notifier;
use crate::peers::{ConfirmOwner, Peers};
use crate::store::{EnrollPasswordRow, PeerRow, Store};
use crate::util::now_ms;

// ---------------------------------------------------------------- 常量与策略

/// 接入会话的有效期（hub 那边也是 5 分钟）。
pub const SESSION_TTL_MS: u64 = 5 * 60 * 1000;
/// 每台 daemon 同时最多几个进行中的接入；多出来的 `hello` 得到 `rejected { busy }`。
pub const MAX_SESSIONS: usize = 3;
/// 超出上限的会话只留一个「桩」，好在 `hello` 到来时回 `busy`；桩也有上限，再多就直接 `enroll_cancel`。
const MAX_STUBS: usize = 8;
/// 接入码的有效期。
pub const CODE_TTL_MS: u64 = 5 * 60 * 1000;
/// 每个接入码合计最多尝试几次（收到新设备的 CPace 份额时扣一次）。
pub const CODE_ATTEMPTS: u32 = 3;
/// 握手成功后等 `pair_done` 最多多久（和配对的看门狗一样 70 秒）；超时就删掉暂存的设备。
pub const CONFIRM_TIMEOUT_MS: u64 = 70 * 1000;
/// 一轮尝试最多多久；超时就收掉，口令尝试的名额不会被一个不再回话的对端占着。口令 / 扫码从 daemon 发出 `ready` 起算；
/// 接入码从收到新设备的 CPace 份额起算（发了 `ready`、等份额的阶段不计时：批准之后用户可以慢慢输码）。
pub const ATTEMPT_TIMEOUT_MS: u64 = 60 * 1000;
/// 口令连续失败满这么多次后开始退避。
pub const BACKOFF_FROM: u32 = 3;
/// 口令连续失败满这么多次后锁定口令接入。
pub const LOCK_AT: u32 = 10;
/// 口令至多多长（字节）。Argon2id 的开销和长度无关，这里只是不收离谱的输入。
pub const PASSWORD_MAX_BYTES: usize = 1024;
/// `hint` 里名称按字符截到这么长。
pub const HINT_MAX_CHARS: usize = 64;
/// 不太重要的审计行（`enroll.requested`、预检拒绝）每分钟最多写这么多，hub 刷请求时不至于把盘写满。
const LOW_VALUE_AUDIT_PER_MIN: u32 = 30;

/// 口令连续失败 `failures` 次之后要等多久才接受下一次尝试：n < 3 不等，n >= 3 是 2^(n−3) 分钟。
pub fn password_backoff_ms(failures: u32) -> u64 {
    if failures < BACKOFF_FROM {
        0
    } else {
        60_000u64.saturating_mul(1u64 << (failures - BACKOFF_FROM).min(40))
    }
}

/// 可以挂时钟：测试里拨快。生产就是 [`now_ms`]。
#[derive(Default)]
pub struct Clock {
    skew_ms: AtomicU64,
}

impl Clock {
    pub fn now(&self) -> u64 {
        now_ms().saturating_add(self.skew_ms.load(Ordering::Relaxed))
    }

    /// 测试用：把时钟拨快 `ms` 毫秒。
    #[cfg(any(test, feature = "testing"))]
    pub fn advance(&self, ms: u64) {
        self.skew_ms.fetch_add(ms, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- 文本清理

fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
        )
}

/// 清掉控制字符 / 双向控制符 / 零宽字符，去掉首尾空白，按字符截断。
pub fn clean_text(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().filter(|c| !is_unsafe_char(*c)).collect();
    t.trim().chars().take(max_chars).collect::<String>().trim().to_string()
}

fn clean_hint(h: &EnrollHint) -> EnrollHint {
    let name = clean_text(&h.name, HINT_MAX_CHARS);
    let platform = clean_text(&h.platform, HINT_MAX_CHARS);
    EnrollHint {
        name: if name.is_empty() { "unknown device".into() } else { name },
        kind: h.kind,
        platform: if platform.is_empty() { "unknown".into() } else { platform },
    }
}

fn tag<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    <[u8; N]>::try_from(hex::decode(s).ok()?.as_slice()).ok()
}

// ---------------------------------------------------------------- 状态

/// 接入码：批准一个请求时生成（只对那个会话有效），或「添加设备」主动生成（任一不请求批准的 `code` 接入可用）。
struct CodeState {
    /// 主动生成的码的编号，用来在握手成功时确认「还是同一个码、没被顶掉 / 用掉」。
    id: u64,
    code: Zeroizing<String>,
    expires_ms: u64,
    attempts_left: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodeRef {
    /// 批准请求时生成、挂在会话上的码。
    Session,
    /// `node.enroll_code` 主动生成的码（编号）。
    Active(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    Scan,
    Code(CodeRef),
    /// `stamp` = 开始时口令行的 `set_at_ms`：口令中途被改 / 清除，这一轮的结果就不再计入。
    Password {
        stamp: u64,
    },
}

enum Step {
    /// 已发 `ready`（带 daemon 的 CPace 份额），等新设备的份额。
    AwaitCpace(Cpace),
    /// PSK 已定，等第 1 条 Noise 消息。
    AwaitMsg1(Box<Handshake>),
    /// 已回第 2 条，等第 3 条。
    AwaitMsg3(Box<Handshake>),
}

struct Attempt {
    kind: AttemptKind,
    /// 超时的起点：口令 / 扫码是发出 `ready` 的时刻；接入码是收到份额的时刻（等份额期间不计时，见 [`Attempt::timed`]）。
    started_ms: u64,
    step: Step,
}

impl Attempt {
    /// 这一轮现在在不在计时：接入码发了 `ready`、等新设备的份额时不计时（码自己有 5 分钟有效期和 3 次上限）。
    fn timed(&self) -> bool {
        !matches!((self.kind, &self.step), (AttemptKind::Code(_), Step::AwaitCpace(_)))
    }
}

#[derive(Clone)]
struct Hello {
    method: EnrollMethod,
    approval: bool,
    nonce: [u8; ENROLL_NONCE_LEN],
    hint: EnrollHint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    WaitingHello,
    WaitApproval,
    Ready,
    Confirming,
}

/// 握手成功、等 `pair_done` 的设备。
struct Confirming {
    transport: Option<Transport>,
    /// 暂存之前就登记过的同 id 老设备（重新接入）：失败时恢复它，而不是删掉。
    previous: Option<PeerRow>,
    name: String,
    claimed: bool,
    /// 暂存的时刻（看门狗从这里算）。
    since_ms: u64,
}

struct Session {
    id: String,
    peer: String,
    created_ms: u64,
    expires_ms: u64,
    /// 超出每台 3 个的上限：只是个桩，`hello` 来了回 `busy` 就走。
    over_limit: bool,
    stage: Stage,
    hello: Option<Hello>,
    hellos: u32,
    /// 批准 / 拒绝过：之后再批准 / 拒绝 → `already_resolved`。
    resolved: bool,
    /// 批准请求时生成的码。
    code: Option<CodeState>,
    attempt: Option<Attempt>,
    /// 口令会话只试一次。
    password_tried: bool,
    confirming: Option<Confirming>,
}

impl Session {
    fn new(id: String, peer: String, now: u64, over_limit: bool) -> Session {
        Session {
            id,
            peer,
            created_ms: now,
            expires_ms: now + SESSION_TTL_MS,
            over_limit,
            stage: Stage::WaitingHello,
            hello: None,
            hellos: 0,
            resolved: false,
            code: None,
            attempt: None,
            password_tried: false,
            confirming: None,
        }
    }

    fn info(&self) -> EnrollInfo {
        EnrollInfo {
            enroll_id: self.id.clone(),
            method: self.hello.as_ref().map(|h| h.method),
            approval: self.hello.as_ref().is_some_and(|h| h.approval),
            hint: self.hello.as_ref().map(|h| h.hint.clone()),
            status: match self.stage {
                Stage::WaitingHello => EnrollSessionStatus::WaitingHello,
                Stage::WaitApproval => EnrollSessionStatus::WaitApproval,
                Stage::Ready | Stage::Confirming => EnrollSessionStatus::Ready,
            },
            created_at_ms: self.created_ms,
            expires_at_ms: self.expires_ms,
        }
    }

    /// 审计里的发起方身份：完成前只有临时 id 和自报的 hint，名字标明未经认证。
    fn caller(&self) -> Caller {
        let (name, kind) = match &self.hello {
            Some(h) => (format!("{} (unverified)", h.hint.name), h.hint.kind),
            None => ("(unverified)".to_string(), DeviceKind::Web),
        };
        Caller { device_id: self.peer.clone(), device_name: name, kind, local: false }
    }

    fn method(&self) -> Option<EnrollMethod> {
        self.hello.as_ref().map(|h| h.method)
    }
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    active_code: Option<CodeState>,
    next_code_id: u64,
    /// 正在做口令尝试的会话（同时只允许一个，否则并发能绕过退避）。
    password_attempt: Option<String>,
    /// 低价值审计行的计数窗口（起点毫秒，条数）。
    audit_window: (u64, u32),
}

/// 一个接入会话怎么结束的。
enum End {
    /// `pair_done` 之后。
    Completed,
    /// 已配对设备拒绝了（`rejected { denied }` 已发出）。
    Denied { by: String },
    /// 过期（会话或次数）。
    Expired,
    /// hub 通知结束（`enroll_failed`）：不用再告诉 hub。
    Hub(EnrollFailReason),
    /// 我们给了终局性的 `rejected`。
    Rejected(EnrollRejectReason),
    /// hub 连接断了。
    HubDown,
    /// 同一个发起方来了新的会话。
    Replaced,
    /// 本地出错（写库失败等）。
    Internal(&'static str),
}

impl End {
    fn result(&self) -> EnrollResult {
        match self {
            End::Completed => EnrollResult::Completed,
            End::Denied { .. } => EnrollResult::Denied,
            End::Expired | End::Hub(EnrollFailReason::Timeout) => EnrollResult::Expired,
            _ => EnrollResult::Failed,
        }
    }

    fn reason(&self) -> String {
        match self {
            End::Completed => "completed".into(),
            End::Denied { .. } => "denied".into(),
            End::Expired => "expired".into(),
            End::Hub(r) => tag(r),
            End::Rejected(r) => tag(r),
            End::HubDown => "hub_down".into(),
            End::Replaced => "replaced".into(),
            End::Internal(s) => (*s).to_string(),
        }
    }

    /// 要不要告诉 hub 释放它那边的会话名额。
    fn tell_hub(&self) -> bool {
        !matches!(self, End::Completed | End::Hub(_) | End::HubDown)
    }
}

/// 接入码的检查结果（[`Enroll::code_gate`]）。
enum Gate {
    /// 会话没了。
    Gone,
    /// 码不能用了（没有 / 过期 / 次数用完 / 被顶掉），带该回的拒绝原因。
    Dead(EnrollRejectReason),
    Open {
        peer: String,
        code: Zeroizing<String>,
    },
}

/// 一轮尝试失败的原因（决定回什么、记什么）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    WrongSecret,
    Invalid,
    Timeout,
}

impl Fail {
    fn as_str(self) -> &'static str {
        match self {
            Fail::WrongSecret => "wrong_secret",
            Fail::Invalid => "invalid",
            Fail::Timeout => "timeout",
        }
    }

    fn reject_reason(self) -> EnrollRejectReason {
        match self {
            Fail::WrongSecret => EnrollRejectReason::WrongSecret,
            Fail::Invalid => EnrollRejectReason::Invalid,
            Fail::Timeout => EnrollRejectReason::Expired,
        }
    }
}

// ---------------------------------------------------------------- 引擎

pub struct EnrollDeps {
    pub store: Store,
    pub config: Arc<ConfigStore>,
    pub outbox: Arc<HubOutbox>,
    pub clients: Arc<Clients>,
    pub audit: Audit,
    pub notifier: Arc<Notifier>,
    pub identity: Arc<Identity>,
}

pub struct Enroll {
    store: Store,
    config: Arc<ConfigStore>,
    outbox: Arc<HubOutbox>,
    clients: Arc<Clients>,
    audit: Audit,
    notifier: Arc<Notifier>,
    identity: Arc<Identity>,
    me: Weak<Enroll>,
    peers: OnceLock<Weak<Peers>>,
    inner: Mutex<Inner>,
    /// 口令行的读-改-写（设置、计数、重新启用）都在这把锁里。
    pw_lock: tokio::sync::Mutex<()>,
    /// 已经向已配对设备报告过的 `[enroll]`：配置文件被直接改了（`anycon config set`、手动编辑）时，和它比较决定要不要补报。
    announced_cfg: Mutex<EnrollConfig>,
    clock: Clock,
}

impl Enroll {
    pub fn new(d: EnrollDeps) -> Arc<Enroll> {
        let announced = d.config.get().enroll.clone();
        Arc::new_cyclic(|me| Enroll {
            me: me.clone(),
            store: d.store,
            config: d.config,
            outbox: d.outbox,
            clients: d.clients,
            audit: d.audit,
            notifier: d.notifier,
            identity: d.identity,
            peers: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
            pw_lock: tokio::sync::Mutex::new(()),
            announced_cfg: Mutex::new(announced),
            clock: Clock::default(),
        })
    }

    /// `Peers::new` 里绑定（接入的收尾要用 Peers 的暂存 / 改键 / `pair.ack`）。
    pub fn bind_peers(&self, peers: &Arc<Peers>) {
        let _ = self.peers.set(Arc::downgrade(peers));
    }

    fn peers(&self) -> Option<Arc<Peers>> {
        self.peers.get().and_then(Weak::upgrade)
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    fn now(&self) -> u64 {
        self.clock.now()
    }

    fn node_id(&self) -> String {
        self.outbox.device_id().or_else(|| self.identity.device_id()).unwrap_or_default()
    }

    fn send_frame(&self, peer: &str, frame: &EnrollFrame) {
        match rmp_serde::to_vec_named(frame) {
            Ok(bytes) => {
                if self.outbox.send_envelope(peer, Kind::Handshake, bytes).is_none() {
                    tracing::debug!("hub not connected; an enroll frame was not sent");
                }
            }
            Err(e) => tracing::error!(error = %e, "cannot encode an enroll frame"),
        }
    }

    fn send_cancel(&self, enroll_id: &str) {
        self.outbox.send_control(&Control::EnrollCancel { enroll_id: enroll_id.to_string() });
    }

    // ------------------------------------------------------------ 审计与事件

    /// 不太重要的审计行（`enroll.requested`、预检拒绝）有每分钟的上限；返回这一条该不该写。
    fn low_value_budget(&self) -> bool {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        let (start, count) = &mut g.audit_window;
        if now.saturating_sub(*start) >= 60_000 {
            *start = now;
            *count = 0;
        }
        *count += 1;
        *count <= LOW_VALUE_AUDIT_PER_MIN
    }

    async fn audit_low(&self, caller: &Caller, action: &str, target: Option<&str>, detail: serde_json::Value) {
        if self.low_value_budget() {
            self.audit.record(caller, action, target, detail).await;
        }
    }

    async fn security_event(&self, ev: NodeSecurityEvent) {
        tracing::info!(kind = %tag(&ev.kind), "security event");
        self.clients.broadcast(methods::NODE_SECURITY_EVENT, &ev);
        self.notifier.security();
    }

    fn event(&self, kind: SecurityEventKind) -> NodeSecurityEvent {
        NodeSecurityEvent {
            kind,
            ts_ms: self.now(),
            by_device: None,
            device_id: None,
            device_name: None,
            method: None,
            failures: None,
            password_set: None,
        }
    }

    fn resolved(&self, enroll_id: &str, result: EnrollResult, by: Option<String>) {
        self.clients.broadcast(
            methods::NODE_ENROLL_RESOLVED,
            NodeEnrollResolved { enroll_id: enroll_id.to_string(), result, by_device: by },
        );
    }

    // ------------------------------------------------------------ hub 的控制消息

    /// hub 的 `enroll_request`：有设备请求接入。建会话，等它的 `hello`。
    pub async fn on_request(&self, enroll_id: String, peer: String) {
        let now = self.now();
        let stale: Vec<String> = {
            let g = self.inner.lock().unwrap();
            if g.sessions.contains_key(&enroll_id) {
                return;
            }
            g.sessions.values().filter(|s| s.peer == peer).map(|s| s.id.clone()).collect()
        };
        // 同一个发起方不会同时有两个会话（hub 每条连接只允许一个）：旧的是残留，作废
        for id in stale {
            self.end_session(&id, End::Replaced).await;
        }
        let mut g = self.inner.lock().unwrap();
        let live = g.sessions.values().filter(|s| !s.over_limit).count();
        let stubs = g.sessions.len() - live;
        let over_limit = live >= MAX_SESSIONS;
        if over_limit && stubs >= MAX_STUBS {
            drop(g);
            tracing::warn!("too many enroll requests; refusing one without tracking it");
            self.send_cancel(&enroll_id);
            return;
        }
        tracing::info!(enroll_id = %enroll_id, over_limit, "enroll request");
        g.sessions.insert(enroll_id.clone(), Session::new(enroll_id, peer, now, over_limit));
    }

    /// hub 的 `enroll_failed`：对端取消 / 断线 / 超时，会话结束。
    pub async fn on_failed(&self, enroll_id: Option<String>, reason: EnrollFailReason) {
        let Some(id) = enroll_id else { return };
        tracing::info!(enroll_id = %id, reason = %tag(&reason), "enroll failed (from the hub)");
        self.end_session(&id, End::Hub(reason)).await;
    }

    /// hub 连接断了：所有接入会话作废（hub 重启即作废）。
    pub async fn on_hub_down(&self) {
        let ids: Vec<String> = self.inner.lock().unwrap().sessions.keys().cloned().collect();
        for id in ids {
            self.end_session(&id, End::HubDown).await;
        }
    }

    /// 收尾：hub 的 `pair_done`。`formal` 是新设备的正式 id。暂存的记录改键、发 `pair.ack`、审计、通知。
    pub async fn on_pair_done(&self, enroll_id: &str, formal: String, my_device_id: Option<String>) {
        let taken = {
            let mut g = self.inner.lock().unwrap();
            g.sessions.get_mut(enroll_id).and_then(|s| {
                let (peer, method) = (s.peer.clone(), s.method());
                let c = s.confirming.as_mut()?;
                Some((peer, method, c.transport.take(), c.name.clone()))
            })
        };
        let Some((tentative, method, transport, name)) = taken else {
            tracing::warn!(enroll_id, "pair_done for an enroll session that is not waiting for it");
            return;
        };
        let Some(peers) = self.peers() else { return };
        if let Err(e) = peers.promote_staged(&tentative, &formal).await {
            tracing::error!(error = %e.message, "cannot save the enrolled device; not confirming the enrollment");
            self.end_session(enroll_id, End::Internal("save_failed")).await;
            return;
        }
        // 已登记：不用再回滚暂存了
        if let Some(s) = self.inner.lock().unwrap().sessions.get_mut(enroll_id) {
            if let Some(c) = s.confirming.as_mut() {
                c.previous = None;
            }
        }
        match transport {
            Some(mut t) => peers.send_enroll_ack(&formal, &mut t, my_device_id),
            None => tracing::error!("enrollment completed without a transport; the new device will not get pair.ack"),
        }
        let done = Caller { device_id: formal.clone(), device_name: name.clone(), kind: DeviceKind::Web, local: false };
        let kind = peers.record(&formal).map(|r| r.kind).unwrap_or(DeviceKind::Web);
        let done = Caller { kind, ..done };
        self.audit
            .record(
                &done,
                "enroll.completed",
                Some(enroll_id),
                serde_json::json!({ "method": method.map(|m| tag(&m)), "result": "completed" }),
            )
            .await;
        self.end_session(enroll_id, End::Completed).await;
        let mut ev = self.event(SecurityEventKind::DeviceEnrolled);
        ev.device_id = Some(formal);
        ev.device_name = Some(name);
        ev.method = method;
        self.security_event(ev).await;
    }

    // ------------------------------------------------------------ 会话结束

    /// 结束一个会话并清理：口令尝试的名额、进行中的口令尝试（算失败）、暂存的设备、hub 那边的名额、其它设备的卡片、审计。
    async fn end_session(&self, id: &str, end: End) {
        let Some(mut s) = self.inner.lock().unwrap().sessions.remove(id) else { return };
        {
            let mut g = self.inner.lock().unwrap();
            if g.password_attempt.as_deref() == Some(id) {
                g.password_attempt = None;
            }
        }
        let caller = s.caller();
        // 进行中的口令尝试：到这里还没成功，就是一次失败（计数早就 +1 了，这里补记事件、退避时间和审计）
        let password_failure_logged = match s.attempt.take() {
            Some(Attempt { kind: AttemptKind::Password { stamp }, .. }) => {
                self.finalize_password_failure(&caller, stamp, &end.reason()).await;
                true
            }
            _ => false,
        };
        if let Some(c) = s.confirming.take() {
            if let Some(peers) = self.peers() {
                peers.release_confirm(&ConfirmOwner::Enroll(id.to_string()));
                if !matches!(end, End::Completed) {
                    peers.enroll_unstage(&s.peer, c.previous).await;
                }
            }
        }
        if end.tell_hub() {
            self.send_cancel(id);
        }
        if s.over_limit || s.hello.is_none() {
            return;
        }
        let by = match &end {
            End::Denied { by } => Some(by.clone()),
            _ => None,
        };
        self.resolved(id, end.result(), by);
        // 审计：完成和拒绝由各自的调用方记，被 `reject` / `attempt_failed` 终结的已经记过；这里记其余的失败 / 过期
        if !password_failure_logged && !matches!(end, End::Completed | End::Denied { .. } | End::Rejected(_)) {
            let detail = serde_json::json!({
                "method": s.method().map(|m| tag(&m)),
                "reason": end.reason(),
                "result": tag(&end.result()),
            });
            self.audit_low(&caller, "enroll.failed", Some(id), detail).await;
        }
    }

    // ------------------------------------------------------------ 握手帧

    /// `handshake` 信封：属于某个接入会话（发起方 id 对得上、body 是 `EnrollFrame`）就处理并返回 `true`；
    /// 否则返回 `false`，交给配对 / IK 重连。
    pub async fn on_handshake(&self, from: &str, body: &[u8]) -> bool {
        let id = {
            let g = self.inner.lock().unwrap();
            g.sessions.values().filter(|s| s.peer == from).max_by_key(|s| s.created_ms).map(|s| s.id.clone())
        };
        let Some(id) = id else { return false };
        let Ok(frame) = rmp_serde::from_slice::<EnrollFrame>(body) else { return false };
        match frame {
            EnrollFrame::Hello { v, method, approval, nonce, hint } => {
                self.on_hello(&id, v, method, approval, &nonce, &hint).await
            }
            EnrollFrame::Cpace { share } => self.on_cpace(&id, &share).await,
            EnrollFrame::Noise { msg } => self.on_noise(&id, &msg).await,
            EnrollFrame::Reply { .. } => tracing::debug!("a reply frame from the new device; ignored"),
        }
        true
    }

    fn reply_rejected(
        &self,
        peer: &str,
        reason: EnrollRejectReason,
        retry_after_ms: Option<u64>,
        attempts_left: Option<u32>,
    ) {
        self.send_frame(
            peer,
            &EnrollFrame::Reply {
                status: EnrollReplyStatus::Rejected,
                nonce: None,
                reason: Some(reason),
                retry_after_ms,
                attempts_left,
                salt: None,
                cpace: None,
            },
        );
    }

    /// 回一条 `rejected`。`terminal` 为真时会话也到此为止（并告诉 hub）。
    async fn reject(
        &self,
        id: &str,
        reason: EnrollRejectReason,
        retry_after_ms: Option<u64>,
        attempts_left: Option<u32>,
        terminal: bool,
    ) {
        let info = {
            let g = self.inner.lock().unwrap();
            g.sessions.get(id).map(|s| (s.peer.clone(), s.caller(), s.method()))
        };
        let Some((peer, caller, method)) = info else { return };
        tracing::info!(enroll_id = %id, reason = %tag(&reason), terminal, "enroll rejected");
        self.reply_rejected(&peer, reason, retry_after_ms, attempts_left);
        // 预检拒绝（方式关了、没设口令、退避中、锁定、没有码、满了…）留一行审计，有每分钟的上限；
        // `wrong_secret` 已经由 `attempt_failed` 带着详情记过了
        if reason != EnrollRejectReason::WrongSecret {
            self.audit_low(
                &caller,
                "enroll.failed",
                Some(id),
                serde_json::json!({
                    "method": method.map(|m| tag(&m)),
                    "reason": tag(&reason),
                    "result": if reason == EnrollRejectReason::Expired { "expired" } else { "failed" },
                }),
            )
            .await;
        }
        if terminal {
            self.end_session(id, End::Rejected(reason)).await;
        }
    }

    async fn on_hello(&self, id: &str, v: u32, method: EnrollMethod, approval: bool, nonce: &str, hint: &EnrollHint) {
        let now = self.now();
        enum Pre {
            Gone,
            Stub,
            Expired,
            Invalid,
            /// 请求批准的会话在等：同一个请求重发 hello，原样再回 `wait_approval`。
            Resend(String),
            Go,
        }
        let n_nonce = decode_hex::<ENROLL_NONCE_LEN>(nonce);
        let pre = {
            let mut g = self.inner.lock().unwrap();
            match g.sessions.get_mut(id) {
                None => Pre::Gone,
                Some(s) if s.over_limit => Pre::Stub,
                Some(s) if s.expires_ms <= now => Pre::Expired,
                Some(s) => {
                    s.hellos += 1;
                    let same = s.hello.as_ref().is_some_and(|h| h.method == method && h.approval == approval);
                    let well_formed = v == ENROLL_FRAME_VERSION && s.hellos <= 12;
                    if let (Stage::WaitApproval, true, true, Some(nonce)) = (s.stage, same, well_formed, n_nonce) {
                        // 等批准期间重发：换成最新的 nonce（`Initiator::hello()` 每次都换），批准后的 `ready` 用它算 sid；
                        // hint 保持第一次的（它只用于批准界面，已经展示给批准的人了）
                        if let Some(h) = s.hello.as_mut() {
                            h.nonce = nonce;
                        }
                        Pre::Resend(s.peer.clone())
                    } else {
                        let allowed = match (s.stage, &s.hello) {
                            (Stage::WaitingHello, _) => true,
                            // `code` 在同一个会话里可以重发 hello 再试（新 nonce、新一轮 CPace）
                            (Stage::Ready, Some(h)) => h.method == EnrollMethod::Code && method == EnrollMethod::Code,
                            _ => false,
                        };
                        if v != ENROLL_FRAME_VERSION || n_nonce.is_none() || !allowed || s.hellos > 12 {
                            Pre::Invalid
                        } else {
                            // 上一轮的尝试作废（接入码的次数在收到份额时才扣，只发 hello 不扣）
                            s.attempt = None;
                            Pre::Go
                        }
                    }
                }
            }
        };
        match pre {
            Pre::Gone => return,
            Pre::Stub => return self.reject(id, EnrollRejectReason::Busy, None, None, true).await,
            Pre::Expired => return self.reject(id, EnrollRejectReason::Expired, None, None, true).await,
            Pre::Invalid => return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await,
            Pre::Resend(peer) => return self.send_frame(&peer, &wait_approval_frame()),
            Pre::Go => {}
        }
        let n_nonce = n_nonce.expect("checked above");
        let (first, caller) = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sessions.get_mut(id) else { return };
            let first = s.hello.is_none();
            s.hello = Some(Hello { method, approval, nonce: n_nonce, hint: clean_hint(hint) });
            (first, s.caller())
        };
        if first {
            self.audit_low(
                &caller,
                "enroll.requested",
                Some(id),
                serde_json::json!({ "method": tag(&method), "approval": approval, "result": "requested" }),
            )
            .await;
        }
        let cfg = self.config.get();
        match method {
            EnrollMethod::Scan => {
                if !cfg.enroll.approval {
                    return self.reject(id, EnrollRejectReason::MethodDisabled, None, None, false).await;
                }
                self.wait_for_approval(id, false).await;
            }
            EnrollMethod::Code if approval => {
                if !cfg.enroll.approval {
                    return self.reject(id, EnrollRejectReason::MethodDisabled, None, None, false).await;
                }
                let granted = self.inner.lock().unwrap().sessions.get(id).is_some_and(|s| s.resolved);
                if granted {
                    // 已经批准过（上一轮输错了重试）：直接用这个会话的码再来一轮
                    self.begin_code(id, CodeRef::Session).await;
                } else {
                    self.wait_for_approval(id, true).await;
                }
            }
            EnrollMethod::Code => {
                if !cfg.enroll.approval {
                    return self.reject(id, EnrollRejectReason::MethodDisabled, None, None, false).await;
                }
                let active = self.inner.lock().unwrap().active_code.as_ref().map(|c| c.id);
                match active {
                    Some(cid) => self.begin_code(id, CodeRef::Active(cid)).await,
                    None => self.reject(id, EnrollRejectReason::NoCode, None, None, false).await,
                }
            }
            EnrollMethod::Password => self.begin_password(id, n_nonce).await,
        }
    }

    /// `scan` / 请求批准的 `code`：回 `wait_approval`；`code` 另外通知所有已配对设备（`scan` 不推送，二维码只在新设备的屏幕上）。
    async fn wait_for_approval(&self, id: &str, announce: bool) {
        let info = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sessions.get_mut(id) else { return };
            s.stage = Stage::WaitApproval;
            (s.peer.clone(), s.info())
        };
        self.send_frame(&info.0, &wait_approval_frame());
        if announce {
            self.clients.broadcast(methods::NODE_ENROLL_REQUESTED, &info.1);
            self.notifier.enroll_requested();
        }
    }

    /// 看一眼接入码还能不能用；`spend` 为真时同时扣一次次数（收到新设备的份额时扣，见 [`Enroll::on_cpace`]）。
    /// 过期 / 用完的码顺手清掉。
    fn code_gate(&self, id: &str, cref: CodeRef, spend: bool) -> Gate {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        let Inner { sessions, active_code, .. } = &mut *g;
        let Some(s) = sessions.get_mut(id) else { return Gate::Gone };
        let slot: &mut Option<CodeState> = match cref {
            CodeRef::Session => &mut s.code,
            CodeRef::Active(_) => active_code,
        };
        let fresh = |c: &CodeState| match cref {
            CodeRef::Session => true,
            CodeRef::Active(cid) => c.id == cid,
        };
        // 会话上的码用完 / 过期是 `expired`；主动码没有只是这次没有（`no_code`）
        let dead = match cref {
            CodeRef::Session => EnrollRejectReason::Expired,
            CodeRef::Active(_) => EnrollRejectReason::NoCode,
        };
        match slot.as_mut().filter(|c| fresh(c)) {
            None => Gate::Dead(dead),
            Some(c) if c.expires_ms <= now || c.attempts_left == 0 => {
                *slot = None;
                Gate::Dead(dead)
            }
            Some(c) => {
                if spend {
                    c.attempts_left -= 1;
                }
                Gate::Open { peer: s.peer.clone(), code: c.code.clone() }
            }
        }
    }

    /// 一轮 `code` 尝试：算 CPace 份额，回 `ready`，然后**不计时**地等新设备的份额。**这里不扣次数**：批准本身、
    /// 只发 `hello` 都不该耗掉接入码；次数在收到份额时扣（[`Enroll::on_cpace`]）。
    /// 用的 nonce 是会话里最新那条 `hello` 的。
    async fn begin_code(&self, id: &str, cref: CodeRef) {
        let now = self.now();
        let node_id = self.node_id();
        let (peer, code) = match self.code_gate(id, cref, false) {
            Gate::Gone => return,
            Gate::Dead(reason) => {
                // 会话上的码用完 / 过期是终局；主动码没有只是这次没有（用户可以现生成一个再试）
                let terminal = matches!(cref, CodeRef::Session);
                return self.reject(id, reason, None, None, terminal).await;
            }
            Gate::Open { peer, code } => (peer, code),
        };
        let Some(n_nonce) = self.inner.lock().unwrap().sessions.get(id).and_then(|s| s.hello.as_ref().map(|h| h.nonce))
        else {
            return;
        };
        let d_nonce = generate_enroll_nonce();
        let ci = enroll_channel_id(&node_id, "code");
        let sid = enroll_sid(&n_nonce, &d_nonce);
        let cpace = match Cpace::new(code.as_bytes(), &ci, &sid) {
            Ok(c) => c,
            Err(_) => return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await,
        };
        let share = cpace.share();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sessions.get_mut(id) else { return };
            s.stage = Stage::Ready;
            s.attempt = Some(Attempt { kind: AttemptKind::Code(cref), started_ms: now, step: Step::AwaitCpace(cpace) });
        }
        self.send_frame(&peer, &ready_frame(&d_nonce, None, Some(&share)));
    }

    /// 一轮 `password` 尝试。**先把失败次数 +1 写库，再发 CPace 份额**。
    async fn begin_password(&self, id: &str, n_nonce: [u8; ENROLL_NONCE_LEN]) {
        let cfg = self.config.get();
        if !cfg.enroll.password {
            return self.reject(id, EnrollRejectReason::MethodDisabled, None, None, false).await;
        }
        // 口令会话只试一次
        let tried = {
            let g = self.inner.lock().unwrap();
            g.sessions.get(id).is_some_and(|s| s.password_tried)
        };
        if tried {
            return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await;
        }
        // 占住唯一的口令尝试名额（同时只允许一个，否则并发能绕过退避）
        let busy = {
            let mut g = self.inner.lock().unwrap();
            let in_flight =
                g.password_attempt.as_ref().is_some_and(|other| other != id && g.sessions.contains_key(other));
            if !in_flight {
                g.password_attempt = Some(id.to_string());
            }
            in_flight
        };
        if busy {
            return self.reject(id, EnrollRejectReason::Busy, None, None, false).await;
        }
        let release = |this: &Enroll| {
            let mut g = this.inner.lock().unwrap();
            if g.password_attempt.as_deref() == Some(id) {
                g.password_attempt = None;
            }
        };
        let guard = self.pw_lock.lock().await;
        let now = self.now();
        let row = match self.store.enroll_password_get().await {
            Ok(Some(r)) => r,
            Ok(None) => {
                drop(guard);
                release(self);
                return self.reject(id, EnrollRejectReason::PasswordNotSet, None, None, false).await;
            }
            Err(e) => {
                drop(guard);
                release(self);
                tracing::error!(error = %e, "cannot read the node password");
                return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await;
            }
        };
        if row.locked {
            drop(guard);
            release(self);
            return self.reject(id, EnrollRejectReason::Locked, None, None, false).await;
        }
        if row.next_allowed_at_ms > now {
            drop(guard);
            release(self);
            let wait = row.next_allowed_at_ms - now;
            return self.reject(id, EnrollRejectReason::Backoff, Some(wait), None, false).await;
        }
        let n = row.failures.saturating_add(1);
        let next = if n >= BACKOFF_FROM { now + password_backoff_ms(n) } else { 0 };
        match self.store.enroll_password_record(n, next, n >= LOCK_AT).await {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                drop(guard);
                release(self);
                tracing::error!("cannot record the password attempt; refusing it");
                return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await;
            }
        }
        drop(guard);
        // 计数已落盘；现在才算份额、发出去
        let d_nonce = generate_enroll_nonce();
        let ci = enroll_channel_id(&self.node_id(), "password");
        let sid = enroll_sid(&n_nonce, &d_nonce);
        let cpace = match Cpace::new(&row.prs, &ci, &sid) {
            Ok(c) => c,
            Err(_) => {
                // 已经计了一次失败，不退
                let caller = self.caller_of(id);
                self.finalize_password_failure(&caller, row.set_at_ms, "invalid").await;
                release(self);
                return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await;
            }
        };
        let share = cpace.share();
        let peer = {
            let mut g = self.inner.lock().unwrap();
            match g.sessions.get_mut(id) {
                Some(s) => {
                    s.stage = Stage::Ready;
                    s.password_tried = true;
                    s.attempt = Some(Attempt {
                        kind: AttemptKind::Password { stamp: row.set_at_ms },
                        started_ms: now,
                        step: Step::AwaitCpace(cpace),
                    });
                    Some(s.peer.clone())
                }
                None => None,
            }
        };
        match peer {
            Some(peer) => self.send_frame(&peer, &ready_frame(&d_nonce, Some(&row.salt), Some(&share))),
            None => {
                // 会话在我们写库的时候没了：这一轮没有发出任何东西，但计数不退（悲观计数）
                release(self);
                let caller = Caller {
                    device_id: "unknown".into(),
                    device_name: "(unverified)".into(),
                    kind: DeviceKind::Web,
                    local: false,
                };
                self.finalize_password_failure(&caller, row.set_at_ms, "cancelled").await;
            }
        }
    }

    fn caller_of(&self, id: &str) -> Caller {
        let g = self.inner.lock().unwrap();
        g.sessions.get(id).map(Session::caller).unwrap_or(Caller {
            device_id: "unknown".into(),
            device_name: "(unverified)".into(),
            kind: DeviceKind::Web,
            local: false,
        })
    }

    async fn on_cpace(&self, id: &str, share: &str) {
        let taken = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sessions.get_mut(id) else { return };
            match s.attempt.take() {
                Some(Attempt { kind, started_ms, step: Step::AwaitCpace(c) }) => {
                    Some((kind, started_ms, c, s.peer.clone()))
                }
                other => {
                    s.attempt = other;
                    None
                }
            }
        };
        let Some((kind, started_ms, cpace, _peer)) = taken else {
            tracing::debug!("a cpace frame outside a code/password attempt; ignored");
            return;
        };
        // 接入码：**收到份额时才扣一次次数**（每个份额最多换来一次第 3 条的校验，所以猜测上限不变），
        // 同时计时从这里开始（份额 → 第 3 条限 60 秒）。口令的计数已经在发份额之前写库了。
        let started_ms = match kind {
            AttemptKind::Code(cref) => match self.code_gate(id, cref, true) {
                Gate::Gone => return,
                Gate::Dead(reason) => {
                    // 等份额的时候码被顶掉 / 过期 / 被别的会话用完了
                    let terminal = matches!(cref, CodeRef::Session);
                    return self.reject(id, reason, None, None, terminal).await;
                }
                Gate::Open { .. } => self.now(),
            },
            _ => started_ms,
        };
        let psk = decode_hex::<32>(share).and_then(|s| cpace.finish(Role::Responder, &s).ok());
        let Some(psk) = psk.map(Zeroizing::new) else {
            return self.attempt_failed(id, kind, Fail::Invalid).await;
        };
        let kp = self.identity.keypair();
        let Ok(hs) = Handshake::pairing(Role::Responder, &kp, &psk) else {
            return self.attempt_failed(id, kind, Fail::Invalid).await;
        };
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.sessions.get_mut(id) {
            s.attempt = Some(Attempt { kind, started_ms, step: Step::AwaitMsg1(Box::new(hs)) });
        }
    }

    async fn on_noise(&self, id: &str, msg: &str) {
        let taken = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sessions.get_mut(id) else { return };
            match s.attempt.take() {
                Some(a @ Attempt { step: Step::AwaitMsg1(_) | Step::AwaitMsg3(_), .. }) => Some(a),
                other => {
                    s.attempt = other;
                    None
                }
            }
        };
        let Some(Attempt { kind, started_ms, step }) = taken else {
            tracing::debug!("a noise frame outside a handshake; ignored");
            return;
        };
        let Ok(bytes) = hex::decode(msg) else { return self.attempt_failed(id, kind, Fail::Invalid).await };
        match step {
            Step::AwaitMsg1(mut hs) => {
                // 第 1 条不用 PSK：读不了只可能是格式错
                if hs.read(&bytes).is_err() {
                    return self.attempt_failed(id, kind, Fail::Invalid).await;
                }
                // 第 2 条带节点信息 `{"name","version"}`（未认证，新设备只当提示）
                let info = serde_json::to_vec(&serde_json::json!({
                    "name": self.config.get().node_name(),
                    "version": env!("CARGO_PKG_VERSION"),
                }))
                .unwrap_or_default();
                let Ok(m2) = hs.write(&info) else { return self.attempt_failed(id, kind, Fail::Invalid).await };
                let peer = {
                    let mut g = self.inner.lock().unwrap();
                    let Some(s) = g.sessions.get_mut(id) else { return };
                    s.attempt = Some(Attempt { kind, started_ms, step: Step::AwaitMsg3(hs) });
                    s.peer.clone()
                };
                self.send_frame(&peer, &EnrollFrame::Noise { msg: hex::encode(m2) });
            }
            Step::AwaitMsg3(mut hs) => match hs.read(&bytes) {
                // 第 3 条在这里校验失败：PSK 不一致（码 / 口令 / 扫码密钥不对，或有中间人）
                Err(_) => self.attempt_failed(id, kind, Fail::WrongSecret).await,
                Ok(payload) if hs.is_finished() => self.handshake_succeeded(id, kind, *hs, payload).await,
                Ok(_) => self.attempt_failed(id, kind, Fail::Invalid).await,
            },
            Step::AwaitCpace(_) => {}
        }
    }

    /// 一轮尝试失败：口令记一次失败（计数早已 +1）、接入码看剩几次、回 `rejected`。
    async fn attempt_failed(&self, id: &str, kind: AttemptKind, why: Fail) {
        let caller = self.caller_of(id);
        match kind {
            AttemptKind::Password { stamp } => {
                // 口令会话只试一次：失败即终局
                self.finalize_password_failure(&caller, stamp, why.as_str()).await;
                {
                    let mut g = self.inner.lock().unwrap();
                    if g.password_attempt.as_deref() == Some(id) {
                        g.password_attempt = None;
                    }
                }
                let reason = why.reject_reason();
                self.reject(id, reason, None, None, true).await;
            }
            AttemptKind::Code(cref) => {
                let left = {
                    let mut g = self.inner.lock().unwrap();
                    let Inner { sessions, active_code, .. } = &mut *g;
                    let slot = match cref {
                        CodeRef::Session => sessions.get_mut(id).map(|s| &mut s.code),
                        CodeRef::Active(cid) => Some(active_code).filter(|c| c.as_ref().is_some_and(|c| c.id == cid)),
                    };
                    match slot {
                        Some(slot) => {
                            let left = slot.as_ref().map_or(0, |c| c.attempts_left);
                            if left == 0 {
                                *slot = None;
                            }
                            left
                        }
                        None => 0,
                    }
                };
                self.audit
                    .record(
                        &caller,
                        "enroll.failed",
                        Some(id),
                        serde_json::json!({ "method": "code", "reason": why.as_str(), "result": "failed", "attempts_left": left }),
                    )
                    .await;
                let reason = why.reject_reason();
                let terminal = why == Fail::Invalid;
                self.reject(id, reason, None, Some(left), terminal).await;
            }
            AttemptKind::Scan => {
                self.audit
                    .record(
                        &caller,
                        "enroll.failed",
                        Some(id),
                        serde_json::json!({ "method": "scan", "reason": why.as_str(), "result": "failed" }),
                    )
                    .await;
                let reason = why.reject_reason();
                self.reject(id, reason, None, None, true).await;
            }
        }
    }

    /// 口令尝试以失败告终（计数已在发份额之前 +1）：刷新退避时间，满 3 次发 `password_failures`，满 10 次锁定并发 `password_locked`，记审计。
    async fn finalize_password_failure(&self, caller: &Caller, stamp: u64, reason: &str) {
        let guard = self.pw_lock.lock().await;
        let row = match self.store.enroll_password_get().await {
            Ok(Some(r)) if r.set_at_ms == stamp => r,
            // 口令中途被改 / 清除：这一轮不再计
            _ => return,
        };
        let n = row.failures;
        if n == 0 {
            // 中途被重新启用，计数清零了
            return;
        }
        let next = if n >= BACKOFF_FROM { self.now() + password_backoff_ms(n) } else { 0 };
        if let Err(e) = self.store.enroll_password_record(n, next, n >= LOCK_AT).await {
            tracing::error!(error = %e, "cannot refresh the password backoff");
        }
        drop(guard);
        self.audit
            .record(
                caller,
                "enroll.failed",
                None,
                serde_json::json!({ "method": "password", "reason": reason, "result": "failed", "failures": n }),
            )
            .await;
        if n == BACKOFF_FROM {
            let mut ev = self.event(SecurityEventKind::PasswordFailures);
            ev.failures = Some(n);
            self.security_event(ev).await;
        }
        if n >= LOCK_AT {
            self.audit
                .record(
                    caller,
                    "enroll.password_locked",
                    None,
                    serde_json::json!({ "method": "password", "result": "failed", "failures": n }),
                )
                .await;
            let mut ev = self.event(SecurityEventKind::PasswordLocked);
            ev.failures = Some(n);
            self.security_event(ev).await;
        }
    }

    /// 第 3 条校验通过：接入码 / 口令的收尾（一次性、清零），暂存设备，发 `pair_confirm`。
    async fn handshake_succeeded(&self, id: &str, kind: AttemptKind, hs: Handshake, payload: Vec<u8>) {
        // 1. 凭据的收尾
        match kind {
            AttemptKind::Password { stamp } => {
                let guard = self.pw_lock.lock().await;
                {
                    let mut g = self.inner.lock().unwrap();
                    if g.password_attempt.as_deref() == Some(id) {
                        g.password_attempt = None;
                    }
                }
                match self.store.enroll_password_get().await {
                    Ok(Some(r)) if r.set_at_ms == stamp => {
                        if let Err(e) = self.store.enroll_password_record(0, 0, false).await {
                            tracing::error!(error = %e, "cannot clear the password failure count");
                        }
                    }
                    // 口令在握手的过程中被改了 / 清了：这次接入用的是旧口令，不放行
                    _ => {
                        drop(guard);
                        return self.reject(id, EnrollRejectReason::Expired, None, None, true).await;
                    }
                }
            }
            AttemptKind::Code(cref) => {
                // 一次性：拿走码。握手过程中码被顶掉 / 用掉 / 过期了就不放行
                let now = self.now();
                let ok = {
                    let mut g = self.inner.lock().unwrap();
                    let Inner { sessions, active_code, .. } = &mut *g;
                    match cref {
                        CodeRef::Session => {
                            sessions.get_mut(id).and_then(|s| s.code.take()).is_some_and(|c| c.expires_ms > now)
                        }
                        CodeRef::Active(cid) => match active_code.take() {
                            Some(c) if c.id == cid && c.expires_ms > now => true,
                            other => {
                                *active_code = other;
                                false
                            }
                        },
                    }
                };
                if !ok {
                    return self.reject(id, EnrollRejectReason::Expired, None, None, true).await;
                }
            }
            AttemptKind::Scan => {}
        }
        // 2. 暂存设备
        let Some(remote) = hs.remote_public() else {
            return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await;
        };
        let transport = match hs.into_transport() {
            Ok(t) => t,
            Err(_) => return self.reject(id, EnrollRejectReason::Invalid, None, None, true).await,
        };
        let Some(peers) = self.peers() else { return };
        let peer = {
            let g = self.inner.lock().unwrap();
            match g.sessions.get(id) {
                Some(s) => s.peer.clone(),
                None => return,
            }
        };
        let (name, dev_kind) = crate::peers::parse_client_info(&payload, &peer);
        let name = clean_text(&name, 100);
        let name = if name.is_empty() { format!("device-{}", peer.chars().take(8).collect::<String>()) } else { name };
        let row = PeerRow {
            device_id: peer.clone(),
            pubkey: remote,
            name: name.clone(),
            kind: dev_kind,
            paired_at_ms: now_ms(),
            last_seen_ms: None,
        };
        let previous = match peers.enroll_stage(&peer, row).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e.message, "cannot register the new device; abandoning this enrollment");
                return self.end_session(id, End::Internal("stage_failed")).await;
            }
        };
        // 写库期间会话可能已经没了（hub 通知结束、过期…）：暂存的记录不能留下
        let still_ours = {
            let mut g = self.inner.lock().unwrap();
            match g.sessions.get_mut(id) {
                Some(s) => {
                    s.stage = Stage::Confirming;
                    s.attempt = None;
                    s.confirming = Some(Confirming {
                        transport: Some(transport),
                        previous: previous.clone(),
                        name,
                        claimed: false,
                        since_ms: self.now(),
                    });
                    true
                }
                None => false,
            }
        };
        if !still_ours {
            peers.enroll_unstage(&peer, previous).await;
            return;
        }
        // 3. pair_confirm：和配对共用一个「确认槽」，同一时刻只有一个等 pair_done 的（pair_done 里没有会话标识）
        if peers.claim_confirm(ConfirmOwner::Enroll(id.to_string())) {
            self.send_confirm(id, &peer);
        } else {
            let (this, id, peer) = (self.me(), id.to_string(), peer);
            tokio::spawn(async move {
                let Some(this) = this else { return };
                for _ in 0..150 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if this.inner.lock().unwrap().sessions.get(&id).is_none_or(|s| s.confirming.is_none()) {
                        return;
                    }
                    let Some(peers) = this.peers() else { return };
                    if peers.claim_confirm(ConfirmOwner::Enroll(id.clone())) {
                        this.send_confirm(&id, &peer);
                        return;
                    }
                }
                this.end_session(&id, End::Internal("confirm_busy")).await;
            });
        }
    }

    fn me(&self) -> Option<Arc<Enroll>> {
        self.me.upgrade()
    }

    fn send_confirm(&self, id: &str, peer: &str) {
        if let Some(s) = self.inner.lock().unwrap().sessions.get_mut(id) {
            if let Some(c) = s.confirming.as_mut() {
                c.claimed = true;
            }
        }
        self.outbox.send_control(&Control::PairConfirm { peer: peer.to_string() });
    }

    // ------------------------------------------------------------ RPC

    /// `node.enroll_list`：进行中的接入会话（含还没收到 `hello` 的）。
    pub fn list(&self) -> NodeEnrollListResult {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<&Session> = g.sessions.values().filter(|s| !s.over_limit).collect();
        v.sort_by_key(|s| s.created_ms);
        NodeEnrollListResult { enrolls: v.into_iter().map(Session::info).collect() }
    }

    /// `node.enroll_approve`。
    pub async fn approve(
        &self,
        caller: &Caller,
        p: NodeEnrollApproveParams,
    ) -> Result<NodeEnrollApproveResult, RpcError> {
        let now = self.now();
        let cfg = self.config.get();
        let node_id = self.node_id();
        enum Plan {
            Code { code: String, expires_ms: u64 },
            Scan { hs: Box<Handshake> },
        }
        let (plan, peer) = {
            let mut g = self.inner.lock().unwrap();
            let s =
                g.sessions.get_mut(&p.enroll_id).filter(|s| !s.over_limit && s.expires_ms > now).ok_or_else(|| {
                    RpcError::new(ErrorCode::NotFound, "no such enrollment request (it may have expired)")
                })?;
            if s.resolved {
                return Err(RpcError::new(
                    ErrorCode::AlreadyResolved,
                    "another device has already handled this request",
                ));
            }
            match s.stage {
                Stage::WaitApproval => {}
                Stage::WaitingHello => {
                    return Err(RpcError::new(
                        ErrorCode::Busy,
                        "the new device has not sent its request yet; try again",
                    ))
                }
                _ => {
                    return Err(RpcError::new(
                        ErrorCode::AlreadyResolved,
                        "this request is no longer waiting for approval",
                    ))
                }
            }
            let h = s
                .hello
                .clone()
                .ok_or_else(|| RpcError::new(ErrorCode::Busy, "the new device has not sent its request yet"))?;
            if !cfg.enroll.approval {
                return Err(RpcError::new(
                    ErrorCode::Forbidden,
                    "approving new devices is turned off ([enroll].approval = false)",
                ));
            }
            let plan = match (h.method, h.approval) {
                (EnrollMethod::Code, true) => {
                    // 请求批准的 `code` 没有二维码密钥：带了 `secret` 说明批准的人以为这是扫码请求，
                    // 不能悄悄忽略（否则他把返回的码当成「已批准」丢掉，真正请求的人永远拿不到码）。
                    // 在改任何状态之前拒绝：不算已处理、不扣次数
                    if p.secret.is_some() {
                        return Err(RpcError::new(
                            ErrorCode::Invalid,
                            "this is not a QR-code request; do not send a secret (approving it returns a 6-digit code)",
                        ));
                    }
                    let code = generate_enroll_code();
                    let expires_ms = (now + CODE_TTL_MS).min(s.expires_ms);
                    s.code = Some(CodeState {
                        id: 0,
                        code: Zeroizing::new(code.clone()),
                        expires_ms,
                        attempts_left: CODE_ATTEMPTS,
                    });
                    Plan::Code { code, expires_ms }
                }
                (EnrollMethod::Scan, _) => {
                    let secret =
                        p.secret.as_deref().and_then(decode_hex::<ENROLL_SECRET_LEN>).map(Zeroizing::new).ok_or_else(
                            || {
                                RpcError::new(
                                    ErrorCode::Invalid,
                                    "secret must be the 32 bytes from the QR code, as 64 hex characters",
                                )
                            },
                        )?;
                    let psk = Zeroizing::new(derive_enroll_scan_psk(&secret, &node_id));
                    let kp = self.identity.keypair();
                    let hs = Handshake::pairing(Role::Responder, &kp, &psk)
                        .map_err(|_| RpcError::new(ErrorCode::Internal, "cannot start the handshake"))?;
                    Plan::Scan { hs: Box::new(hs) }
                }
                _ => return Err(RpcError::new(ErrorCode::Invalid, "this request does not need approval")),
            };
            s.resolved = true;
            (plan, s.peer.clone())
        };
        let by = if caller.local { "local".to_string() } else { caller.device_id.clone() };
        let method = match &plan {
            Plan::Code { .. } => EnrollMethod::Code,
            Plan::Scan { .. } => EnrollMethod::Scan,
        };
        self.audit
            .record(
                caller,
                "enroll.approved",
                Some(&p.enroll_id),
                serde_json::json!({ "method": tag(&method), "result": "approved" }),
            )
            .await;
        self.resolved(&p.enroll_id, EnrollResult::Approved, Some(by));
        match plan {
            Plan::Code { code, expires_ms } => {
                self.begin_code(&p.enroll_id, CodeRef::Session).await;
                Ok(NodeEnrollApproveResult { code: Some(code), expires_at_ms: Some(expires_ms) })
            }
            Plan::Scan { hs } => {
                let d_nonce = generate_enroll_nonce();
                {
                    let mut g = self.inner.lock().unwrap();
                    if let Some(s) = g.sessions.get_mut(&p.enroll_id) {
                        s.stage = Stage::Ready;
                        s.attempt =
                            Some(Attempt { kind: AttemptKind::Scan, started_ms: now, step: Step::AwaitMsg1(hs) });
                    }
                }
                self.send_frame(&peer, &ready_frame(&d_nonce, None, None));
                Ok(NodeEnrollApproveResult::default())
            }
        }
    }

    /// `node.enroll_deny`。
    pub async fn deny(&self, caller: &Caller, enroll_id: &str) -> Result<(), RpcError> {
        let now = self.now();
        let peer = {
            let mut g = self.inner.lock().unwrap();
            let s = g.sessions.get_mut(enroll_id).filter(|s| !s.over_limit && s.expires_ms > now).ok_or_else(|| {
                RpcError::new(ErrorCode::NotFound, "no such enrollment request (it may have expired)")
            })?;
            if s.resolved {
                return Err(RpcError::new(
                    ErrorCode::AlreadyResolved,
                    "another device has already handled this request",
                ));
            }
            match s.stage {
                Stage::WaitApproval => {}
                Stage::WaitingHello => {
                    return Err(RpcError::new(
                        ErrorCode::Busy,
                        "the new device has not sent its request yet; try again",
                    ))
                }
                _ => {
                    return Err(RpcError::new(
                        ErrorCode::AlreadyResolved,
                        "this request is no longer waiting for approval",
                    ))
                }
            }
            s.resolved = true;
            s.peer.clone()
        };
        self.audit.record(caller, "enroll.denied", Some(enroll_id), serde_json::json!({ "result": "denied" })).await;
        self.reply_rejected(&peer, EnrollRejectReason::Denied, None, None);
        let by = if caller.local { "local".to_string() } else { caller.device_id.clone() };
        self.end_session(enroll_id, End::Denied { by }).await;
        Ok(())
    }

    /// `node.enroll_code`：主动生成 6 位接入码，5 分钟内本账号任一不请求批准的 `code` 接入可用；新的顶掉旧的。
    pub async fn make_code(&self, caller: &Caller) -> Result<NodeEnrollCodeResult, RpcError> {
        if !self.config.get().enroll.approval {
            return Err(RpcError::new(ErrorCode::Forbidden, "join codes are turned off ([enroll].approval = false)"));
        }
        let now = self.now();
        let code = generate_enroll_code();
        let expires_at_ms = now + CODE_TTL_MS;
        {
            let mut g = self.inner.lock().unwrap();
            g.next_code_id += 1;
            let id = g.next_code_id;
            g.active_code = Some(CodeState {
                id,
                code: Zeroizing::new(code.clone()),
                expires_ms: expires_at_ms,
                attempts_left: CODE_ATTEMPTS,
            });
        }
        self.audit.record(caller, "enroll.code", None, serde_json::json!({})).await;
        Ok(NodeEnrollCodeResult { code, expires_at_ms })
    }

    /// `node.enroll_status`。
    pub async fn status(&self) -> Result<NodeEnrollStatusResult, RpcError> {
        let cfg = self.config.get();
        let row = self
            .store
            .enroll_password_get()
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot read the node password: {e}")))?;
        let now = self.now();
        let password = match row {
            None => EnrollPasswordStatus {
                set: false,
                state: if cfg.enroll.password { EnrollPasswordState::Enabled } else { EnrollPasswordState::Disabled },
                failures: 0,
                retry_after_ms: None,
                set_at_ms: None,
                set_by: None,
            },
            Some(r) => EnrollPasswordStatus {
                set: true,
                state: if !cfg.enroll.password {
                    EnrollPasswordState::Disabled
                } else if r.locked {
                    EnrollPasswordState::Locked
                } else {
                    EnrollPasswordState::Enabled
                },
                failures: r.failures,
                retry_after_ms: (r.next_allowed_at_ms > now && !r.locked).then(|| r.next_allowed_at_ms - now),
                set_at_ms: Some(r.set_at_ms),
                set_by: Some(r.set_by.clone()),
            },
        };
        Ok(NodeEnrollStatusResult { approval: cfg.enroll.approval, password })
    }

    /// `node.set_enroll_password`：设置 / 修改（`Some`）或清除（`None`）。口令至少 8 个字符（NFC 之后）。
    pub async fn set_password(&self, caller: &Caller, password: Option<String>) -> Result<(), RpcError> {
        let by = if caller.local { "local".to_string() } else { caller.device_id.clone() };
        let guard = self.pw_lock.lock().await;
        let previous = self
            .store
            .enroll_password_get()
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot read the node password: {e}")))?;
        let set = match password {
            Some(pw) => {
                let pw = Zeroizing::new(pw);
                if pw.len() > PASSWORD_MAX_BYTES {
                    return Err(RpcError::new(
                        ErrorCode::Invalid,
                        format!("the password is too long (at most {PASSWORD_MAX_BYTES} bytes)"),
                    ));
                }
                validate_enroll_password(&pw)
                    .map_err(|_| RpcError::new(ErrorCode::Invalid, "the password must have at least 8 characters"))?;
                let salt = generate_enroll_salt();
                // Argon2id（64 MiB，约 0.1–0.3 s）：放进阻塞线程
                let prs = {
                    let pw = pw.clone();
                    tokio::task::spawn_blocking(move || derive_enroll_password_prs(&pw, &salt))
                        .await
                        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("password derivation failed: {e}")))?
                        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("password derivation failed: {e}")))?
                };
                // 同一毫秒内连改两次也要有不同的 stamp（进行中的尝试靠它判断口令有没有变）
                let mut set_at_ms = self.now();
                if let Some(p) = &previous {
                    set_at_ms = set_at_ms.max(p.set_at_ms + 1);
                }
                self.store
                    .enroll_password_set(&EnrollPasswordRow {
                        salt,
                        prs,
                        set_at_ms,
                        set_by: by.clone(),
                        failures: 0,
                        next_allowed_at_ms: 0,
                        locked: false,
                    })
                    .await
                    .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot save the node password: {e}")))?;
                true
            }
            None => {
                self.store
                    .enroll_password_clear()
                    .await
                    .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot clear the node password: {e}")))?;
                false
            }
        };
        drop(guard);
        if set {
            self.audit
                .record(caller, "enroll.password_set", None, serde_json::json!({ "changed": previous.is_some() }))
                .await;
        } else {
            self.audit.record(caller, "enroll.password_cleared", None, serde_json::json!({})).await;
        }
        let mut ev = self.event(SecurityEventKind::PasswordChanged);
        ev.by_device = Some(by);
        ev.password_set = Some(set);
        self.security_event(ev).await;
        Ok(())
    }

    /// `node.set_enroll_enabled`：写 `[enroll]`（和 `config set` 同一套写盘），立即热加载；`password: true` 同时解除锁定、失败计数清零。
    pub async fn set_enabled(
        &self,
        caller: &Caller,
        paths: &Paths,
        p: NodeSetEnrollEnabledParams,
    ) -> Result<(), RpcError> {
        if p.approval.is_none() && p.password.is_none() {
            return Err(RpcError::new(ErrorCode::Invalid, "nothing to change: give approval and/or password"));
        }
        let by = if caller.local { "local".to_string() } else { caller.device_id.clone() };
        let mut edits = Vec::new();
        if let Some(a) = p.approval {
            edits.push(("enroll.approval".to_string(), a.to_string()));
        }
        if let Some(a) = p.password {
            edits.push(("enroll.password".to_string(), a.to_string()));
        }
        // 先登记「已经报告过」：下面的热加载会唤醒配置监视，它不能把这次改动再报一遍
        {
            let mut next = self.config.get().enroll.clone();
            if let Some(a) = p.approval {
                next.approval = a;
            }
            if let Some(a) = p.password {
                next.password = a;
            }
            *self.announced_cfg.lock().unwrap() = next;
        }
        crate::config::set_values_blocking(paths, edits)
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot write config.toml: {e}")))?;
        // 重读配置（figment 解析，同步）放进阻塞线程：不占着轮询线程上已经很深的调用栈（debug 下很容易撑爆）
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || config.reload())
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot reload the config: {e}")))?
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot reload the config: {e}")))?;
        let mut reset = false;
        if p.password == Some(true) {
            let _g = self.pw_lock.lock().await;
            reset = self
                .store
                .enroll_password_record(0, 0, false)
                .await
                .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot reset the failure count: {e}")))?;
        }
        self.audit
            .record(
                caller,
                "enroll.settings",
                None,
                serde_json::json!({ "approval": p.approval, "password": p.password, "failures_reset": reset }),
            )
            .await;
        let mut ev = self.event(SecurityEventKind::SettingsChanged);
        ev.by_device = Some(by);
        self.security_event(ev).await;
        Ok(())
    }

    /// `[enroll]` 被直接改了（`anycon config set`、手动编辑 `config.toml`，由配置热加载发现）而不是经 `node.set_enroll_enabled`：
    /// 补报 `settings_changed` 并记审计（docs/spec/daemon.md § 14：`[enroll]` 被修改要通知已配对设备）。
    pub async fn on_config_changed(&self) {
        let now = self.config.get().enroll.clone();
        {
            let mut known = self.announced_cfg.lock().unwrap();
            if *known == now {
                return;
            }
            *known = now.clone();
        }
        let editor =
            Caller { device_id: "local".into(), device_name: "config.toml".into(), kind: DeviceKind::Cli, local: true };
        self.audit
            .record(
                &editor,
                "enroll.settings",
                None,
                serde_json::json!({ "approval": now.approval, "password": now.password, "source": "config.toml" }),
            )
            .await;
        let mut ev = self.event(SecurityEventKind::SettingsChanged);
        ev.by_device = Some("local".into());
        self.security_event(ev).await;
    }

    /// 后台任务：盯着配置，`[enroll]` 变了就补报（见 [`on_config_changed`](Self::on_config_changed)）。
    pub fn spawn_config_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::downgrade(self);
        let mut rx = self.config.subscribe();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(this) = this.upgrade() else { return };
                this.on_config_changed().await;
            }
        })
    }

    // ------------------------------------------------------------ 过期

    /// 收掉过期的会话 / 停滞的尝试 / 过期的主动码。后台每秒调一次，测试里拨快时钟后直接调。
    pub async fn sweep(&self) {
        let now = self.now();
        let (expired, stalled): (Vec<String>, Vec<(String, AttemptKind)>) = {
            let mut g = self.inner.lock().unwrap();
            if g.active_code.as_ref().is_some_and(|c| c.expires_ms <= now) {
                g.active_code = None;
            }
            // 会话过期了，或者停在「等 pair_done」超过看门狗时间（暂存的设备要删掉）
            let expired = g
                .sessions
                .values()
                .filter(|s| {
                    s.expires_ms <= now
                        || s.confirming.as_ref().is_some_and(|c| now.saturating_sub(c.since_ms) >= CONFIRM_TIMEOUT_MS)
                })
                .map(|s| s.id.clone())
                .collect();
            // 停滞的尝试在这把锁里就摘下来，之后的处理不会碰到新开始的那一轮。
            // 接入码发了 `ready`、等份额的阶段不计时（`Attempt::timed`）
            let stalled = g
                .sessions
                .values_mut()
                .filter(|s| s.expires_ms > now)
                .filter_map(|s| {
                    let a = s.attempt.as_ref()?;
                    if !a.timed() || now.saturating_sub(a.started_ms) < ATTEMPT_TIMEOUT_MS {
                        return None;
                    }
                    let kind = a.kind;
                    s.attempt = None;
                    Some((s.id.clone(), kind))
                })
                .collect();
            (expired, stalled)
        };
        for id in expired {
            self.end_session(&id, End::Expired).await;
        }
        for (id, kind) in stalled {
            // 对端不再回话：这一轮算失败。口令 / 扫码是终局；接入码（次数在收到份额时已经扣了）留着会话让它重试
            self.attempt_failed(&id, kind, Fail::Timeout).await;
        }
    }

    /// 后台任务：每秒 [`sweep`](Self::sweep)。
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(this) = this.upgrade() else { return };
                this.sweep().await;
            }
        })
    }

    /// 测试 / 诊断：进行中的会话数（不含桩）。
    pub fn live_count(&self) -> usize {
        self.inner.lock().unwrap().sessions.values().filter(|s| !s.over_limit).count()
    }

    /// doctor：口令的状态。
    pub async fn password_summary(&self) -> Result<NodeEnrollStatusResult, RpcError> {
        self.status().await
    }
}

fn wait_approval_frame() -> EnrollFrame {
    EnrollFrame::Reply {
        status: EnrollReplyStatus::WaitApproval,
        nonce: None,
        reason: None,
        retry_after_ms: None,
        attempts_left: None,
        salt: None,
        cpace: None,
    }
}

fn ready_frame(d_nonce: &[u8; ENROLL_NONCE_LEN], salt: Option<&[u8; 16]>, cpace: Option<&[u8; 32]>) -> EnrollFrame {
    EnrollFrame::Reply {
        status: EnrollReplyStatus::Ready,
        nonce: Some(hex::encode(d_nonce)),
        reason: None,
        retry_after_ms: None,
        attempts_left: None,
        salt: salt.map(hex::encode),
        cpace: cpace.map(hex::encode),
    }
}

// 避免未使用警告：`Zeroize` 用于 `Zeroizing<T>` 的 trait 约束
const _: fn() = || {
    fn assert_zeroize<T: Zeroize>() {}
    assert_zeroize::<Vec<u8>>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_starts_at_the_third_failure_and_doubles() {
        let minutes = |n: u32| password_backoff_ms(n) / 60_000;
        assert_eq!((0..3).map(password_backoff_ms).collect::<Vec<_>>(), vec![0, 0, 0]);
        assert_eq!((3..=9).map(minutes).collect::<Vec<_>>(), vec![1, 2, 4, 8, 16, 32, 64], "2^(n-3) minutes");
        // 不会溢出
        assert!(password_backoff_ms(u32::MAX) > 0);
        assert_eq!(LOCK_AT, 10);
    }

    #[test]
    fn hints_lose_control_and_bidi_characters_and_are_cut_to_64_characters() {
        assert_eq!(clean_text("  Pho\u{1b}[2Jne\u{202e}\u{0}\n ", 64), "Pho[2Jne");
        assert_eq!(clean_text("a\u{200b}b\u{2066}c\u{feff}", 64), "abc");
        assert_eq!(clean_text(&"é".repeat(100), HINT_MAX_CHARS).chars().count(), 64);
        let h = clean_hint(&EnrollHint { name: "\u{1b}\u{7}".into(), kind: DeviceKind::Web, platform: "".into() });
        assert_eq!((h.name.as_str(), h.platform.as_str()), ("unknown device", "unknown"));
    }

    #[test]
    fn the_clock_can_be_moved_forward_for_tests() {
        let c = Clock::default();
        let t = c.now();
        c.advance(60_000);
        assert!(c.now() >= t + 60_000);
    }
}
