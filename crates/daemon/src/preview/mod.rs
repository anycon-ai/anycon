//! dev server 预览（ADR-39，docs/spec/daemon.md § 19，docs/protocol.md § 11）。
//!
//! daemon 给已配对设备签发预览票据（`preview.*` 方法，[`api`]），接受预览页（浏览器预览 origin 里的 service worker）经 hub
//! 建立的 Noise 隧道（[`tunnel`]），把隧道里的 HTTP 请求和 WebSocket 转给本机端口（[`upstream`]）。hub 只转发密文。
//!
//! 模块分工（带「纯」的只有数据变换，各有单元测试）：
//!
//! - `ports`（纯 + 命令执行）：`lsof` / `ps` 探测、会话归属三条规则、端口策略；
//! - `rewrite`（纯）：请求 / 响应头改写、响应体编码选择；
//! - `stream`（纯）：stream 表（id 递增、窗口、`RESET`、WebSocket 消息拼装、结束条件）；
//! - `ledger`（纯）：端到端回执账本和出站帧队列；
//! - `upstream`：每个 stream 的上游任务（连本机端口、HTTP / WebSocket）和发送窗口；
//! - `tunnel`：每个预览对端一个任务，握手、收发、定时器；
//! - 本文件：[`Preview`]——登记预览对端、隧道登记表（至多 64 条）、hub 事件的入口、票据作废、后台任务。
//!
//! hub 事件的路由在 `hub.rs`：`control.preview_peer` 登记对端，之后这个对端的 `handshake` / `data` 信封和退回 / 离线通知
//! 交给这里；`peers.rs` 在吊销设备时调 [`Preview::on_device_revoked`]。

pub mod api;
pub mod ledger;
pub mod ports;
pub mod rewrite;
pub mod stream;
mod tunnel;
pub mod upstream;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_protocol::tunnel as wire;
use anycon_protocol::PreviewGoawayCode;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

use crate::audit::Audit;
use crate::config::ConfigStore;
use crate::hub::HubOutbox;
use crate::identity::Identity;
use crate::peers::Peers;
use crate::sessions::Sessions;
use crate::store::Store;
use crate::util::now_ms;
use ports::{Policy, PortDetector, Verdict};
use tunnel::{Msg, PeerTask};
use upstream::Timeouts;

/// daemon 同时至多多少条隧道，超了握手回 `busy`。
pub const MAX_TUNNELS: usize = 64;
/// 登记的预览对端数上限（hub 自己也有限制；这里兜底，防止内存被无限登记吃掉）。
const MAX_PEERS: usize = 512;
/// 每台设备至多多少张未过期的票据。
pub const MAX_TICKETS_PER_DEVICE: usize = 64;
/// 拒绝一个端口之前，探测结果旧到这个程度就重新探测一次。
const STALE_DENIAL: Duration = Duration::from_millis(500);
/// 过期票据多久清一次。
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// `preview.list` / 断开隧道 / 作废票据要看的、一条隧道的信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelInfo {
    pub label: String,
    pub ticket_id: String,
    /// 签发这张票据的设备。
    pub device_id: String,
    pub port: u16,
    pub expires_at_ms: u64,
}

/// [`Preview`] 的依赖。
pub struct PreviewDeps {
    pub store: Store,
    pub config: Arc<ConfigStore>,
    pub identity: Arc<Identity>,
    pub outbox: Arc<HubOutbox>,
    pub audit: Audit,
    pub sessions: Arc<Sessions>,
    pub peers: Arc<Peers>,
    /// daemon 的环境（跑 `lsof` / `ps` 用）。
    pub env: Arc<HashMap<String, String>>,
}

struct PeerEntry {
    label: String,
    tx: mpsc::UnboundedSender<Msg>,
}

pub struct Preview {
    pub(crate) d: PreviewDeps,
    detector: PortDetector,
    /// 已登记的预览对端（hub 的 `preview_peer`）。
    peers: Mutex<HashMap<String, PeerEntry>>,
    /// 此刻有隧道的对端。
    tunnels: Mutex<HashMap<String, TunnelInfo>>,
    /// 签发票据（label、复用判断、数量上限）串行化。
    open_lock: AsyncMutex<()>,
    timeouts: Mutex<Timeouts>,
    /// 有 stream 活动时多久复核一次端口策略（默认 [`tunnel::RECHECK_EVERY`]，测试里缩短）。
    recheck_every: Mutex<Duration>,
}

impl Preview {
    pub fn new(d: PreviewDeps) -> Arc<Preview> {
        let detector = PortDetector::system(d.env.clone());
        Arc::new(Preview {
            d,
            detector,
            peers: Mutex::new(HashMap::new()),
            tunnels: Mutex::new(HashMap::new()),
            open_lock: AsyncMutex::new(()),
            timeouts: Mutex::new(Timeouts::default()),
            recheck_every: Mutex::new(tunnel::RECHECK_EVERY),
        })
    }

