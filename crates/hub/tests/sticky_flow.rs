//! 顺序保证（粘住规则）与速率预算的真实栈测试。
//!
//! 起真实 hub，两个假设备经它互发用 `envelope::frame::encode` 切出来的分片。发送方按 docs/protocol.md § 3.1 的规则办事
//! （保留原文、收到 `backpressure` 就暂停、等 `retry_after_ms`、按升序原样重发），接收方慢慢读并用 `Reassembler` 重组。
//! 断言：每一片都到、**严格有序**、重组出来的消息和发出去的逐字节相同；没有一封被静默丢（要么到，要么收到过 `backpressure`）；
//! `rate_limited` 每秒至多一条；粘住状态（被退回 seq 的有序集合）随任一端断开 / 吊销清理，回执晚到时更大的 seq 不能抢在前面，
//! 集合超过 4096 项 1008 断开；只有无视暂停、一直被限速退回的连接才会被 1008 断开。

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Keypair, Role, Transport};
use anycon_hub::config::MIN_ENVELOPE_BYTES;
use anycon_protocol::envelope::frame::{self, DataFrame, Reassembler};
use anycon_protocol::{Control, DeviceKind, DropReason, Envelope, Kind};
use common::*;
use tokio::task::JoinHandle;

const MIB: usize = 1024 * 1024;

