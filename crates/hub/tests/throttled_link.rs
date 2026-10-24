//! 受限链路上的顺序保证。
//!
//! 本机链路上，在途数据几毫秒内就被 hub 处理完，退回的回执总是早于发送方的重发计时，所以别的真实栈测试测不到这类问题。
//! 链路一受限，在途数据就远多于 100 ms 能排空的量：发送方在「上一轮重发的回执还在路上」时就开始下一轮重发，
//! 只能重发它**已经收到回执**的 seq。hub 如果只记一个被退回的 seq，就会先放行一个更大的 seq，接收方看到乱序，传输重置。
//!
//! 这里在 hub 和「daemon 连接」之间放一个限速 TCP 代理（只限 daemon → hub 方向，约 6 MiB/s），
//! 再把 daemon 的字节预算调到会持续触发速率退回；发送方是一个读写分离的 daemon 模型
//! （保留原文、收到 `backpressure` 就暂停、`rate_limited` 暂停整条连接、按升序原样重发，**每轮只重发已收到回执的**），
//! 发 6 条 9 MiB 的真 Noise 加密消息；接收端用 `Reassembler` 重组并解密。
//! 断言：每一片恰好到一次、严格按发送顺序、没有 `out_of_order`、解密不失步、消息逐字节一致。

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Keypair, Role, Transport};
use anycon_protocol::envelope::frame::{self, DataFrame, FrameError, Reassembler};
use anycon_protocol::{Control, DeviceKind, Envelope, Kind};
use common::*;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const MIB: usize = 1024 * 1024;

/// daemon → hub 的上行速率。
const UPLINK_BYTES_PER_SEC: u64 = 6 * MIB as u64;
/// 6 条消息一共约 54 MiB。预算 32 MiB / 分钟（桶容量 32 MiB，约 0.53 MiB / 秒补充）：前面约 32 MiB 在链路速度下过去，
/// 后面约 22 MiB 被匀速限流，持续触发速率退回。
///
/// 预算要在「链路比标称慢得多」时也一定被打到：传输耗时 T 秒，桶里最多能过 32 + 0.53·T MiB，要小于 54 MiB，
/// 即 T < 41 s（链路低于约 1.3 MiB/s 才会漏过）。共享的 CI 机器上 debug 构建实测只跑到约 2.6 MiB/s（T ≈ 21 s），
/// 预算取 42 MiB 时 T 的上限只有约 17 s，补充量就把余下的数据全放过去了，测试断言「预算确实被打到」而失败。
const DAEMON_BYTES_PER_MIN: u64 = 32 * MIB as u64;
const MESSAGES: usize = 6;
const MESSAGE_BYTES: usize = 9 * MIB;

/// 确定性的明文：每 64 KiB 一块，块内是按 `salt` 生成的伪随机内容，开头盖上（消息盐，块号）——
/// 每一块都不同，分片乱序或重复一定会让拼出来的结果和原文对不上。（debug 构建里逐字节生成 54 MiB 要好几秒，所以按块复制。）
fn plaintext(len: usize, salt: u64) -> Vec<u8> {
    const BLOCK: usize = 64 * 1024;
    let mut x = salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let pattern: Vec<u8> = (0..BLOCK)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    let mut out = vec![0u8; len];
    for (i, block) in out.chunks_mut(BLOCK).enumerate() {
        block.copy_from_slice(&pattern[..block.len()]);
        if block.len() >= 16 {
            block[..8].copy_from_slice(&salt.to_be_bytes());
            block[8..16].copy_from_slice(&(i as u64).to_be_bytes());
        }
    }
    out
}

/// 真的 Noise IK 传输（隐式递增的 nonce）：丢一片、乱序一片都会让解密失败。返回 (发送方的, 接收方的)。
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
// 限速 TCP 代理（按固定速率从一端读、写到另一端；读得慢 → 对方 TCP 窗口满 → 对方写阻塞）
// ---------------------------------------------------------------------------