    /// `node.info.capabilities.preview`：节点开着预览。
    pub fn enabled(&self) -> bool {
        self.d.config.get().preview.enabled
    }

    /// 端口探测器（测试里可以换命令执行器）。
    pub fn detector(&self) -> &PortDetector {
        &self.detector
    }

    /// 上游超时（测试里缩短）。
    pub fn set_timeouts(&self, t: Timeouts) {
        *self.timeouts.lock().unwrap() = t;
    }

    /// 端口策略的复核间隔（测试里缩短；对之后建立的隧道生效）。
    pub fn set_recheck_every(&self, d: Duration) {
        *self.recheck_every.lock().unwrap() = d;
    }

    pub(crate) fn recheck_every(&self) -> Duration {
        *self.recheck_every.lock().unwrap()
    }

    pub(crate) fn timeouts(&self) -> Timeouts {
        *self.timeouts.lock().unwrap()
    }

    // ------------------------------------------------------------ 端口策略

    /// 端口现在能不能预览（docs/spec/daemon.md § 19.3）。`lenient`：隧道握手 / 复核时，没有进程在监听也放行。
    pub async fn check_port(&self, port: u16, lenient: bool) -> Verdict {
        let cfg = self.d.config.get();
        let allow = cfg.preview_allow_ports();
        let hub_port = ports::local_hub_port(cfg.hub.url.as_deref());
        let policy = Policy { enabled: cfg.preview.enabled, allow_ports: &allow, hub_port };
        // 配置里写了的端口不需要探测：省一次 lsof
        if policy.enabled && port != 0 && hub_port != Some(port) && allow.contains(&port) {
            return Verdict::Allowed;
        }
        if !policy.enabled {
            return Verdict::Disabled;
        }
        let (snap, age) = self.detector.snapshot_aged().await;
        let sessions = self.d.sessions.preview_targets().await;
        let verdict = ports::check_port(port, &policy, &snap, &sessions, lenient);
        // 被拒绝时，如果依据的探测结果已经有一会儿了，重新探测一次再下结论：用户可能在 agent 刚把 dev server 拉起来的
        // 两秒内点了「预览」，缓存里还没有这个监听
        if verdict == Verdict::NotAllowed && age >= STALE_DENIAL {
            let snap = self.detector.refresh().await;
            return ports::check_port(port, &policy, &snap, &sessions, lenient);
        }
        verdict
    }

    // ------------------------------------------------------------ 隧道登记

    /// 登记一条隧道。已满（不含这个对端自己的旧记录）返回 `false`。
    pub(crate) fn register_tunnel(&self, peer: &str, info: TunnelInfo) -> bool {
        let mut m = self.tunnels.lock().unwrap();
        if !m.contains_key(peer) && m.len() >= MAX_TUNNELS {
            return false;
        }
        m.insert(peer.to_string(), info);
        true
    }

    pub(crate) fn unregister_tunnel(&self, peer: &str) {
        self.tunnels.lock().unwrap().remove(peer);
    }

    /// 此刻的隧道数。
    pub fn tunnel_count(&self) -> usize {
        self.tunnels.lock().unwrap().len()
    }

    /// 每张票据此刻有几条隧道在用。
    pub(crate) fn tunnels_per_ticket(&self) -> HashMap<String, u32> {
        let mut m = HashMap::new();
        for i in self.tunnels.lock().unwrap().values() {
            *m.entry(i.ticket_id.clone()).or_insert(0) += 1;
        }
        m
    }

    /// 给满足条件的隧道发 `GOAWAY`，返回有几条。
    pub(crate) fn goaway_where(&self, code: PreviewGoawayCode, pred: impl Fn(&TunnelInfo) -> bool) -> usize {
        let targets: Vec<String> =
            self.tunnels.lock().unwrap().iter().filter(|(_, i)| pred(i)).map(|(p, _)| p.clone()).collect();
        let peers = self.peers.lock().unwrap();
        targets.iter().filter(|p| peers.get(*p).is_some_and(|e| e.tx.send(Msg::Goaway(code)).is_ok())).count()
    }

    // ------------------------------------------------------------ hub 事件

    /// 这个 id 是不是已登记的预览对端。
    pub fn is_peer(&self, id: &str) -> bool {
        self.peers.lock().unwrap().contains_key(id)
    }

    /// `control.preview_peer`：有一条预览连接接上来了。
    pub fn on_preview_peer(self: &Arc<Self>, peer: String, label: String) {
        if !wire::is_valid_label(&label) {
            tracing::debug!("preview_peer with a malformed label; ignored");
            return;
        }
        let mut peers = self.peers.lock().unwrap();
        if let Some(e) = peers.get(&peer) {
            if e.label == label {
                return;
            }
            let _ = e.tx.send(Msg::Stop);
            peers.remove(&peer);
        }
        if peers.len() >= MAX_PEERS {
            tracing::warn!("too many preview connections; ignoring one");
            return;
        }
        tracing::debug!(label = %label, peer = %peer, "preview peer registered");
        let (tx, _task) = PeerTask::spawn(self.clone(), peer.clone(), label.clone());
        peers.insert(peer, PeerEntry { label, tx });
    }