/// 确定性的「密文」：每一段内容都不同，分片乱序或重复一定会让拼出来的结果和原文对不上。
fn message(len: usize, salt: u64) -> Vec<u8> {
    let mut x = salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// 真的 Noise IK 传输（和 daemon / 客户端用的是同一个 crate）：隐式递增的 nonce，丢一片、乱序一片都会让解密失败——
/// 「没有 Noise 失步」就是这样被检验的。返回 (发送方的, 接收方的)。
fn noise_pair() -> (Transport, Transport) {
    let (sender_kp, receiver_kp) = (Keypair::generate(), Keypair::generate());
    let receiver_pub = receiver_kp.public;
    let mut init = Handshake::reconnect(Role::Initiator, &sender_kp, Some(&receiver_pub)).unwrap();
    let mut resp = Handshake::reconnect(Role::Responder, &receiver_kp, None).unwrap();
    let m1 = init.write(&[]).unwrap();
    resp.read(&m1).unwrap();
    let m2 = resp.write(&[]).unwrap();
    init.read(&m2).unwrap();
    (init.into_transport().unwrap(), resp.into_transport().unwrap())
}

// ---------------------------------------------------------------------------
// 发送方：照协议办事
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SenderStats {
    /// 收到的 `backpressure` 条数（含被粘住而退回的）。
    backpressure: usize,
    /// 收到 `rate_limited` 的时刻和提示。
    rate_limited: Vec<(Instant, u64)>,
    /// 重发了多少次（按封算）。
    resent: usize,
    /// 不该出现的回执（`dropped`、`undeliverable`……）。
    unexpected: Vec<Control>,
}

struct Sender {
    dev: FakeDevice,
    to: String,
    /// 按发出顺序的 seq。
    sent: Vec<u64>,
    /// seq -> 信封原文（重发用，不会重新编码）。
    retained: BTreeMap<u64, Vec<u8>>,
    returned: BTreeSet<u64>,
    /// 最近一次 `rate_limited` 叫停到什么时刻。
    resume_at: Instant,
    stats: SenderStats,
}

impl Sender {
    fn new(dev: FakeDevice, to: &str) -> Self {
        Self {
            dev,
            to: to.to_string(),
            sent: Vec::new(),
            retained: BTreeMap::new(),
            returned: BTreeSet::new(),
            resume_at: Instant::now(),
            stats: SenderStats::default(),
        }
    }

    async fn send(&mut self, kind: Kind, body: Vec<u8>) -> u64 {
        let seq = self.dev.seq + 1;
        self.dev.seq = seq;
        let bytes = Envelope::new(self.dev.id.clone(), self.to.clone(), kind, seq, body).encode().unwrap();
        self.retained.insert(seq, bytes.clone());
        self.sent.push(seq);
        self.dev.send_raw(bytes).await;
        seq
    }

    /// 一条消息（密文）：切成 `WHOLE` / `FRAGMENT` 的若干个 `data` 信封，连续发出，不等回执。
    async fn send_message(&mut self, ciphertext: &[u8], msg_id: u32) -> usize {
        let bodies = frame::encode(ciphertext, msg_id);
        let n = bodies.len();
        for b in bodies {
            self.send(Kind::Data, b).await;
        }
        n
    }

    fn absorb(&mut self, env: Envelope) {
        assert_eq!(env.kind, Kind::Control, "the sender only expects control messages, got {env:?}");
        match env.control_body().unwrap() {
            Control::Backpressure { to, seq } => {
                assert_eq!(to, self.to);
                self.stats.backpressure += 1;
                self.returned.insert(seq);
            }
            Control::RateLimited { retry_after_ms } => {
                let now = Instant::now();
                self.stats.rate_limited.push((now, retry_after_ms));
                self.resume_at = self.resume_at.max(now + Duration::from_millis(retry_after_ms));
            }
            Control::PeerOnline { .. } | Control::PeerOffline { .. } => {}
            other => self.stats.unexpected.push(other),
        }
    }

    /// 读 `dur`（把期间到的回执都消化掉）。连接被关闭时返回 `Err(close code)`。
    async fn pump(&mut self, dur: Duration) -> Result<(), Option<u16>> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.dev.recv_within(left).await {
                None => return Ok(()),
                Some(Recv::Env(e)) => self.absorb(e),
                Some(Recv::Closed(code)) => return Err(code),
            }
        }
    }

    /// 重发规则：有被退回的就暂停（`rate_limited` 给了时间就等够，没有就 100–300 ms），
    /// 然后把所有被退回的 seq 按升序原样重发；重发期间又被退回就再来一轮；一段时间没有新的退回就算完成。
    async fn settle(&mut self) -> Result<(), Option<u16>> {
        let mut round = 0u32;
        loop {
            self.pump(Duration::from_millis(50)).await?;
            if self.returned.is_empty() {
                // 安静观察一会儿：重发出去的东西如果又被退回，回执会在这段时间里到
                self.pump(Duration::from_millis(500)).await?;
                if self.returned.is_empty() {
                    return Ok(());
                }
            }
            round += 1;
            let default_pause = Duration::from_millis(100 + u64::from(round % 5) * 50);
            let until = self.resume_at.max(Instant::now() + default_pause);
            // 等的同时继续收回执（飞行中的信封还会陆续被退回）
            self.pump(until.saturating_duration_since(Instant::now())).await?;
            let batch = std::mem::take(&mut self.returned);
            for seq in batch {
                self.stats.resent += 1;
                self.dev.send_raw(self.retained[&seq].clone()).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 接收方：慢慢读 + Reassembler
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Received {
    /// 收到的每个 `data` 信封的 seq，按到达顺序。
    seqs: Vec<u64>,
    messages: Vec<Vec<u8>>,
    /// 重组器报的错（有序的话应该一个都没有）。
    frame_errors: Vec<String>,
    /// 完成后还多收到的信封（重复）。
    extra: usize,
}

/// `noise`：给了就把重组出来的密文用它解密（`messages` 里是明文）；不给就原样收下（`messages` 是密文）。
fn spawn_receiver(
    mut dev: FakeDevice,
    from: String,
    expect_messages: usize,
    first_delay: Duration,
    per_frame: Duration,
    mut noise: Option<Transport>,
) -> JoinHandle<Received> {
    tokio::spawn(async move {
        let mut got = Received::default();
        let mut reasm = Reassembler::new();
        tokio::time::sleep(first_delay).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            // 收齐之后再多等一会儿，看有没有重复的片
            let idle = if got.messages.len() >= expect_messages {
                Duration::from_millis(700)
            } else {
                Duration::from_secs(20)
            };
            let left = deadline.saturating_duration_since(tokio::time::Instant::now()).min(idle);
            match dev.recv_within(left).await {
                None => break,
                Some(Recv::Closed(code)) => {
                    got.frame_errors.push(format!("receiver connection closed: {code:?}"));
                    break;
                }
                Some(Recv::Env(e)) if e.kind == Kind::Data => {
                    assert_eq!(e.from, from);
                    if got.messages.len() >= expect_messages {
                        got.extra += 1;
                        continue;
                    }
                    got.seqs.push(e.seq);
                    match frame::decode(&e.body) {
                        Ok(f) => {
                            let f: DataFrame<'_> = f;
                            match reasm.push(f) {
                                Ok(Some(ct)) => match noise.as_mut() {
                                    None => got.messages.push(ct),
                                    Some(t) => match t.decrypt(&ct) {
                                        Ok(plain) => got.messages.push(plain),
                                        Err(err) => {
                                            got.frame_errors.push(format!("noise desync at seq {}: {err}", e.seq))
                                        }
                                    },
                                },
                                Ok(None) => {}
                                Err(err) => {
                                    got.frame_errors.push(format!("seq {}: {err}", e.seq));
                                    reasm.clear();
                                }
                            }
                        }
                        Err(err) => got.frame_errors.push(format!("seq {}: {err}", e.seq)),
                    }
                    tokio::time::sleep(per_frame).await;
                }
                Some(Recv::Env(_)) => {} // peer_online 等
            }
        }
        got
    })
}

// ---------------------------------------------------------------------------
// 一次完整的「发送方 → hub → 慢接收方」
// ---------------------------------------------------------------------------

struct Flow {
    hub: TestHub,
    sender: Sender,
    received: Received,
    messages: Vec<Vec<u8>>,
    frames: usize,
    elapsed: Duration,
}

impl Flow {
    /// 收尾断言（所有流量共有的）：每一片都到、严格有序、消息逐字节一致、没有静默丢弃的迹象。
    fn assert_complete_and_ordered(&self) {
        let r = &self.received;
        assert!(r.frame_errors.is_empty(), "the reassembler saw trouble: {:?}", r.frame_errors);
        assert_eq!(r.extra, 0, "duplicate envelopes arrived");
        assert_eq!(r.seqs.len(), self.frames, "every fragment must arrive");
        assert_eq!(
            r.seqs, self.sender.sent,
            "fragments must arrive exactly once each, strictly in the order they were sent"
        );
        assert_eq!(r.messages.len(), self.messages.len());
        for (i, (got, want)) in r.messages.iter().zip(&self.messages).enumerate() {
            assert!(got == want, "message {i} was corrupted ({} vs {} bytes)", got.len(), want.len());
        }
        let s = &self.sender.stats;
        assert!(s.unexpected.is_empty(), "nothing may be dropped or undeliverable: {:?}", s.unexpected);
        assert!(self.sender.returned.is_empty());
        // 粘住记录随着最后一次重发成功而清空
        assert_eq!(self.hub.handle.state.registry.sticky().len(), 0, "{:?}", self.hub.handle.state.registry);
    }

    /// 人看的一行摘要（`cargo test -- --nocapture` 时可见）。
    fn report(&self, label: &str) {
        let s = &self.sender.stats;
        println!(
            "{label}: {} fragments, {} messages, {:?}; backpressure notices {}, resent {}, rate_limited {:?}",
            self.frames,
            self.messages.len(),
            self.elapsed,
            s.backpressure,
            s.resent,
            s.rate_limited.iter().map(|(_, ms)| *ms).collect::<Vec<_>>()
        );
    }

    /// `rate_limited` 每秒至多一条（接收时刻有一点抖动，留 10% 余量），提示至少 100 ms。
    fn assert_rate_limited_notices_are_spaced(&self) {
        let n = &self.sender.stats.rate_limited;
        for w in n.windows(2) {
            let gap = w[1].0.duration_since(w[0].0);
            assert!(gap >= Duration::from_millis(900), "two rate_limited notices {gap:?} apart: {n:?}");
        }
        assert!(
            n.iter().all(|(_, ms)| (100..=30_000).contains(ms)),
            "retry_after_ms is between 100 ms and 30 s (the client caps it at 30 s): {n:?}"
        );
    }
}

struct FlowSpec {
    sender_kind: DeviceKind,
    receiver_kind: DeviceKind,
    sizes: Vec<usize>,
    first_delay: Duration,
    per_frame: Duration,
}

async fn run_flow(hub: TestHub, spec: FlowSpec) -> Flow {
    let a = hub.seed_device(spec.sender_kind, "sender").await;
    let b = hub.seed_device(spec.receiver_kind, "receiver").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), spec.sender_kind, "sender").await;
    let db = FakeDevice::connect(&hub, Some(&b.token), spec.receiver_kind, "receiver").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));

    let messages: Vec<Vec<u8>> = spec.sizes.iter().enumerate().map(|(i, n)| message(*n, i as u64 + 1)).collect();
    let (mut tx, rx) = noise_pair();
    let receiver = spawn_receiver(db, a.id.clone(), messages.len(), spec.first_delay, spec.per_frame, Some(rx));
    // 先按顺序加密好（Noise 的 nonce 按加密顺序递增；debug 构建里加密 30 MiB 要几秒，不计入下面的计时），再切片灌出去
    let ciphertexts: Vec<Vec<u8>> = messages.iter().map(|m| tx.encrypt(m).expect("encrypt")).collect();
    let mut sender = Sender::new(da, &b.id);
    let started = Instant::now();
    let mut frames = 0;
    for (i, ct) in ciphertexts.iter().enumerate() {
        frames += sender.send_message(ct, i as u32 + 1).await;
    }
    if let Err(code) = sender.settle().await {
        panic!("the sender's connection was closed ({code:?}) before everything was delivered");
    }
    let received = tokio::time::timeout(Duration::from_secs(60), receiver)
        .await
        .expect("the receiver finishes")
        .expect("receiver task");
    let elapsed = started.elapsed();
    Flow { hub, sender, received, messages, frames, elapsed }
}