const CHUNK: usize = 32 * 1024;
/// 代理内部最多缓冲的块数（6 MiB）。加上两侧内核 socket 缓冲，在途数据量约 7 MiB，和实测的受限链路相当。
const PROXY_BUFFERED_CHUNKS: usize = 192;

/// 监听一个本机端口，把每条连接转给 `hub`。客户端 → hub 方向限速 `up_bytes_per_sec`，反方向不限。
struct ThrottledProxy {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl ThrottledProxy {
    async fn start(hub: SocketAddr, up_bytes_per_sec: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(upstream) = TcpStream::connect(hub).await else { continue };
                let (client_r, client_w) = client.into_split();
                let (hub_r, hub_w) = upstream.into_split();
                tokio::spawn(throttled_copy(client_r, hub_w, up_bytes_per_sec));
                tokio::spawn(plain_copy(hub_r, client_w));
            }
        });
        Self { addr, task }
    }

    fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/ws", self.addr.port())
    }
}

impl Drop for ThrottledProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn plain_copy(mut src: OwnedReadHalf, mut dst: OwnedWriteHalf) {
    let _ = tokio::io::copy(&mut src, &mut dst).await;
    let _ = dst.shutdown().await;
}

/// 读任务尽快把数据读进有界通道，写任务按字节配额（以绝对时刻排程，不累积误差）往外放。
async fn throttled_copy(mut src: OwnedReadHalf, mut dst: OwnedWriteHalf, bytes_per_sec: u64) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PROXY_BUFFERED_CHUNKS);
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match src.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut next = tokio::time::Instant::now();
    while let Some(chunk) = rx.recv().await {
        // 睡过头的那一点点要补回来（不然速率会比标称的低）；但空闲过之后不能攒出一个突发：落后超过 20 ms 就从现在重新排程
        let now = tokio::time::Instant::now();
        if next + Duration::from_millis(20) < now {
            next = now;
        }
        next += Duration::from_secs_f64(chunk.len() as f64 / bytes_per_sec as f64);
        tokio::time::sleep_until(next).await;
        if dst.write_all(&chunk).await.is_err() {
            break;
        }
    }
    reader.abort();
    let _ = dst.shutdown().await;
}

// ---------------------------------------------------------------------------
// 发送方：读写分离的 daemon 模型
// ---------------------------------------------------------------------------

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// 读任务看到的回执。
#[derive(Default)]
struct Receipts {
    /// 已经收到回执、等着重发的 seq（升序）。
    returned: BTreeSet<u64>,
    /// 最近一次 `rate_limited` 叫停整条连接到什么时刻。
    resume_at: Option<Instant>,
    /// 收到的 `backpressure` 条数（含被粘住而退回的）。
    backpressure: usize,
    rate_limited: usize,
    /// 连接被关闭了（带 close code）。
    closed: Option<Option<u16>>,
    /// 不该出现的回执（`dropped`、`undeliverable`……）。
    unexpected: Vec<Control>,
}

type Shared = Arc<Mutex<Receipts>>;

/// 一直读：把回执记下来，唤醒写端。和写端并行，所以发送方卡在写阻塞时回执照样被消化（daemon 也是这样）。
async fn read_receipts(mut stream: WsStream, shared: Shared, wake: Arc<Notify>) {
    loop {
        let item = stream.next().await;
        let mut r = shared.lock().unwrap();
        match item {
            Some(Ok(Message::Binary(b))) => {
                let env = Envelope::decode(&b).expect("valid envelope");
                assert_eq!(env.kind, Kind::Control, "the sender only expects control messages, got {env:?}");
                match env.control_body().unwrap() {
                    Control::Backpressure { seq, .. } => {
                        r.backpressure += 1;
                        r.returned.insert(seq);
                    }
                    Control::RateLimited { retry_after_ms, .. } => {
                        r.rate_limited += 1;
                        let until = Instant::now() + Duration::from_millis(retry_after_ms.min(30_000));
                        r.resume_at = Some(r.resume_at.map_or(until, |t| t.max(until)));
                    }
                    Control::PeerOnline { .. } | Control::PeerOffline { .. } => {}
                    other => r.unexpected.push(other),
                }
            }
            Some(Ok(Message::Close(frame))) => {
                r.closed = Some(frame.map(|f| u16::from(f.code)));
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                r.closed.get_or_insert(None);
            }
        }
        let finished = r.closed.is_some();
        drop(r);
        wake.notify_waiters();
        if finished {
            return;
        }
    }
}