    fn send(&self, peer: &str, m: Msg) {
        if let Some(e) = self.peers.lock().unwrap().get(peer) {
            let _ = e.tx.send(m);
        }
    }

    /// 预览页的握手消息。
    pub fn on_handshake(&self, from: &str, body: &[u8]) {
        self.send(from, Msg::Handshake(body.to_vec()));
    }

    /// 预览页的 `data` 信封体。
    pub fn on_data(&self, from: &str, body: &[u8]) {
        self.send(from, Msg::Data(body.to_vec()));
    }

    /// hub 退回了发给 `to` 的 `seq`。
    pub fn on_backpressure(&self, to: &str, seq: u64) {
        self.send(to, Msg::Backpressure(seq));
    }

    /// `undeliverable { to }`：预览页已经断开。
    pub fn on_undeliverable(&self, to: &str) {
        self.send(to, Msg::Undeliverable);
    }

    /// hub 丢了发给 `peer` 的一封信封。
    pub fn on_dropped(&self, peer: &str) {
        self.send(peer, Msg::Dropped);
    }

    /// `peer_offline`：预览连接断开。
    pub fn on_peer_offline(&self, peer: &str) {
        let entry = self.peers.lock().unwrap().remove(peer);
        if let Some(e) = entry {
            let _ = e.tx.send(Msg::Stop);
        }
        self.unregister_tunnel(peer);
    }

    /// hub 连接断开：出站通道里的东西都没了，所有隧道作废（hub 重连后会对还在的预览连接再发 `preview_peer`）。
    pub fn on_hub_down(&self) {
        let all: Vec<PeerEntry> = self.peers.lock().unwrap().drain().map(|(_, e)| e).collect();
        for e in all {
            let _ = e.tx.send(Msg::Stop);
        }
        self.tunnels.lock().unwrap().clear();
    }

    /// 吊销设备：它签发的票据全部作废，用这些票据的隧道收到 `GOAWAY { revoked }`。
    ///
    /// 会等这些隧道把 `GOAWAY` 交给出站通道（最多 0.5 秒）再返回：吊销流程随后马上给 hub 发 `revoke`，hub 会以 `4004`
    /// 关掉这个设备签发的令牌对应的预览连接；`GOAWAY` 不先排进出站通道的话，预览页只会看到连接被关，不知道为什么。
    pub async fn on_device_revoked(&self, device_id: &str) {
        match self.d.store.preview_tickets_delete_device(device_id).await {
            Ok(ids) if !ids.is_empty() => {
                tracing::debug!(tickets = ids.len(), "preview tickets of a revoked device removed");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "cannot remove the preview tickets of a revoked device"),
        }
        // 库里没有票据（读写出错）时，内存里的隧道也要断
        let n = self.goaway_where(PreviewGoawayCode::Revoked, |i| i.device_id == device_id);
        if n > 0 {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            while self.tunnels.lock().unwrap().values().any(|i| i.device_id == device_id)
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tracing::debug!(tunnels = n, "tunnels of a revoked device closed");
        }
    }

    // ------------------------------------------------------------ 后台任务

    /// 清过期票据（并断开用它们的隧道）；`preview.enabled = false` 时断开全部隧道。
    pub fn spawn_background(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let sweeper = {
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(SWEEP_EVERY).await;
                    this.sweep().await;
                }
            })
        };
        let watcher = {
            let this = self.clone();
            tokio::spawn(async move {
                let mut rx = this.d.config.subscribe();
                let mut enabled = rx.borrow().preview.enabled;
                while rx.changed().await.is_ok() {
                    let now_enabled = rx.borrow().preview.enabled;
                    if enabled && !now_enabled {
                        let n = this.goaway_where(PreviewGoawayCode::Disabled, |_| true);
                        tracing::debug!(tunnels = n, "preview was turned off");
                    }
                    enabled = now_enabled;
                }
            })
        };
        vec![sweeper, watcher]
    }

    /// 删掉过期的票据，用它们的隧道收到 `GOAWAY { expired }`。
    pub async fn sweep(&self) {
        let now = now_ms();
        if let Err(e) = self.d.store.preview_tickets_delete_expired(now).await {
            tracing::warn!(error = %e, "cannot remove the expired preview tickets");
        }
        let n = self.goaway_where(PreviewGoawayCode::Expired, |i| i.expires_at_ms <= now);
        if n > 0 {
            tracing::debug!(tunnels = n, "expired preview tunnels closed");
        }
    }

    /// daemon 退出前：通知全部隧道（`GOAWAY { shutdown }`），给它们一点时间把消息交给 hub。
    pub async fn shutdown(&self) {
        let n = self.goaway_where(PreviewGoawayCode::Shutdown, |_| true);
        if n > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