// ---------------------------------------------------------------------------
// (a) 5 MiB 和 9 MiB 的分片消息，接收端慢读
// ---------------------------------------------------------------------------

#[tokio::test]
async fn five_and_nine_mib_messages_arrive_in_order_through_the_4_mib_buffer_with_a_slow_reader() {
    // 默认配置：4 MiB 发送缓冲，daemon 100 MiB / 分钟
    let hub = TestHub::start().await;
    assert_eq!(hub.handle.state.config.outbound_buffer_bytes, 4 * MIB);
    let sizes = vec![5 * MIB, 9 * MIB + 12_345];
    // 5 MiB 切成 ≥ 25 片，9 MiB 切成 ≥ 45 片
    let pieces: Vec<usize> = sizes.iter().map(|n| n.div_ceil(frame::MAX_FRAGMENT)).collect();
    assert!(pieces[0] >= 25 && pieces[1] >= 45, "{pieces:?}");

    let flow = run_flow(
        hub,
        FlowSpec {
            sender_kind: DeviceKind::Daemon,
            receiver_kind: DeviceKind::Web,
            sizes,
            first_delay: Duration::from_millis(400),
            per_frame: Duration::from_millis(8),
        },
    )
    .await;
    assert!(flow.frames >= pieces.iter().sum::<usize>(), "{} fragments", flow.frames);
    flow.report("flow");
    flow.assert_complete_and_ordered();
    let s = &flow.sender.stats;
    assert!(s.backpressure > 0, "the 4 MiB buffer must actually have pushed back, or this test proves nothing");
    assert!(s.resent > 0);
    assert!(s.rate_limited.is_empty(), "14 MiB is far below the daemon budget: {:?}", s.rate_limited);
}