struct DaemonSender {
    sink: WsSink,
    shared: Shared,
    wake: Arc<Notify>,
    reader: JoinHandle<()>,
    /// 按发出顺序的 seq。
    sent: Vec<u64>,
    /// seq -> 信封原文（重发用，不会重新编码）。
    retained: BTreeMap<u64, Vec<u8>>,
    /// 重发了多少封次、多少轮。
    resent: usize,
    rounds: usize,
}

impl DaemonSender {
    fn new(dev: FakeDevice) -> (Self, String, u64) {
        let FakeDevice { ws, id, seq, .. } = dev;
        let (sink, stream) = ws.split();
        let shared = Shared::default();
        let wake = Arc::new(Notify::new());
        let reader = tokio::spawn(read_receipts(stream, shared.clone(), wake.clone()));
        let sender =
            Self { sink, shared, wake, reader, sent: Vec::new(), retained: BTreeMap::new(), resent: 0, rounds: 0 };
        (sender, id, seq)
    }

    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.sink.send(Message::binary(bytes)).await.map_err(|e| format!("write failed: {e}"))
    }

    /// 发送，直到接收端报告收齐（`done`）。发送方的规则：
    /// - 一直先发新的，直到看到第一条回执；
    /// - 有被退回的（已收到回执的）：暂停 100–300 ms（让同一轮里陆续到的回执一起带上；`rate_limited` 给了更久就等够），
    ///   然后把此刻所有已知被退回的 seq 按升序原样重发，再回到发新的；
    /// - `rate_limited` 暂停整条连接（新的和重发的都等）。
    ///
    /// **每轮只重发已收到回执的 seq**：上一轮重发的东西若又被退回，回执要走一个链路来回，到不到全看运气——
    /// 这正是受限链路上 hub 要扛住的。
    async fn deliver(
        &mut self,
        frames: Vec<(u64, Vec<u8>)>,
        done: &CancellationToken,
        give_up_at: Instant,
    ) -> Result<(), String> {
        let mut fresh = frames.into_iter();
        let mut pause_until: Option<Instant> = None;
        loop {
            if done.is_cancelled() {
                return Ok(());
            }
            if Instant::now() >= give_up_at {
                return Err("gave up waiting for everything to be delivered".into());
            }
            let (has_returned, resume_at, closed) = {
                let r = self.shared.lock().unwrap();
                (!r.returned.is_empty(), r.resume_at, r.closed)
            };
            if let Some(code) = closed {
                return Err(format!("the sender's connection was closed ({code:?})"));
            }
            let now = Instant::now();
            if let Some(t) = resume_at.filter(|t| *t > now) {
                tokio::select! { _ = tokio::time::sleep(t - now) => {}, _ = done.cancelled() => {} }
                continue;
            }
            if has_returned {
                let until = *pause_until.get_or_insert_with(|| {
                    // 确定性的「抖动」：100..=300 ms
                    now + Duration::from_millis(100 + (self.rounds as u64 * 67) % 201)
                });
                if now < until {
                    tokio::select! { _ = tokio::time::sleep(until - now) => {}, _ = done.cancelled() => {} }
                    continue;
                }
                pause_until = None;
                self.rounds += 1;
                let batch = std::mem::take(&mut self.shared.lock().unwrap().returned);
                for seq in batch {
                    self.resent += 1;
                    let bytes = self.retained[&seq].clone();
                    self.send_bytes(bytes).await?;
                }
                continue;
            }
            if let Some((seq, bytes)) = fresh.next() {
                self.sent.push(seq);
                self.retained.insert(seq, bytes.clone());
                self.send_bytes(bytes).await?;
                continue;
            }
            // 全都发出去了：等新的回执，或者接收端收齐
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = done.cancelled() => {},
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
        }
    }
}

impl Drop for DaemonSender {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

// ---------------------------------------------------------------------------
// 接收方：直连 hub，快速读 + Reassembler + 解密
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Received {
    /// 收到的每个 `data` 信封的 seq，按到达顺序。
    seqs: Vec<u64>,
    messages: Vec<Vec<u8>>,
    /// 重组器 / 解密报的错（有序的话应该一个都没有）。
    frame_errors: Vec<String>,
    /// 其中 `out_of_order` 的个数。
    out_of_order: usize,
    /// 完成后还多收到的信封（重复）。
    extra: usize,
}

fn spawn_receiver(
    mut dev: FakeDevice,
    from: String,
    expect_messages: usize,
    mut noise: Transport,
    done: CancellationToken,
) -> JoinHandle<Received> {
    tokio::spawn(async move {
        let mut got = Received::default();
        let mut reasm = Reassembler::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
        loop {
            // 收齐之后再多等一会儿，看有没有重复的片
            let complete = got.messages.len() >= expect_messages;
            let idle = if complete { Duration::from_millis(1000) } else { Duration::from_secs(30) };
            let left = deadline.saturating_duration_since(tokio::time::Instant::now()).min(idle);
            match dev.recv_within(left).await {
                None => break,
                Some(Recv::Closed(code)) => {
                    got.frame_errors.push(format!("receiver connection closed: {code:?}"));
                    break;
                }
                Some(Recv::Env(e)) if e.kind == Kind::Data => {
                    assert_eq!(e.from, from);
                    if complete {
                        got.extra += 1;
                        continue;
                    }
                    got.seqs.push(e.seq);
                    match frame::decode(&e.body) {
                        Ok(f) => {
                            let f: DataFrame<'_> = f;
                            match reasm.push(f) {
                                Ok(Some(ct)) => match noise.decrypt(&ct) {
                                    Ok(plain) => got.messages.push(plain),
                                    Err(err) => got.frame_errors.push(format!("noise desync at seq {}: {err}", e.seq)),
                                },
                                Ok(None) => {}
                                Err(err) => {
                                    if matches!(err, FrameError::OutOfOrder { .. }) {
                                        got.out_of_order += 1;
                                    }
                                    got.frame_errors.push(format!("seq {}: {err}", e.seq));
                                    reasm.clear();
                                }
                            }
                        }
                        Err(err) => got.frame_errors.push(format!("seq {}: {err}", e.seq)),
                    }
                    if got.messages.len() >= expect_messages || !got.frame_errors.is_empty() {
                        // 收齐了，或者已经出错（真实接收方此时会重置传输）：发送方可以停了
                        done.cancel();
                    }
                    if !got.frame_errors.is_empty() {
                        break;
                    }
                }
                Some(Recv::Env(_)) => {} // peer_online 等
            }
        }
        done.cancel();
        got
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn six_9_mib_messages_over_a_6_mib_per_s_uplink_under_a_tight_daemon_budget_arrive_in_order() {
    let hub = TestHub::start_with(|c| c.limits.daemon_bytes_per_min = DAEMON_BYTES_PER_MIN).await;
    let proxy = ThrottledProxy::start(hub.handle.addr, UPLINK_BYTES_PER_SEC).await;

    let a = hub.seed_device(DeviceKind::Daemon, "daemon").await;
    let b = hub.seed_device(DeviceKind::Web, "client").await;
    hub.seed_paired(&a, &b).await;
    // 客户端直连 hub（不限速），daemon 经限速代理
    let db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "client").await;
    let mut da = FakeDevice::open_with(&proxy.ws_url(), &[]).await.expect("ws upgrade through the proxy");
    da.send_auth(Some(&a.token), DeviceKind::Daemon, "daemon").await;
    match da.recv_control().await {
        Control::AuthOk { device_id, .. } => da.id = device_id,
        other => panic!("expected auth_ok, got {other:?}"),
    }

    // 先按顺序加密、切片、编码好（Noise 的 nonce 按加密顺序递增；debug 构建里要几秒，不计入下面的限速时间）
    let plains: Vec<Vec<u8>> = (0..MESSAGES).map(|i| plaintext(MESSAGE_BYTES, i as u64 + 1)).collect();
    let (mut tx, rx) = noise_pair();
    let mut frames = Vec::new();
    let mut seq = da.seq;
    for (i, p) in plains.iter().enumerate() {
        let ct = tx.encrypt(p).expect("encrypt");
        for body in frame::encode(&ct, i as u32 + 1) {
            seq += 1;
            frames.push((seq, Envelope::new(da.id.clone(), b.id.clone(), Kind::Data, seq, body).encode().unwrap()));
        }
    }
    let total_frames = frames.len();
    assert!(total_frames >= MESSAGES * 45, "{total_frames} frames");

    let done = CancellationToken::new();
    let receiver = spawn_receiver(db, a.id.clone(), MESSAGES, rx, done.clone());
    let (mut sender, _id, _seq) = DaemonSender::new(da);

    let started = Instant::now();
    let result = sender.deliver(frames, &done, Instant::now() + Duration::from_secs(120)).await;
    let elapsed = started.elapsed();
    // 先让接收方把结果交出来：出错时它的诊断比发送方的更有用
    done.cancel();
    let received = tokio::time::timeout(Duration::from_secs(30), receiver)
        .await
        .expect("the receiver finishes")
        .expect("receiver task");

    let (backpressure, rate_limited, unexpected, closed) = {
        let r = sender.shared.lock().unwrap();
        (r.backpressure, r.rate_limited, r.unexpected.len(), r.closed)
    };
    println!(
        "{total_frames} frames in {elapsed:?}; backpressure notices {backpressure}, rate_limited notices {rate_limited}, \
         resent {} in {} rounds",
        sender.resent, sender.rounds
    );

    assert_eq!(
        received.out_of_order,
        0,
        "the receiver saw fragments out of order: {:?} (first of {})",
        received.frame_errors.first(),
        received.frame_errors.len()
    );
    assert!(received.frame_errors.is_empty(), "the receiver saw trouble: {:?}", received.frame_errors);
    if let Err(e) = result {
        panic!("{e}; the receiver got {} of {total_frames} fragments", received.seqs.len());
    }
    assert_eq!(received.extra, 0, "duplicate envelopes arrived");
    assert_eq!(received.seqs.len(), total_frames, "every fragment must arrive");
    assert_eq!(
        received.seqs, sender.sent,
        "fragments must arrive exactly once each, strictly in the order they were sent"
    );
    assert_eq!(received.messages.len(), MESSAGES);
    for (i, (got, want)) in received.messages.iter().zip(&plains).enumerate() {
        assert!(got == want, "message {i} was corrupted ({} vs {} bytes)", got.len(), want.len());
    }
    assert_eq!(unexpected, 0, "nothing may be dropped or undeliverable");
    assert_eq!(closed, None, "the daemon connection must stay open");

    // 这个测试只在 hub 真的退回过、并且是被速率预算持续退回时才有意义
    assert!(rate_limited > 0, "the daemon budget must actually have been hit");
    assert!(backpressure > total_frames / 4, "sustained bouncing expected, got {backpressure} notices");
    assert!(sender.resent > 0 && sender.rounds >= 5, "resent {} in {} rounds", sender.resent, sender.rounds);
    // 最后一片转发出去之后，粘住状态很快清空（清理紧跟在入队之后，所以要等一下）
    let cleared = Instant::now() + Duration::from_secs(2);
    while !hub.handle.state.registry.sticky().is_empty() {
        assert!(Instant::now() < cleared, "sticky state left behind: {:?}", hub.handle.state.registry);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