// ---------------------------------------------------------------------------
// (b) daemon 灌 30 MiB
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thirty_mib_from_a_daemon_to_a_slow_reader_loses_nothing_and_stays_connected() {
    let hub = TestHub::start().await;
    let flow = run_flow(
        hub,
        FlowSpec {
            sender_kind: DeviceKind::Daemon,
            receiver_kind: DeviceKind::Web,
            sizes: vec![10 * MIB, 10 * MIB, 10 * MIB],
            first_delay: Duration::from_millis(300),
            per_frame: Duration::from_millis(3),
        },
    )
    .await;
    assert!(flow.frames >= 150, "{} fragments", flow.frames);
    flow.report("flow");
    flow.assert_complete_and_ordered();
    assert!(flow.sender.stats.backpressure > 0, "a slow reader must have pushed back");
    assert!(flow.sender.stats.rate_limited.is_empty(), "30 MiB is under the daemon budget of 100 MiB / min");
    // 节点仍在线：照常办事
    let mut d = flow.sender.dev;
    let (_, match_id) = new_code();
    assert!(d.try_send_control(&Control::PairBegin { match_id }).await);
    loop {
        match d.recv_control().await {
            Control::PairRegistered { .. } => break,
            Control::Backpressure { .. } | Control::RateLimited { .. } => continue,
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn thirty_mib_over_a_lower_daemon_budget_is_throttled_not_dropped() {
    // 把 daemon 预算降到 20 MiB / 分钟（桶容量 20 MiB，回填约 0.33 MiB / 秒）：灌 30 MiB 会撞上它，多出来的要等额度慢慢回来（约 30 秒）。
    // 传输耗时 T 秒时桶里最多能过 20 + 0.33·T MiB，要小于 30 MiB，即 T < 30 s：共享的 CI 机器上实测 T 约 10 s，
    // 预算取 27 MiB 时上限只有约 7 s，补充量就把余下的数据全放过去了，预算根本没被打到。
    let hub = TestHub::start_with(|c| c.limits.daemon_bytes_per_min = 20 * MIB as u64).await;
    let flow = run_flow(
        hub,
        FlowSpec {
            sender_kind: DeviceKind::Daemon,
            receiver_kind: DeviceKind::Web,
            sizes: vec![10 * MIB, 10 * MIB, 10 * MIB],
            first_delay: Duration::from_millis(100),
            per_frame: Duration::ZERO,
        },
    )
    .await;
    flow.report("flow");
    flow.assert_complete_and_ordered();
    let s = &flow.sender.stats;
    assert!(!s.rate_limited.is_empty(), "the budget must actually have been hit");
    assert!(s.backpressure > 0 && s.resent > 0);
    flow.assert_rate_limited_notices_are_spaced();
    assert!(flow.elapsed >= Duration::from_secs(2), "the last few MiB had to wait for the budget: {:?}", flow.elapsed);
}

// ---------------------------------------------------------------------------
// (c) 客户端种类：10 MiB / 分钟
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_client_over_its_10_mib_per_minute_budget_is_throttled_not_dropped_while_a_daemon_is_not() {
    let hub = TestHub::start().await;
    assert_eq!(hub.handle.state.config.limits.client_bytes_per_min, 10 * MIB as u64);
    assert_eq!(hub.handle.state.config.limits.daemon_bytes_per_min, 100 * MIB as u64);
    // web 客户端发 11 MiB：超出 1 MiB，要等额度（10 MiB / 分钟 ≈ 175 KiB / 秒）回来
    let flow = run_flow(
        hub,
        FlowSpec {
            sender_kind: DeviceKind::Web,
            receiver_kind: DeviceKind::Daemon,
            sizes: vec![11 * MIB],
            first_delay: Duration::ZERO,
            per_frame: Duration::ZERO,
        },
    )
    .await;
    flow.report("flow");
    flow.assert_complete_and_ordered();
    let s = &flow.sender.stats;
    assert!(!s.rate_limited.is_empty(), "11 MiB is over the client budget");
    assert!(s.backpressure > 0 && s.resent > 0);
    flow.assert_rate_limited_notices_are_spaced();
    assert!(flow.elapsed >= Duration::from_secs(3), "{:?}", flow.elapsed);

    // 同一个 hub 上，daemon 种类发同样多的数据：不会碰到速率限制
    let hub = flow.hub;
    let flow = run_flow(
        hub,
        FlowSpec {
            sender_kind: DeviceKind::Daemon,
            receiver_kind: DeviceKind::Web,
            sizes: vec![11 * MIB],
            first_delay: Duration::ZERO,
            per_frame: Duration::ZERO,
        },
    )
    .await;
    flow.report("flow");
    flow.assert_complete_and_ordered();
    assert!(flow.sender.stats.rate_limited.is_empty(), "{:?}", flow.sender.stats.rate_limited);
}

// ---------------------------------------------------------------------------
// (d) 粘住状态随任一端断开 / 吊销清理
// ---------------------------------------------------------------------------

/// 让 `a -> peer` 这一对确定地被粘住：每秒信封数上限只有 2，连发 6 封，后面的被退回，第一封被退回的 seq 记下来。
async fn make_stuck(a: &mut FakeDevice, peer: &str) -> u64 {
    let mut first_returned = None;
    for _ in 0..6 {
        a.send_env(peer, Kind::Data, vec![0u8; 16]).await;
    }
    for c in a.drain_controls(Duration::from_millis(400)).await {
        if let Control::Backpressure { to, seq } = c {
            assert_eq!(to, peer);
            first_returned.get_or_insert(seq);
        }
    }
    first_returned.expect("some envelopes were handed back")
}

async fn wait_sticky(hub: &TestHub, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = hub.handle.state.registry.sticky().len();
        if n == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sticky entries: {n}, expected {expected} ({:?})",
            hub.handle.state.registry
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stuck_hub() -> (TestHub, Seeded, Seeded, Seeded) {
    let hub = TestHub::start_with(|c| c.limits.conn_envelopes_per_sec = 2).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    let c = hub.seed_device(DeviceKind::Web, "c").await;
    hub.seed_paired(&a, &b).await;
    hub.seed_paired(&a, &c).await;
    (hub, a, b, c)
}

#[tokio::test]
async fn sticky_state_is_dropped_when_the_target_disconnects() {
    let (hub, a, b, c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    let _dc = FakeDevice::connect(&hub, Some(&c.token), DeviceKind::Web, "c").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    tokio::time::sleep(Duration::from_millis(1100)).await; // 新的一秒，额度窗口恢复
    make_stuck(&mut da, &c.id).await;
    wait_sticky(&hub, 2).await;
    assert!(format!("{:?}", hub.handle.state.registry).contains("entries: 2"));

    // b 下线：只清掉发给 b 的那条，发给 c 的还在
    drop(db);
    wait_sticky(&hub, 1).await;
}

#[tokio::test]
async fn sticky_state_is_dropped_when_the_sender_disconnects_and_a_reconnect_starts_clean() {
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    drop(da);
    wait_sticky(&hub, 0).await;

    // 新连接是一条全新的传输：seq 从头开始，也不受旧的粘住影响（先把 b 收到的前两封和上下线通知读掉）
    while db.recv_within(Duration::from_millis(200)).await.is_some() {}
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut da2 = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    da2.drain_controls(Duration::from_millis(100)).await;
    let seq = da2.send_env(&b.id, Kind::Data, vec![7u8; 16]).await;
    assert_eq!(seq, 2, "a new connection numbers its envelopes from scratch (auth was seq 1)");
    let delivered = loop {
        let e = db.recv_env().await;
        if e.kind == Kind::Data {
            break e;
        }
    };
    assert_eq!((delivered.seq, delivered.body), (2, vec![7u8; 16]));
    wait_sticky(&hub, 0).await;
}

#[tokio::test]
async fn sticky_state_is_dropped_when_the_target_is_revoked() {
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // a 吊销 b（同一账号，已配对）
    da.send_control(&Control::Revoke { device: b.id.clone() }).await;
    let close = loop {
        match db.recv().await {
            Recv::Closed(code) => break code,
            Recv::Env(_) => continue, // 先到的前两封
        }
    };
    assert_eq!(close, Some(4004));
    wait_sticky(&hub, 0).await;
}

#[tokio::test]
async fn sticky_state_is_dropped_when_the_target_is_superseded_by_a_new_connection() {
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let _db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    // b 用同一个 token 再连一次，旧连接被顶掉（4001）
    let mut db2 = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    wait_sticky(&hub, 0).await;
    // 新连接一上来就能收到 a 的新信封，不会因为旧连接上被退回的 seq 而被挡住
    tokio::time::sleep(Duration::from_millis(1100)).await;
    da.drain_controls(Duration::from_millis(100)).await;
    let seq = da.send_env(&b.id, Kind::Data, vec![5u8; 8]).await;
    let got = loop {
        let e = db2.recv_env().await;
        if e.kind == Kind::Data {
            break e;
        }
    };
    assert_eq!(got.seq, seq);
}

// ---------------------------------------------------------------------------
// 握手解除粘住
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_forwarded_handshake_releases_a_stuck_pair() {
    // 被退回的那封发送方可能已经不要了（自己的重发缓冲里没有）：它的出路是丢弃传输、重新握手。
    // 握手被转发之后旧的顺序约束作废，否则这一对会永远卡死。
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // 新的一秒里：data 仍被粘住（不是被退回的那个 seq）
    let s = da.send_env(&b.id, Kind::Data, vec![1u8; 8]).await;
    assert!(matches!(da.recv_control().await, Control::Backpressure { seq, .. } if seq == s));
    // 握手被转发
    let h = da.send_env(&b.id, Kind::Handshake, vec![9u8; 8]).await;
    wait_sticky(&hub, 0).await;
    let seen: Vec<Envelope> = {
        let mut v = Vec::new();
        while let Some(Recv::Env(e)) = db.recv_within(Duration::from_millis(300)).await {
            if e.kind == Kind::Handshake {
                v.push(e);
            }
        }
        v
    };
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].seq, h);
    // 之后的 data 又能过了（等新的一秒，每秒信封数的额度才够）
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let s2 = da.send_env(&b.id, Kind::Data, vec![2u8; 8]).await;
    let got = loop {
        let e = db.recv_env().await;
        if e.kind == Kind::Data {
            break e;
        }
    };
    assert_eq!(got.seq, s2);
}

async fn recv_kind(dev: &mut FakeDevice, kind: Kind) -> Envelope {
    loop {
        let e = dev.recv_env().await;
        if e.kind == kind {
            return e;
        }
    }
}

/// 反方向的握手同样解除：A→B 的 data 被粘住，B 发起新的握手（B→A）也要把 A→B 的约束清掉。
#[tokio::test]
async fn a_handshake_in_the_opposite_direction_also_releases_a_stuck_pair() {
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    // b 重新握手（b -> a）
    db.send_env(&a.id, Kind::Handshake, vec![4u8; 8]).await;
    wait_sticky(&hub, 0).await;
    assert_eq!(recv_kind(&mut da, Kind::Handshake).await.from, b.id);
    // a 的新 seq 又能到 b
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let s = da.send_env(&b.id, Kind::Data, vec![6u8; 8]).await;
    // b 先读到的是粘住之前就到了的那两封
    let mut seen = Vec::new();
    loop {
        let seq = recv_kind(&mut db, Kind::Data).await.seq;
        seen.push(seq);
        if seq == s {
            break;
        }
    }
    assert_eq!(seen.len(), 3, "the two that arrived before, then the new one: {seen:?}");
}

/// 握手永远不被粘住拦住、也不会造成粘住：超速时它和 data 一样被退回（`backpressure`），但不留任何记录。
#[tokio::test]
async fn handshakes_neither_set_nor_obey_the_sticky_state() {
    let (hub, a, b, _c) = stuck_hub().await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    // 每秒只放 2 个：连发 5 个握手，3 个被退回，但不留粘住记录
    let mut seqs = Vec::new();
    for _ in 0..5 {
        seqs.push(da.send_env(&b.id, Kind::Handshake, vec![1u8; 8]).await);
    }
    let returned: Vec<u64> = da
        .drain_controls(Duration::from_millis(400))
        .await
        .into_iter()
        .filter_map(|c| match c {
            Control::Backpressure { seq, .. } => Some(seq),
            _ => None,
        })
        .collect();
    assert_eq!(returned, seqs[2..].to_vec());
    assert_eq!(hub.handle.state.registry.sticky().len(), 0);
    // 另一个方向、另一个种类被粘住时，握手照常转发（不看 blocked）
    tokio::time::sleep(Duration::from_millis(1100)).await;
    make_stuck(&mut da, &b.id).await;
    wait_sticky(&hub, 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let h = da.send_env(&b.id, Kind::Handshake, vec![2u8; 8]).await;
    let mut got = Vec::new();
    while let Some(Recv::Env(e)) = db.recv_within(Duration::from_millis(300)).await {
        if e.kind == Kind::Handshake {
            got.push(e.seq);
        }
    }
    assert!(got.contains(&h), "{got:?}");
}

/// 真的 Noise IK：传输被重置之后，发送方不会再重发旧传输上被退回的那封，所以新的握手必须能解除粘住，
/// 新传输上的 seq 才能正常转发并被解密。`initiator_is_sender`：握手由数据的发送方发起，还是由接收方发起（反方向）。
async fn noise_reset_after_stuck(initiator_is_sender: bool) {
    let (hub, x, y, _c) = stuck_hub().await;
    let mut dx = FakeDevice::connect(&hub, Some(&x.token), DeviceKind::Daemon, "x").await;
    let mut dy = FakeDevice::connect(&hub, Some(&y.token), DeviceKind::Web, "y").await;
    dx.drain_controls(Duration::from_millis(100)).await;
    let (kp_x, kp_y) = (Keypair::generate(), Keypair::generate());

    // 旧传输：由 y 发起 IK（客户端发起）。x 用 responder 一侧加密，y 解密。
    let handshake = |init: &Keypair, resp: &Keypair| {
        let mut i = Handshake::reconnect(Role::Initiator, init, Some(&resp.public)).unwrap();
        let mut r = Handshake::reconnect(Role::Responder, resp, None).unwrap();
        let m1 = i.write(&[]).unwrap();
        r.read(&m1).unwrap();
        let m2 = r.write(&[]).unwrap();
        i.read(&m2).unwrap();
        (i.into_transport().unwrap(), r.into_transport().unwrap())
    };
    let (mut ty_old, mut tx_old) = handshake(&kp_y, &kp_x);
    // x 连发 6 条（每条一个 WHOLE 帧）：每秒只放 2 个，前 2 条到达 y，第 3 条起被退回并粘住——x 不再重发它们
    let mut sent_plain = Vec::new();
    for i in 0..6u8 {
        let plain = vec![i; 100];
        let ct = tx_old.encrypt(&plain).unwrap();
        let body = frame::encode(&ct, 1).remove(0);
        dx.send_env(&y.id, Kind::Data, body).await;
        sent_plain.push(plain);
    }
    let mut delivered = 0;
    while let Some(Recv::Env(e)) = dy.recv_within(Duration::from_millis(400)).await {
        if e.kind != Kind::Data {
            continue;
        }
        let DataFrame::Whole(ct) = frame::decode(&e.body).unwrap() else { panic!("expected a whole frame") };
        assert_eq!(ty_old.decrypt(ct).unwrap(), sent_plain[delivered]);
        delivered += 1;
    }
    assert_eq!(delivered, 2);
    wait_sticky(&hub, 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // 新的 IK 握手。第一条消息的发起方：x（数据发送方）或 y（反方向）
    let (tx_new, mut ty_new) = if initiator_is_sender {
        let mut i = Handshake::reconnect(Role::Initiator, &kp_x, Some(&kp_y.public)).unwrap();
        let mut r = Handshake::reconnect(Role::Responder, &kp_y, None).unwrap();
        dx.send_env(&y.id, Kind::Handshake, i.write(&[]).unwrap()).await;
        r.read(&recv_kind(&mut dy, Kind::Handshake).await.body).unwrap();
        wait_sticky(&hub, 0).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        dy.send_env(&x.id, Kind::Handshake, r.write(&[]).unwrap()).await;
        i.read(&recv_kind(&mut dx, Kind::Handshake).await.body).unwrap();
        (i.into_transport().unwrap(), r.into_transport().unwrap())
    } else {
        let mut i = Handshake::reconnect(Role::Initiator, &kp_y, Some(&kp_x.public)).unwrap();
        let mut r = Handshake::reconnect(Role::Responder, &kp_x, None).unwrap();
        dy.send_env(&x.id, Kind::Handshake, i.write(&[]).unwrap()).await;
        r.read(&recv_kind(&mut dx, Kind::Handshake).await.body).unwrap();
        wait_sticky(&hub, 0).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        dx.send_env(&y.id, Kind::Handshake, r.write(&[]).unwrap()).await;
        i.read(&recv_kind(&mut dy, Kind::Handshake).await.body).unwrap();
        (r.into_transport().unwrap(), i.into_transport().unwrap())
    };
    let mut tx_new = tx_new;
    // 新传输上 x 的新 seq 正常转发，y 能解密
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let plain = message(1000, 77);
    let ct = tx_new.encrypt(&plain).unwrap();
    let seq = dx.send_env(&y.id, Kind::Data, frame::encode(&ct, 2).remove(0)).await;
    let got = recv_kind(&mut dy, Kind::Data).await;
    assert_eq!(got.seq, seq);
    let DataFrame::Whole(ct) = frame::decode(&got.body).unwrap() else { panic!("expected a whole frame") };
    assert_eq!(ty_new.decrypt(ct).unwrap(), plain);
    assert_eq!(hub.handle.state.registry.sticky().len(), 0);
}

#[tokio::test]
async fn a_new_noise_handshake_from_the_sender_unsticks_the_pair_and_the_new_transport_works() {
    noise_reset_after_stuck(true).await;
}

#[tokio::test]
async fn a_new_noise_handshake_from_the_receiver_unsticks_the_pair_and_the_new_transport_works() {
    noise_reset_after_stuck(false).await;
}

// ---------------------------------------------------------------------------
// 被退回的 seq 是一个有序集合，回执晚到时更大的 seq 也不能抢在前面
// ---------------------------------------------------------------------------

/// 收 `dur` 内到的所有 `data` 信封的 seq（按到达顺序）。
async fn collect_data_seqs(dev: &mut FakeDevice, dur: Duration) -> Vec<u64> {
    let mut seqs = Vec::new();
    while let Some(r) = dev.recv_within(dur).await {
        match r {
            Recv::Env(e) if e.kind == Kind::Data => seqs.push(e.seq),
            Recv::Env(_) => {}
            Recv::Closed(code) => panic!("connection closed ({code:?})"),
        }
    }
    seqs
}

/// 发一封指定 seq 的 `data`（重发用，内容和第一次相同）。
async fn resend(dev: &mut FakeDevice, to: &str, seq: u64, body: &[u8]) {
    let bytes = Envelope::new(dev.id.clone(), to, Kind::Data, seq, body.to_vec()).encode().unwrap();
    dev.send_raw(bytes).await;
}

fn backpressure_seqs(controls: Vec<Control>) -> Vec<u64> {
    controls
        .into_iter()
        .filter_map(|c| match c {
            Control::Backpressure { seq, .. } => Some(seq),
            _ => None,
        })
        .collect()
}

/// 报告里的序列，在真 hub 上：几封被退回、其中一封的回执（发送方还不知道）没到，发送方先重发它已知的，
/// 目标这时装得下、速率额度也够——更大的 seq 也必须被退回，不能抢在欠着的那封前面。
/// 旧实现（只记第一个被退回的 seq，放行就整条清掉）在这里放行 s3, s5, ...，接收方看到乱序。
#[tokio::test]
async fn a_larger_seq_may_not_overtake_a_returned_one_whose_receipt_has_not_arrived() {
    let (hub, a, b, _c) = stuck_hub().await; // 每秒只放 2 个信封
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;
    let body = |i: usize| vec![i as u8; 32];

    // 第一秒：连发 6 封。前 2 封到达，后 4 封被退回，全部记进集合
    let mut seqs = Vec::new();
    for i in 0..6 {
        seqs.push(da.send_env(&b.id, Kind::Data, body(i)).await);
    }
    assert_eq!(backpressure_seqs(da.drain_controls(Duration::from_millis(400)).await), seqs[2..].to_vec());
    assert_eq!(collect_data_seqs(&mut db, Duration::from_millis(300)).await, seqs[..2].to_vec());
    wait_sticky(&hub, 1).await;
    assert_eq!(hub.handle.state.registry.sticky().seq_count(), 4);

    // 第二秒：发送方只知道 s3 和 s5 被退回了（s4 的回执还在路上）。s3 是集合的最小值，放行；
    // s5 不是，额度和缓冲都装得下也必须退回
    tokio::time::sleep(Duration::from_millis(1100)).await;
    resend(&mut da, &b.id, seqs[2], &body(2)).await;
    resend(&mut da, &b.id, seqs[4], &body(4)).await;
    assert_eq!(backpressure_seqs(da.drain_controls(Duration::from_millis(400)).await), vec![seqs[4]]);
    assert_eq!(collect_data_seqs(&mut db, Duration::from_millis(300)).await, vec![seqs[2]], "s5 must not get through");
    assert_eq!(hub.handle.state.registry.sticky().seq_count(), 3, "s4, s5, s6 are still owed");

    // 第三秒：回执到齐，升序重发 s4, s5，放行；s6 这一秒的额度不够，被退回
    tokio::time::sleep(Duration::from_millis(1100)).await;
    for i in [3, 4, 5] {
        resend(&mut da, &b.id, seqs[i], &body(i)).await;
    }
    assert_eq!(backpressure_seqs(da.drain_controls(Duration::from_millis(400)).await), vec![seqs[5]]);
    assert_eq!(collect_data_seqs(&mut db, Duration::from_millis(300)).await, vec![seqs[3], seqs[4]]);

    // 第四秒：s6 放行，集合排空
    tokio::time::sleep(Duration::from_millis(1100)).await;
    resend(&mut da, &b.id, seqs[5], &body(5)).await;
    assert_eq!(collect_data_seqs(&mut db, Duration::from_millis(300)).await, vec![seqs[5]]);
    assert!(da.drain_controls(Duration::from_millis(200)).await.is_empty());
    wait_sticky(&hub, 0).await;
    assert_eq!(hub.handle.state.registry.sticky().seq_count(), 0);
}

/// 集合上限 4096 项：无视回执一直发新的信封，被退回的 seq 攒过上限，就以 1008 断开发送方（回执都在 Close 之前）。
#[tokio::test]
async fn a_sender_with_more_than_4096_returned_seqs_outstanding_is_closed_with_1008() {
    use anycon_hub::ws::sticky::MAX_RETURNED;
    // 每秒只放 1 个：第 1 封转发，之后的全被退回并记入集合
    let hub = TestHub::start_with(|c| c.limits.conn_envelopes_per_sec = 1).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let _db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;

    let mut returned = BTreeSet::new();
    let mut close = None;
    let started = Instant::now();
    'send: for batch in 0..(MAX_RETURNED + 200).div_ceil(64) {
        assert!(started.elapsed() < Duration::from_secs(20), "never closed (batch {batch})");
        for _ in 0..64 {
            if !da.try_send_env(&b.id, Kind::Data, vec![0u8; 8]).await {
                break 'send;
            }
        }
        // 边发边读回执：不读的话控制消息队列先满，发送方会因为 4003 被断开，测不到集合上限
        while let Some(r) = da.recv_within(Duration::from_millis(1)).await {
            match r {
                Recv::Env(e) => {
                    if let Ok(Control::Backpressure { seq, .. }) = e.control_body() {
                        returned.insert(seq);
                    }
                }
                Recv::Closed(code) => {
                    close = Some(code);
                    break 'send;
                }
            }
        }
    }
    // 收尾：把还在路上的回执和 Close 读完
    while close.is_none() {
        match da.recv_within(Duration::from_secs(3)).await {
            Some(Recv::Env(e)) => {
                if let Ok(Control::Backpressure { seq, .. }) = e.control_body() {
                    returned.insert(seq);
                }
            }
            Some(Recv::Closed(code)) => close = Some(code),
            None => panic!("the sender was never closed; {} receipts", returned.len()),
        }
    }
    assert_eq!(close, Some(Some(1008)), "policy violation, not slow consumer");
    assert_eq!(
        returned.len(),
        MAX_RETURNED,
        "exactly the 4096 that fit were returned; the one that did not fit closed the connection"
    );
    wait_registry_gone(&hub, &a.id).await;
    wait_sticky(&hub, 0).await;
}

// ---------------------------------------------------------------------------
// (e) 持续超限 10 秒才断开
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sender_that_keeps_flooding_while_being_handed_back_is_closed_with_1008_after_ten_seconds() {
    let hub = TestHub::start_with(|c| c.limits.conn_envelopes_per_sec = 5).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let _db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;

    // 每 50 ms 一个（20/s，远超每秒 5 个），完全无视 backpressure / rate_limited，只是读着不让缓冲堆积
    let started = Instant::now();
    let mut rate_limited_at = Vec::new();
    let mut backpressure = 0usize;
    let mut forwarded_or_returned = 0usize;
    let close = loop {
        assert!(started.elapsed() < Duration::from_secs(25), "the flooding connection was never closed");
        if !da.try_send_env(&b.id, Kind::Data, vec![0u8; 16]).await {
            break None;
        }
        forwarded_or_returned += 1;
        let until = tokio::time::Instant::now() + Duration::from_millis(50);
        let mut closed = None;
        loop {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            match da.recv_within(left).await {
                None => break,
                Some(Recv::Closed(code)) => {
                    closed = Some(code);
                    break;
                }
                Some(Recv::Env(e)) => match e.control_body() {
                    Ok(Control::RateLimited { .. }) => rate_limited_at.push(Instant::now()),
                    Ok(Control::Backpressure { .. }) => backpressure += 1,
                    _ => {}
                },
            }
        }
        if let Some(code) = closed {
            break code;
        }
    };
    let elapsed = started.elapsed();
    assert_eq!(close, Some(1008), "closed with 1008 (rate limited)");
    assert!(elapsed >= Duration::from_secs(9), "must be sustained for ~10 s before closing, got {elapsed:?}");
    assert!(elapsed <= Duration::from_secs(16), "closed late: {elapsed:?}");
    assert!(
        backpressure > 100,
        "every over-limit envelope is handed back, not dropped: {backpressure} of {forwarded_or_returned}"
    );
    for w in rate_limited_at.windows(2) {
        assert!(w[1].duration_since(w[0]) >= Duration::from_millis(900), "rate_limited at most once a second");
    }
    assert!(!rate_limited_at.is_empty());
    wait_registry_gone(&hub, &a.id).await;
    wait_sticky(&hub, 0).await;
}

async fn wait_registry_gone(hub: &TestHub, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.handle.state.registry.get(id).is_some() {
        assert!(Instant::now() < deadline, "connection was never unregistered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_sender_that_waits_as_told_is_never_closed_however_long_it_is_throttled() {
    // 把「持续」缩到 2 秒，让一个老老实实被匀速限流 6 秒的发送方证明自己不会被断开：
    // 每个周期都有信封通过，计时随之清零。
    let hub = TestHub::start_with(|c| {
        c.limits.conn_envelopes_per_sec = 10;
        c.timing.rate_violation_close_after = Duration::from_secs(2);
    })
    .await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;

    let mut sender = Sender::new(da, &b.id);
    let started = Instant::now();
    for i in 0..60u8 {
        sender.send(Kind::Data, vec![i; 32]).await;
    }
    let result = sender.settle().await;
    assert!(result.is_ok(), "an obedient sender must not be closed: {result:?}");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "60 envelopes at 10/s take ≥ 5 s, took {:?}",
        started.elapsed()
    );
    let mut got = Vec::new();
    while let Some(Recv::Env(e)) = db.recv_within(Duration::from_millis(300)).await {
        if e.kind == Kind::Data {
            got.push(e.seq);
        }
    }
    assert_eq!(got, sender.sent, "all 60 arrived, in order");
    let s = &sender.stats;
    assert!(s.unexpected.is_empty(), "{:?}", s.unexpected);
    assert!(!s.rate_limited.is_empty() && s.resent > 0);
}

// ---------------------------------------------------------------------------
// 别的路径上的保证
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_backpressure_notice_that_cannot_be_queued_closes_the_sender_instead_of_losing_it_silently() {
    // 发送方不读 socket：hub 给它的 backpressure 通知堆满控制消息队列。丢掉一条通知，发送方就永远不知道那封信没到——
    // 所以不等 30 秒，立刻按慢消费者（4003）断开，让它重连重新握手。
    let hub = TestHub::start_with(|c| {
        c.outbound_buffer_bytes = 64 * 1024; // 控制消息上限随之是 64 KiB
        c.limits.conn_envelopes_per_sec = 1; // 之后的每一封都被退回
        c.timing.rate_violation_close_after = Duration::from_secs(600);
    })
    .await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await; // 之后从不读
    let _db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    let conn = hub.handle.state.registry.get(&a.id).expect("registered");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut sent = 0u64;
    while !conn.kill.is_cancelled() {
        assert!(Instant::now() < deadline, "the sender was never closed (sent {sent})");
        for _ in 0..500 {
            if !da.try_send_env(&b.id, Kind::Data, vec![0u8; 8]).await {
                break;
            }
            sent += 1;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(conn.kill_code(), 4003);
}

#[tokio::test]
async fn a_full_size_fragment_fits_exactly_the_minimum_envelope_limit() {
    // ANYCON_MAX_ENVELOPE_BYTES 的下限就是一个满片的信封；开到下限的 hub 必须放得过满片。
    let hub = TestHub::start_with(|c| c.max_envelope_bytes = MIN_ENVELOPE_BYTES).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    da.drain_controls(Duration::from_millis(100)).await;

    let bodies = frame::encode(&message(frame::MAX_FRAGMENT + 1, 3), u32::MAX);
    let full = bodies.into_iter().next().unwrap();
    let envelope_len =
        Envelope::new(a.id.clone(), b.id.clone(), Kind::Data, u64::MAX, full.clone()).encode().unwrap().len();
    assert!(envelope_len <= MIN_ENVELOPE_BYTES, "{envelope_len} > {MIN_ENVELOPE_BYTES}");
    assert!(envelope_len > MIN_ENVELOPE_BYTES - 1024, "the minimum is within 1 KiB of a full fragment: {envelope_len}");
    da.seq = u64::MAX - 1;
    let seq = da.send_env(&b.id, Kind::Data, full.clone()).await;
    let got = loop {
        let e = db.recv_env().await;
        if e.kind == Kind::Data {
            break e;
        }
    };
    assert_eq!((got.seq, got.body), (seq, full));
    assert!(da
        .drain_controls(Duration::from_millis(200))
        .await
        .iter()
        .all(|c| !matches!(c, Control::Dropped { reason: DropReason::TooLarge, .. })));
}
