//! 账号内接入（ADR-36）：daemon 的 responder 引擎。
//!
//! 不经 hub：用 [`FakeHubLink`] 接管 daemon 的出站通道，直接喂 `enroll_request` 和 `EnrollFrame`，读 daemon 发出的信封；
//! 新设备一侧用 `enroll_initiator::Initiator`（CLI 用的同一份状态机）。覆盖四种路径的成功、错口令的计数 / 退避 / 锁定 /
//! 重新启用、悲观计数、并发限制、接入码的次数 / 过期 / 一次性、批准 / 拒绝、`[enroll]` 开关、通知 / 安全事件 / 审计。
//! 经真实 hub 的端到端见 `crates/cli/tests/enroll_real_hub.rs` 和 `e2e/tests/enroll.spec.ts`。

use std::sync::Arc;
use std::time::Duration;

use anycon_crypto::{Handshake, Keypair, Role, Transport};
use anycon_daemon::clients::Caller;
use anycon_daemon::enroll::{password_backoff_ms, CODE_ATTEMPTS, CODE_TTL_MS, MAX_SESSIONS, SESSION_TTL_MS};
use anycon_daemon::enroll_initiator::{base32_encode, Event, Finished, Initiator, InitiatorConfig};
use anycon_daemon::notify::testing::Recorder;
use anycon_daemon::testutil::{eventually, FakeHubLink, LinkOut, TestClient, TestDaemon};
use anycon_protocol::envelope::frame;
use anycon_protocol::*;
use serde_json::{json, Value};

const PW: &str = "correct horse battery";
const T: Duration = Duration::from_secs(10);

struct Bench {
    td: TestDaemon,
    link: FakeHubLink,
    client: TestClient,
    recorder: Arc<Recorder>,
    /// 还没被 `notes` 取走的通知。
    seen: Vec<RpcNotification>,
    /// 收到过的全部通知（只增不减，查泄漏用）。
    all_seen: Vec<RpcNotification>,
    counter: u32,
}

/// 新设备一侧：临时 id（hub 给的）、接入会话 id、initiator。
struct Newcomer {
    peer: String,
    enroll_id: String,
    init: Initiator,
    kp_pub: [u8; 32],
    kp_secret: [u8; 32],
}

/// 新设备 initiator 走到哪了。
#[derive(Debug)]
enum Out {
    /// Noise 握手走完了（第 3 条已发出）。
    Finished(Box<FinishedBox>),
    Rejected {
        reason: EnrollRejectReason,
        retry_after_ms: Option<u64>,
        attempts_left: Option<u32>,
    },
    Waiting,
    NeedCode,
}

struct FinishedBox(Finished);

impl std::fmt::Debug for FinishedBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Finished")
    }
}

fn hint(name: &str) -> EnrollHint {
    EnrollHint { name: name.into(), kind: DeviceKind::Cli, platform: "test".into() }
}

impl Bench {
    async fn new() -> Bench {
        Self::with(&[]).await
    }

    async fn with(overrides: &[(&str, &str)]) -> Bench {
        let td = TestDaemon::start_with(overrides, false).await;
        Self::around(td)
    }

    fn around(td: TestDaemon) -> Bench {
        let link = FakeHubLink::attach(&td);
        let client = td.client("phone");
        let recorder = Arc::new(Recorder::default());
        td.ctx.notifier.set_out(recorder.clone());
        Bench { td, link, client, recorder, seen: Vec::new(), all_seen: Vec::new(), counter: 0 }
    }

    async fn restart(self) -> Bench {
        let mut td = self.td;
        td.restart().await;
        Self::around(td)
    }

    async fn call<R: serde::de::DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<R, RpcError> {
        self.client.call(&self.td, method, params).await
    }

    async fn call_ok(&mut self, method: &str, params: Value) {
        self.call::<Value>(method, params).await.unwrap_or_else(|e| panic!("{method}: {e:?}"));
    }

    async fn set_password(&mut self, pw: &str) {
        self.call_ok(methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": pw })).await;
    }

    async fn status(&mut self) -> NodeEnrollStatusResult {
        self.call(methods::NODE_ENROLL_STATUS, json!({})).await.unwrap()
    }

    async fn list(&mut self) -> Vec<EnrollInfo> {
        self.call::<NodeEnrollListResult>(methods::NODE_ENROLL_LIST, json!({})).await.unwrap().enrolls
    }

    /// 本机客户端收到的某类通知（取走：下次只返回之后新来的）。
    fn notes(&mut self, method: &str) -> Vec<Value> {
        let fresh = self.client.drain_now();
        self.all_seen.extend(fresh.iter().cloned());
        self.seen.extend(fresh);
        let (taken, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.seen).into_iter().partition(|n| n.method == method);
        self.seen = kept;
        taken.into_iter().map(|n| n.params).collect()
    }

    /// 新设备放弃（hub 通知 daemon 对端取消了）。
    async fn cancel(&mut self, n: &Newcomer) {
        self.td.ctx.enroll.on_failed(Some(n.enroll_id.clone()), EnrollFailReason::Cancelled).await;
    }

    fn security_kinds(&mut self) -> Vec<String> {
        self.notes(methods::NODE_SECURITY_EVENT).iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect()
    }

    fn notified(&self, c: NotifyCategory) -> usize {
        self.recorder.0.lock().unwrap().iter().filter(|b| b.category == c).count()
    }

    /// 新建一个接入会话（hub 发 `enroll_request`）和对应的新设备。
    async fn newcomer(&mut self, method: EnrollMethod, approval: bool) -> Newcomer {
        self.newcomer_as(method, approval, "Test Phone", Keypair::generate(), None).await
    }

    async fn newcomer_as(
        &mut self,
        method: EnrollMethod,
        approval: bool,
        name: &str,
        kp: Keypair,
        peer: Option<String>,
    ) -> Newcomer {
        self.counter += 1;
        let peer = peer.unwrap_or_else(|| format!("temp-{:02}-{}", self.counter, "0".repeat(24)));
        let enroll_id = format!("{:032x}", self.counter);
        self.link.request(&self.td, &enroll_id, &peer).await;
        let (kp_pub, kp_secret) = (kp.public, kp.secret);
        let init = Initiator::new(InitiatorConfig {
            node_id: self.link.node_id.clone(),
            method,
            approval,
            hint: hint(name),
            client_info: ClientInfo {
                name: name.into(),
                kind: DeviceKind::Cli,
                platform: "test".into(),
                version: "0".into(),
                origin: None,
            },
            keypair: kp,
        });
        Newcomer { peer, enroll_id, init, kp_pub, kp_secret }
    }

    async fn send(&mut self, n: &Newcomer, f: &EnrollFrame) {
        self.link.send_frame(&self.td, &n.peer, f).await;
    }

    /// 把 daemon 发给 `n` 的帧逐个交给 initiator，它要发的帧再喂给 daemon，直到没有新帧。返回最后的状态。
    async fn step(&mut self, n: &mut Newcomer) -> Out {
        let mut last = Out::Waiting;
        loop {
            let frames = self.link.take_frames(&n.peer);
            if frames.is_empty() {
                return last;
            }
            for f in frames {
                match n.init.handle(f).expect("the initiator accepts the node's frame") {
                    Event::Send(fs) => {
                        for fr in fs {
                            self.send(n, &fr).await;
                        }
                    }
                    Event::WaitApproval => last = Out::Waiting,
                    Event::NeedCode => last = Out::NeedCode,
                    Event::Rejected { reason, retry_after_ms, attempts_left } => {
                        return Out::Rejected { reason, retry_after_ms, attempts_left }
                    }
                    Event::Finished(fin) => {
                        for fr in &fin.frames {
                            self.send(n, fr).await;
                        }
                        last = Out::Finished(Box::new(FinishedBox(*fin)));
                    }
                }
            }
        }
    }

    /// 发 hello 并推进到静止。
    async fn start(&mut self, n: &mut Newcomer) -> Out {
        let h = n.init.hello();
        self.send(n, &h).await;
        self.step(n).await
    }

    /// 收尾：daemon 发了 `pair_confirm`，hub 回 `pair_done`，daemon 经传输发 `pair.ack`；返回（正式 id，校验过的 ack）。
    async fn finish(&mut self, n: &Newcomer, fin: Finished) -> (String, PairAck) {
        let formal = format!("formal-{}-{}", n.peer, "f".repeat(8));
        let controls = self.link.take_controls();
        assert!(
            controls.iter().any(|c| matches!(c, Control::PairConfirm { peer } if *peer == n.peer)),
            "the node confirms the pairing: {controls:?}"
        );
        // 暂存期间：登记在临时 id 下，但不出现在设备列表里
        assert!(self.td.ctx.peers.record(&n.peer).is_some());
        self.td
            .ctx
            .peers
            .on_control(Control::PairDone { peer: formal.clone(), device_id: None, device_token: None })
            .await;
        let data = self.link.take_data(&formal);
        assert_eq!(data.len(), 1, "exactly one pair.ack goes to the new device");
        let ack = decrypt_ack(fin.transport, &data[0]);
        assert_eq!(ack.peer_id, formal);
        assert_eq!(ack.node_id, self.link.node_id);
        assert_eq!(ack.fingerprint, anycon_crypto::fingerprint(&fin.node_pub));
        (formal, ack)
    }

    async fn audit(&self) -> Vec<AuditEntry> {
        self.td.ctx.audit.query(None, None, 1000).await
    }
}

fn decrypt_ack(mut t: Transport, body: &[u8]) -> PairAck {
    let ct = match frame::decode(body).unwrap() {
        frame::DataFrame::Whole(ct) => ct.to_vec(),
        other => panic!("{other:?}"),
    };
    let plain = t.decrypt(&ct).expect("pair.ack decrypts under the enrollment transport");
    match RpcMessage::decode(&plain).unwrap() {
        RpcMessage::Notification(n) if n.method == methods::PAIR_ACK => serde_json::from_value(n.params).unwrap(),
        other => panic!("first message must be pair.ack, got {other:?}"),
    }
}

fn finished(o: Out) -> Finished {
    match o {
        Out::Finished(f) => f.0,
        other => panic!("expected the handshake to finish, got {other:?}"),
    }
}

fn rejected(o: Out) -> (EnrollRejectReason, Option<u64>, Option<u32>) {
    match o {
        Out::Rejected { reason, retry_after_ms, attempts_left } => (reason, retry_after_ms, attempts_left),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

fn caller(id: &str) -> Caller {
    Caller { device_id: id.into(), device_name: format!("device {id}"), kind: DeviceKind::Web, local: false }
}

// ====================================================================== 成功路径

/// 完成后的公共断言：设备登记在正式 id 下、能出现在设备列表里、通知 / 审计齐全；再用 IK 和 daemon 说话。
async fn assert_joined(b: &mut Bench, n: &Newcomer, formal: &str, secrets: &[&str]) -> Vec<Value> {
    let peers = &b.td.ctx.peers;
    assert!(peers.record(&n.peer).is_none(), "the tentative record moved to the permanent id");
    let rec = peers.record(formal).expect("registered under the permanent id");
    assert_eq!((rec.name.as_str(), rec.kind, rec.pubkey), ("Test Phone", DeviceKind::Cli, n.kp_pub));
    let devices = peers.list();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, formal);
    assert_eq!(b.td.ctx.enroll.live_count(), 0, "the session is gone");
    // 审计：requested + completed，完成后记正式 id；任何地方都没有秘密
    let audit = b.audit().await;
    let actions: Vec<&str> = audit.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"enroll.requested") && actions.contains(&"enroll.completed"), "{actions:?}");
    let done = audit.iter().find(|e| e.action == "enroll.completed").unwrap();
    assert_eq!(done.device_id, formal);
    let requested = audit.iter().find(|e| e.action == "enroll.requested").unwrap();
    assert_eq!(requested.device_id, n.peer, "before it completes the initiator is its temporary id");
    assert!(requested.device_name.contains("unverified"), "{}", requested.device_name);
    let dump = serde_json::to_string(&audit).unwrap();
    for s in secrets {
        assert!(!dump.contains(s), "the audit log must not contain a secret ({} bytes)", s.len());
    }
    // 其它在线设备收到 device_enrolled，推送里有 security
    let events = b.notes(methods::NODE_SECURITY_EVENT);
    let ev = events.iter().find(|e| e["kind"] == "device_enrolled").expect("device_enrolled");
    assert_eq!((ev["device_id"].as_str(), ev["device_name"].as_str()), (Some(formal), Some("Test Phone")));
    assert!(b.notified(NotifyCategory::Security) >= 1);
    let resolved = b.notes(methods::NODE_ENROLL_RESOLVED);
    assert!(
        resolved.iter().any(|r| r["enroll_id"] == json!(n.enroll_id) && r["result"] == "completed"),
        "{resolved:?}"
    );
    // 之后就是普通的已配对设备：IK 重连握手，加密 RPC
    let kp = Keypair::from_secret(n.kp_secret);
    let node_pub = b.td.ctx.identity.public();
    let mut hs = Handshake::reconnect(Role::Initiator, &kp, Some(&node_pub)).unwrap();
    let m1 = hs.write(&[]).unwrap();
    b.link.send_raw(&b.td, formal, &m1).await;
    let reply = b.link.take_handshakes(formal).pop().expect("the node answers the IK handshake");
    hs.read(&reply).unwrap();
    let mut t = hs.into_transport().unwrap();
    let req = RpcMessage::request(1, methods::NODE_INFO, json!({}));
    let ct = t.encrypt(&req.encode()).unwrap();
    for body in frame::encode(&ct, 1) {
        b.td.ctx.peers.on_data(formal, &body).await;
    }
    let link = &mut b.link;
    let mut got = Vec::new();
    assert!(
        eventually(T, || {
            got.extend(link.take_data(formal));
            let ok = !got.is_empty();
            async move { ok }
        })
        .await,
        "an RPC answer arrives"
    );
    let ct = match frame::decode(&got[0]).unwrap() {
        frame::DataFrame::Whole(ct) => ct.to_vec(),
        other => panic!("{other:?}"),
    };
    let plain = t.decrypt(&ct).unwrap();
    let RpcMessage::Response(r) = RpcMessage::decode(&plain).unwrap() else { panic!() };
    assert_eq!(r.result.unwrap()["node_id"], json!(b.link.node_id));
    resolved
}

#[tokio::test]
async fn password_enrollment_succeeds_clears_the_counter_and_the_device_can_talk() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    assert_eq!(b.status().await.password.failures, 0);
    let (formal, _ack) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[PW]).await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn scan_enrollment_needs_an_approver_with_the_qr_secret() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Scan, false).await;
    let secret = *n.init.scan_secret().unwrap();
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    // 扫码请求在 enroll_list 里（确认页要用），但不推送、不广播 enroll_requested
    let list = b.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].enroll_id.as_str(), list[0].method, list[0].status),
        (n.enroll_id.as_str(), Some(EnrollMethod::Scan), EnrollSessionStatus::WaitApproval)
    );
    assert!(b.notes(methods::NODE_ENROLL_REQUESTED).is_empty());
    assert_eq!(b.notified(NotifyCategory::EnrollRequested), 0, "scan requests are never pushed");
    // 批准：带上二维码里的密钥
    let r: NodeEnrollApproveResult = b
        .call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id, "secret": hex::encode(secret) }))
        .await
        .unwrap();
    assert_eq!(r, NodeEnrollApproveResult::default(), "scan returns nothing");
    let fin = finished(b.step(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    let resolved = assert_joined(&mut b, &n, &formal, &[&hex::encode(secret), &base32_encode(&secret)]).await;
    assert!(resolved.iter().any(|r| r["result"] == "approved" && r["by_device"] == "local"), "{resolved:?}");
    b.td.shutdown().await;
}

#[tokio::test]
async fn code_enrollment_with_approval_shows_the_code_only_to_the_approver() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    // 所有在线设备收到请求，推送只带类别
    let req = b.notes(methods::NODE_ENROLL_REQUESTED);
    assert_eq!(req.len(), 1);
    assert_eq!(req[0]["enroll_id"], json!(n.enroll_id));
    assert_eq!(req[0]["method"], "code");
    assert_eq!(req[0]["hint"]["name"], "Test Phone");
    assert_eq!(b.notified(NotifyCategory::EnrollRequested), 1);
    let pushed = b.recorder.0.lock().unwrap().iter().find(|p| p.category == NotifyCategory::EnrollRequested).cloned();
    let pushed = pushed.unwrap();
    assert!(
        (pushed.session_id.clone(), pushed.detail.clone()) == (None, None),
        "no session id, no details: {pushed:?}"
    );
    // 批准：6 位码只在 RPC 结果里
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    let code = r.code.expect("the code");
    assert!(code.len() == 6 && code.bytes().all(|c| c.is_ascii_digit()), "{code}");
    assert!(r.expires_at_ms.unwrap() > 0);
    // daemon 的 ready 已经发出，但新设备还没有码
    assert!(matches!(b.step(&mut n).await, Out::NeedCode));
    let Some(Event::Send(frames)) = n.init.set_code(&code).unwrap() else { panic!("the pending ready resumes") };
    for f in &frames {
        b.send(&n, f).await;
    }
    let fin = finished(b.step(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[&code]).await;
    assert!(b.notes(methods::NODE_ENROLL_REQUESTED).is_empty(), "one announcement per request");
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_join_code_from_add_device_works_for_a_code_request_that_needs_no_approval() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    assert_eq!(made.code.len(), 6);
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    assert!(n.init.set_code(&format!("{}-{}", &made.code[..3], &made.code[3..])).unwrap().is_none());
    let fin = finished(b.start(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[&made.code]).await;
    // 没有 approve、没有推送
    assert_eq!(b.notified(NotifyCategory::EnrollRequested), 0);
    let audit = b.audit().await;
    assert!(audit.iter().any(|e| e.action == "enroll.code"));
    b.td.shutdown().await;
}

// ====================================================================== 口令：计数、退避、锁定、重新启用

async fn password_attempt(b: &mut Bench, pw: &str) -> (Newcomer, Out) {
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(pw);
    let out = b.start(&mut n).await;
    (n, out)
}

/// 用 `pw` 试一次口令接入，期望被拒绝。
async fn pw_rejected(b: &mut Bench, pw: &str) -> (EnrollRejectReason, Option<u64>, Option<u32>) {
    rejected(password_attempt(b, pw).await.1)
}

/// 用 `pw` 试一次口令接入，期望握手走完，并把收尾做完。
async fn pw_joins(b: &mut Bench, pw: &str) -> (String, PairAck) {
    let (n, out) = password_attempt(b, pw).await;
    b.finish(&n, finished(out)).await
}

#[tokio::test]
async fn wrong_passwords_back_off_then_lock_and_re_enabling_resets_everything() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    b.notes(methods::NODE_SECURITY_EVENT);
    // 前两次错：只计数
    for i in 1..=2u32 {
        let (reason, retry, _) = pw_rejected(&mut b, "wrong password").await;
        assert_eq!((reason, retry), (EnrollRejectReason::WrongSecret, None));
        let st = b.status().await.password;
        assert_eq!((st.failures, st.retry_after_ms, st.state), (i, None, EnrollPasswordState::Enabled));
    }
    assert!(b.security_kinds().is_empty(), "nothing to report before the third failure");
    // 第三次：开始退避，通知所有已配对设备
    pw_rejected(&mut b, "wrong password").await;
    let st = b.status().await.password;
    assert_eq!(st.failures, 3);
    let wait = st.retry_after_ms.expect("backing off");
    assert!(wait > 55_000 && wait <= 60_000, "2^0 minute after the third failure: {wait}");
    let kinds = b.security_kinds();
    assert_eq!(kinds, vec!["password_failures".to_string()], "{kinds:?}");
    assert!(b.notified(NotifyCategory::Security) >= 1);
    // 退避期间：连正确的口令都被拒，带剩余时间，而且不计数
    let (reason, retry, _) = pw_rejected(&mut b, PW).await;
    assert_eq!(reason, EnrollRejectReason::Backoff);
    assert!(retry.unwrap() > 0 && retry.unwrap() <= 60_000, "{retry:?}");
    assert_eq!(b.status().await.password.failures, 3);
    // 等过了退避：再错一次，等待翻倍
    for n in 4..=9u32 {
        b.td.ctx.enroll.clock().advance(password_backoff_ms(n - 1) + 1_000);
        pw_rejected(&mut b, "wrong password").await;
        let st = b.status().await.password;
        assert_eq!(st.failures, n);
        let wait = st.retry_after_ms.unwrap();
        assert!(wait <= password_backoff_ms(n) && wait + 5_000 > password_backoff_ms(n), "n={n}: {wait}");
    }
    assert_eq!(password_backoff_ms(9), 64 * 60_000, "2^(9-3) minutes");
    // 第 10 次：锁定
    b.td.ctx.enroll.clock().advance(password_backoff_ms(9) + 1_000);
    b.security_kinds();
    pw_rejected(&mut b, "wrong password").await;
    let st = b.status().await.password;
    assert_eq!((st.failures, st.state), (10, EnrollPasswordState::Locked));
    assert_eq!(b.security_kinds(), vec!["password_locked".to_string()]);
    let audit = b.audit().await;
    assert!(audit.iter().any(|e| e.action == "enroll.password_locked"));
    // 10 次真正的失败，每次都带着当时的连续失败次数（被退避挡回去的那次预检拒绝另算）
    let counted: Vec<u64> = audit
        .iter()
        .filter(|e| e.action == "enroll.failed" && e.detail["method"] == "password")
        .filter_map(|e| e.detail["failures"].as_u64())
        .collect();
    assert_eq!(counted.len(), 10);
    assert_eq!(counted.iter().max(), Some(&10));
    // 锁定后连正确的口令也进不来，再久也不行
    b.td.ctx.enroll.clock().advance(48 * 3600 * 1000);
    let (reason, _, _) = pw_rejected(&mut b, PW).await;
    assert_eq!(reason, EnrollRejectReason::Locked);
    // 重新启用（已配对设备或本机）：计数清零，解除锁定，通知
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": true })).await;
    let st = b.status().await.password;
    assert_eq!((st.failures, st.state, st.retry_after_ms), (0, EnrollPasswordState::Enabled, None));
    assert!(b.security_kinds().contains(&"settings_changed".to_string()));
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_correct_password_clears_the_failure_count() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    pw_rejected(&mut b, "nope nope nope").await;
    pw_rejected(&mut b, "nope nope nope").await;
    assert_eq!(b.status().await.password.failures, 2);
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    let st = b.status().await.password;
    assert_eq!((st.failures, st.retry_after_ms), (0, None));
    b.td.shutdown().await;
}

#[tokio::test]
async fn the_failure_is_counted_before_the_share_leaves_and_survives_a_disconnect_and_a_restart() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    // hello -> ready：daemon 的 CPace 份额已经发出，新设备什么都没再发
    let h = n.init.hello();
    b.send(&n, &h).await;
    let frames = b.link.take_frames(&n.peer);
    assert!(
        matches!(
            &frames[..],
            [EnrollFrame::Reply { status: EnrollReplyStatus::Ready, cpace: Some(_), salt: Some(_), .. }]
        ),
        "{frames:?}"
    );
    // 悲观计数：份额发出时失败次数已经是 1（哪怕这一轮最后会成功）
    assert_eq!(b.status().await.password.failures, 1);
    // 中途断开（hub 说对端没了）：这一轮算失败，计数不退
    b.td.ctx.enroll.on_failed(Some(n.enroll_id.clone()), EnrollFailReason::PeerGone).await;
    assert_eq!(b.status().await.password.failures, 1);
    assert_eq!(b.td.ctx.enroll.live_count(), 0);
    let audit = b.audit().await;
    let failed = audit.iter().find(|e| e.action == "enroll.failed").expect("the abandoned attempt is on record");
    assert_eq!((failed.detail["failures"].as_u64(), failed.detail["reason"].as_str()), (Some(1), Some("peer_gone")));
    // daemon 重启：计数还在（落盘了）
    let mut b = b.restart().await;
    assert_eq!(b.status().await.password.failures, 1);
    // 崩溃：份额发出后进程直接没了，没有任何收尾——计数同样还在
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let h = n.init.hello();
    b.send(&n, &h).await;
    assert_eq!(b.status().await.password.failures, 2);
    let mut b = b.restart().await;
    assert_eq!(b.status().await.password.failures, 2, "a crash in the middle of an attempt counts as a failure");
    b.td.shutdown().await;
}

#[tokio::test]
async fn only_one_password_attempt_runs_at_a_time() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut first = b.newcomer(EnrollMethod::Password, false).await;
    first.init.set_password(PW);
    let mut second = b.newcomer(EnrollMethod::Password, false).await;
    second.init.set_password(PW);
    // 第一个拿到 ready 之后停住
    let h = first.init.hello();
    b.send(&first, &h).await;
    assert!(matches!(
        &b.link.take_frames(&first.peer)[..],
        [EnrollFrame::Reply { status: EnrollReplyStatus::Ready, .. }]
    ));
    // 第二个同时来：busy，而且不计数
    let (reason, _, _) = rejected(b.start(&mut second).await);
    assert_eq!(reason, EnrollRejectReason::Busy);
    assert_eq!(b.status().await.password.failures, 1);
    // 第一个断了（失败一次），名额放出来，第二个在同一个会话里重来就可以了
    b.td.ctx.enroll.on_failed(Some(first.enroll_id.clone()), EnrollFailReason::Cancelled).await;
    let fin = finished(b.start(&mut second).await);
    b.finish(&second, fin).await;
    assert_eq!(b.status().await.password.failures, 0);
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_stalled_password_attempt_times_out_frees_the_slot_and_counts() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let h = n.init.hello();
    b.send(&n, &h).await;
    b.link.take_frames(&n.peer);
    b.td.ctx.enroll.clock().advance(61_000);
    b.td.ctx.enroll.sweep().await;
    assert_eq!(b.status().await.password.failures, 1);
    assert_eq!(b.td.ctx.enroll.live_count(), 0, "a password session gets one attempt");
    // 名额放出来了：下一个（退避之前）可以开始
    pw_joins(&mut b, PW).await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn password_rules_status_and_who_set_it() {
    let mut b = Bench::new().await;
    let st = b.status().await;
    assert!(st.approval && !st.password.set && st.password.state == EnrollPasswordState::Enabled);
    // 至少 8 个字符（NFC 之后，按字符算）
    let e = b.call::<Value>(methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": "short" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    let e =
        b.call::<Value>(methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": "x".repeat(2000) })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    b.call_ok(methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": "一二三四五六七八" })).await;
    let st = b.status().await.password;
    assert!(st.set && st.set_at_ms.is_some());
    assert_eq!(st.set_by.as_deref(), Some("local"), "the local socket counts as `local`");
    // 已配对设备远程改：记设备 id
    b.td.ctx.enroll.set_password(&caller("dev-remote"), Some(PW.into())).await.unwrap();
    assert_eq!(b.status().await.password.set_by.as_deref(), Some("dev-remote"));
    // 改动通知所有已配对设备并进审计（不含口令）
    let events = b.notes(methods::NODE_SECURITY_EVENT);
    let changed: Vec<&Value> = events.iter().filter(|e| e["kind"] == "password_changed").collect();
    assert_eq!(changed.len(), 2);
    assert_eq!(
        (changed[1]["by_device"].as_str(), changed[1]["password_set"].as_bool()),
        (Some("dev-remote"), Some(true))
    );
    // 清除
    b.call_ok(methods::NODE_SET_ENROLL_PASSWORD, json!({})).await;
    let st = b.status().await.password;
    assert!(!st.set && st.set_by.is_none());
    let events = b.notes(methods::NODE_SECURITY_EVENT);
    assert_eq!(events.last().unwrap()["password_set"], json!(false));
    let audit = b.audit().await;
    let actions: Vec<&str> = audit.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"enroll.password_set") && actions.contains(&"enroll.password_cleared"), "{actions:?}");
    let dump = serde_json::to_string(&audit).unwrap();
    assert!(!dump.contains(PW) && !dump.contains("一二三四"));
    // 没设口令 -> password_not_set
    let (reason, _, _) = pw_rejected(&mut b, PW).await;
    assert_eq!(reason, EnrollRejectReason::PasswordNotSet);
    b.td.shutdown().await;
}

#[tokio::test]
async fn the_same_password_typed_in_a_different_unicode_form_still_works() {
    let mut b = Bench::new().await;
    b.set_password("cafe\u{301}-au-lait").await; // e + 组合重音
    pw_joins(&mut b, "caf\u{e9}-au-lait").await; // 预组合的 é
    b.td.shutdown().await;
}

#[tokio::test]
async fn changing_the_password_discards_the_old_failures_and_the_old_password_stops_working() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    pw_rejected(&mut b, "wrong wrong").await;
    assert_eq!(b.status().await.password.failures, 1);
    b.set_password("brand new password").await;
    assert_eq!(b.status().await.password.failures, 0, "changing the password resets the count");
    let (reason, _, _) = pw_rejected(&mut b, PW).await;
    assert_eq!(reason, EnrollRejectReason::WrongSecret);
    pw_joins(&mut b, "brand new password").await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn the_enroll_switches_are_written_to_config_and_take_effect_at_once() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": false })).await;
    let toml = std::fs::read_to_string(b.td.paths.config_toml()).unwrap();
    assert!(toml.contains("[enroll]") && toml.contains("approval = false"), "{toml}");
    assert!(!b.td.ctx.config.get().enroll.approval, "hot-loaded without waiting for the file watcher");
    assert!(!b.status().await.approval);
    // 请求批准的 code / scan / 主动码都被拒；主动生成接入码也不行
    for (method, approval) in [(EnrollMethod::Scan, false), (EnrollMethod::Code, true), (EnrollMethod::Code, false)] {
        let mut n = b.newcomer(method, approval).await;
        if method == EnrollMethod::Code && !approval {
            n.init.set_code("123456").unwrap();
        }
        let (reason, _, _) = rejected(b.start(&mut n).await);
        assert_eq!(reason, EnrollRejectReason::MethodDisabled, "{method:?} approval={approval}");
        b.cancel(&n).await;
    }
    let e = b.call::<Value>(methods::NODE_ENROLL_CODE, json!({})).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
    // 口令不受影响
    pw_joins(&mut b, PW).await;
    // 关口令
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": false })).await;
    let st = b.status().await;
    assert_eq!((st.password.set, st.password.state), (true, EnrollPasswordState::Disabled));
    let (reason, _, _) = pw_rejected(&mut b, PW).await;
    assert_eq!(reason, EnrollRejectReason::MethodDisabled);
    // 全部重新打开
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": true, "password": true })).await;
    assert!(b.status().await.approval);
    // 什么都不改是参数错误
    let e = b.call::<Value>(methods::NODE_SET_ENROLL_ENABLED, json!({})).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    let audit = b.audit().await;
    assert_eq!(audit.iter().filter(|e| e.action == "enroll.settings").count(), 3);
    b.td.shutdown().await;
}

// ====================================================================== 接入码：次数、过期、一次性、顶掉

#[tokio::test]
async fn a_join_code_allows_three_attempts_across_hellos_in_one_session() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let wrong = if made.code == "000000" { "000001" } else { "000000" };
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(wrong).unwrap();
    let (reason, _, left) = rejected(b.start(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    // 同一个会话里重发 hello 再试
    let (reason, _, left) = rejected(b.start(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(1)));
    n.init.set_code(&made.code).unwrap();
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    // 一次性：用掉了
    let mut again = b.newcomer(EnrollMethod::Code, false).await;
    again.init.set_code(&made.code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut again).await);
    assert_eq!(reason, EnrollRejectReason::NoCode);
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_join_code_is_dead_after_three_wrong_tries_even_for_the_right_code() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let wrong = if made.code == "000000" { "000001" } else { "000000" };
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(wrong).unwrap();
    for left in (0..CODE_ATTEMPTS).rev() {
        let (reason, _, l) = rejected(b.start(&mut n).await);
        assert_eq!((reason, l), (EnrollRejectReason::WrongSecret, Some(left)));
    }
    n.init.set_code(&made.code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::NoCode, "the code is used up");
    // 重新生成一个就又可以
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    n.init.set_code(&made.code).unwrap();
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_join_code_expires_after_five_minutes() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    b.td.ctx.enroll.clock().advance(CODE_TTL_MS + 1_000);
    // 没等扫描任务也过期了（检查是即时的）；扫描只是把它清走
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(&made.code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::NoCode);
    b.td.ctx.enroll.sweep().await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn generating_a_new_join_code_replaces_the_old_one() {
    let mut b = Bench::new().await;
    let first: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut second: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    while second.code == first.code {
        second = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    }
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(&first.code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::WrongSecret, "the old code no longer opens anything");
    n.init.set_code(&second.code).unwrap();
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    b.td.shutdown().await;
}

#[tokio::test]
async fn two_devices_racing_for_one_join_code_only_one_gets_in() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut a = b.newcomer(EnrollMethod::Code, false).await;
    let mut c = b.newcomer(EnrollMethod::Code, false).await;
    a.init.set_code(&made.code).unwrap();
    c.init.set_code(&made.code).unwrap();
    // 两个都拿到 ready、都发了份额和第 1 条
    for n in [&mut a, &mut c] {
        let h = n.init.hello();
        b.send(n, &h).await;
    }
    for n in [&mut a, &mut c] {
        let ready = b.link.take_frames(&n.peer).pop().unwrap();
        let Event::Send(frames) = n.init.handle(ready).unwrap() else { panic!() };
        for f in &frames {
            b.send(n, f).await;
        }
    }
    // a 先发第 3 条：成功，码被拿走
    let m2 = b.link.take_frames(&a.peer).pop().unwrap();
    let Event::Finished(fa) = a.init.handle(m2).unwrap() else { panic!() };
    let fa = *fa;
    b.send(&a, &fa.frames[0]).await;
    // c 的第 3 条也通过了 PSK 校验，但码已经被用掉：不放行
    let m2 = b.link.take_frames(&c.peer).pop().unwrap();
    let Event::Finished(fc) = c.init.handle(m2).unwrap() else { panic!() };
    let fc = *fc;
    b.send(&c, &fc.frames[0]).await;
    let (reason, _, _) = rejected(b.step(&mut c).await);
    assert_eq!(reason, EnrollRejectReason::Expired);
    let controls = b.link.take_controls();
    assert!(controls.iter().any(|x| matches!(x, Control::PairConfirm { peer } if *peer == a.peer)));
    assert!(!controls.iter().any(|x| matches!(x, Control::PairConfirm { peer } if *peer == c.peer)));
    assert!(b.td.ctx.peers.record(&c.peer).is_none());
    b.td.shutdown().await;
}

// ====================================================================== 批准 / 拒绝

#[tokio::test]
async fn an_approved_code_request_retries_with_ready_directly_and_runs_out_with_wrong_secret_then_expired() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    let code = r.code.unwrap();
    let wrong = if code == "000000" { "000001" } else { "000000" };
    // 批准的那一刻 daemon 就发了 ready（扣一次）；新设备输错
    assert!(matches!(b.step(&mut n).await, Out::NeedCode));
    let Some(Event::Send(frames)) = n.init.set_code(wrong).unwrap() else { panic!() };
    for f in &frames {
        b.send(&n, f).await;
    }
    let (reason, _, left) = rejected(b.step(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    let mut first_nonce = None;
    for expected_left in [1u32, 0] {
        n.init.set_code(wrong).unwrap();
        let h = n.init.hello();
        b.send(&n, &h).await;
        // 同一个会话里重发 hello：直接 ready（同一个码、新 nonce、新份额），不再 wait_approval，也不再通知别的设备
        let frames = b.link.take_frames(&n.peer);
        let [EnrollFrame::Reply { status: EnrollReplyStatus::Ready, nonce: Some(nonce), cpace: Some(_), .. }] =
            &frames[..]
        else {
            panic!("expected a ready, got {frames:?}")
        };
        assert_ne!(first_nonce.replace(nonce.clone()), Some(nonce.clone()), "a fresh daemon nonce every round");
        let Event::Send(out) = n.init.handle(frames[0].clone()).unwrap() else { panic!() };
        for f in &out {
            b.send(&n, f).await;
        }
        let (reason, _, left) = rejected(b.step(&mut n).await);
        assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(expected_left)));
    }
    assert_eq!(b.notes(methods::NODE_ENROLL_REQUESTED).len(), 1, "only the first hello announces the request");
    // 次数用完：最后一次错误回 attempts_left = 0；再 hello（哪怕码是对的）一律 expired，会话结束
    n.init.set_code(&code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::Expired);
    assert!(b.list().await.is_empty());
    b.td.shutdown().await;
}

#[tokio::test]
async fn deny_tells_the_new_device_and_frees_the_session() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    b.call_ok(methods::NODE_ENROLL_DENY, json!({ "enroll_id": n.enroll_id })).await;
    let (reason, _, _) = rejected(b.step(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::Denied);
    assert!(b.list().await.is_empty());
    // 告诉 hub 释放名额；其它设备收起卡片，带上是谁拒绝的
    assert!(b
        .link
        .take_controls()
        .iter()
        .any(|c| matches!(c, Control::EnrollCancel { enroll_id } if *enroll_id == n.enroll_id)));
    let resolved = b.notes(methods::NODE_ENROLL_RESOLVED);
    assert!(resolved.iter().any(|r| r["result"] == "denied" && r["by_device"] == "local"), "{resolved:?}");
    assert!(b.audit().await.iter().any(|e| e.action == "enroll.denied"));
    // 已经没有这个请求了
    let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    b.td.shutdown().await;
}

#[tokio::test]
async fn only_the_first_device_to_handle_a_request_wins() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    // 还没有 hello：不能批准（也不能拒绝）
    let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Busy));
    let e = b.call::<Value>(methods::NODE_ENROLL_DENY, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Busy));
    let list = b.list().await;
    assert_eq!((list.len(), list[0].status, list[0].method), (1, EnrollSessionStatus::WaitingHello, None));
    // 不存在的请求
    let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": "nope" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    let list = b.list().await;
    assert_eq!(
        (list[0].status, list[0].method, list[0].approval),
        (EnrollSessionStatus::WaitApproval, Some(EnrollMethod::Code), true)
    );
    // 第一台批准，另一台再批准 / 拒绝 -> already_resolved；第二台能看到谁批准的
    let first: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    assert!(first.code.is_some());
    let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::AlreadyResolved));
    let e = b.call::<Value>(methods::NODE_ENROLL_DENY, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::AlreadyResolved));
    let resolved = b.notes(methods::NODE_ENROLL_RESOLVED);
    assert_eq!(resolved.iter().filter(|r| r["result"] == "approved").count(), 1, "{resolved:?}");
    // 批准之后会话进入 ready
    assert_eq!(b.list().await[0].status, EnrollSessionStatus::Ready);
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_scan_approval_needs_exactly_32_bytes_and_a_wrong_secret_ends_it() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Scan, false).await;
    let secret = *n.init.scan_secret().unwrap();
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    let id = n.enroll_id.clone();
    for bad in [
        json!({ "enroll_id": id }),
        json!({ "enroll_id": id, "secret": hex::encode([1u8; 31]) }),
        json!({ "enroll_id": id, "secret": hex::encode([1u8; 33]) }),
        json!({ "enroll_id": id, "secret": "zz" }),
    ] {
        let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, bad).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    }
    // 失败的批准不算「已处理」：随后用对的密钥还能批准
    assert_eq!(b.list().await[0].status, EnrollSessionStatus::WaitApproval);
    // 扫到的是别的二维码（密钥不对）：握手第 3 条校验不过，会话作废
    b.call_ok(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": id, "secret": hex::encode([9u8; 32]) })).await;
    assert_ne!(secret, [9u8; 32]);
    let (reason, _, _) = rejected(b.step(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::WrongSecret);
    assert!(b.list().await.is_empty());
    assert!(b.td.ctx.peers.list().is_empty());
    b.td.shutdown().await;
}

// ====================================================================== 会话：数量、过期、提示信息、结束

#[tokio::test]
async fn at_most_three_enrollments_are_in_flight_and_the_fourth_is_told_busy() {
    let mut b = Bench::new().await;
    let mut ns = Vec::new();
    for _ in 0..MAX_SESSIONS {
        ns.push(b.newcomer(EnrollMethod::Code, true).await);
    }
    let mut fourth = b.newcomer(EnrollMethod::Code, true).await;
    for n in ns.iter_mut() {
        assert!(matches!(b.start(n).await, Out::Waiting));
    }
    let (reason, _, _) = rejected(b.start(&mut fourth).await);
    assert_eq!(reason, EnrollRejectReason::Busy);
    assert_eq!(b.list().await.len(), MAX_SESSIONS, "the over-limit request is not listed");
    assert!(b
        .link
        .take_controls()
        .iter()
        .any(|c| matches!(c, Control::EnrollCancel { enroll_id } if *enroll_id == fourth.enroll_id)));
    // 一个结束了，名额就有了
    b.td.ctx.enroll.on_failed(Some(ns[0].enroll_id.clone()), EnrollFailReason::Cancelled).await;
    let mut fifth = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut fifth).await, Out::Waiting));
    assert_eq!(b.list().await.len(), MAX_SESSIONS);
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_request_that_is_not_answered_expires_and_everyone_is_told() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    b.td.ctx.enroll.clock().advance(SESSION_TTL_MS + 1_000);
    b.td.ctx.enroll.sweep().await;
    assert!(b.list().await.is_empty());
    let resolved = b.notes(methods::NODE_ENROLL_RESOLVED);
    assert!(resolved.iter().any(|r| r["enroll_id"] == json!(n.enroll_id) && r["result"] == "expired"), "{resolved:?}");
    assert!(
        b.link.take_controls().iter().any(|c| matches!(c, Control::EnrollCancel { .. })),
        "the hub is told to drop it too"
    );
    // 过期之后不能批准
    let e = b.call::<Value>(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
    b.td.shutdown().await;
}

#[tokio::test]
async fn the_hint_is_cleaned_and_truncated_and_only_a_hint() {
    let mut b = Bench::new().await;
    let evil = format!("Pho\u{1b}[2Jne\u{202e}\u{0}\n{}", "x".repeat(200));
    let mut n = b.newcomer_as(EnrollMethod::Code, true, &evil, Keypair::generate(), None).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    let list = b.list().await;
    let shown = list[0].hint.as_ref().unwrap().name.clone();
    assert!(shown.starts_with("Pho[2Jne") && shown.chars().count() <= 64, "{shown:?}");
    assert!(!shown.chars().any(|c| c.is_control() || c == '\u{202e}'), "{shown:?}");
    // 批准后的设备名取握手第 3 条里认证过的 ClientInfo，不是 hint
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    b.step(&mut n).await;
    let Some(Event::Send(frames)) = n.init.set_code(&r.code.unwrap()).unwrap() else { panic!() };
    for f in &frames {
        b.send(&n, f).await;
    }
    let fin = finished(b.step(&mut n).await);
    b.finish(&n, fin).await;
    let rec = b.td.ctx.peers.list();
    assert_eq!(rec.len(), 1);
    assert!(rec[0].name.starts_with("Pho") && !rec[0].name.contains('\u{1b}'));
    b.td.shutdown().await;
}

#[tokio::test]
async fn the_hub_ending_a_session_discards_the_staged_device_and_a_replayed_hello_is_harmless() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    assert!(b.td.ctx.peers.record(&n.peer).is_some(), "staged");
    assert!(b.td.ctx.peers.list().is_empty(), "a staged device is not listed");
    // hub 说对端没了（pair_done 之前）：暂存的记录整个删掉
    b.td.ctx.enroll.on_failed(Some(n.enroll_id.clone()), EnrollFailReason::PeerGone).await;
    assert!(b.td.ctx.peers.record(&n.peer).is_none());
    assert!(b.td.ctx.peers.list().is_empty());
    assert!(!std::fs::read_to_string(b.td.paths.peers_json()).unwrap().contains(&n.peer));
    // 之后的 pair_done（迟到）不会让它复活
    b.td.ctx.peers.on_control(Control::PairDone { peer: "late".into(), device_id: None, device_token: None }).await;
    assert!(b.td.ctx.peers.list().is_empty());
    drop(fin);
    // 同样：收尾阶段的失败（配额等）——hub 对接入发的是带 id 的 `enroll_failed`，不是 `pair_failed`
    let mut n2 = b.newcomer(EnrollMethod::Password, false).await;
    n2.init.set_password(PW);
    let _fin2 = finished(b.start(&mut n2).await);
    assert!(b.td.ctx.peers.record(&n2.peer).is_some());
    b.td.ctx.enroll.on_failed(Some(n2.enroll_id.clone()), EnrollFailReason::Quota).await;
    assert!(b.td.ctx.peers.record(&n2.peer).is_none());
    assert_eq!(b.td.ctx.enroll.live_count(), 0);
    let audit = b.audit().await;
    assert!(audit.iter().any(|e| e.action == "enroll.failed" && e.detail["reason"] == "quota"), "{audit:?}");
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_device_stuck_waiting_for_pair_done_is_removed_by_the_watchdog() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let (n, out) = password_attempt(&mut b, PW).await;
    let _fin = finished(out);
    assert!(b.td.ctx.peers.record(&n.peer).is_some(), "staged");
    // 还没到时间：留着
    b.td.ctx.enroll.clock().advance(30_000);
    b.td.ctx.enroll.sweep().await;
    assert!(b.td.ctx.peers.record(&n.peer).is_some());
    // 70 秒还没等到 pair_done：和配对的看门狗一样，暂存的设备连同登记一起删掉
    b.td.ctx.enroll.clock().advance(41_000);
    b.td.ctx.enroll.sweep().await;
    assert!(b.td.ctx.peers.record(&n.peer).is_none());
    assert!(b.td.ctx.peers.list().is_empty());
    assert_eq!(b.td.ctx.enroll.live_count(), 0);
    let resolved = b.notes(methods::NODE_ENROLL_RESOLVED);
    assert!(resolved.iter().any(|r| r["enroll_id"] == json!(n.enroll_id) && r["result"] == "expired"), "{resolved:?}");
    // 口令这一次是对的：计数已经清零，不会因为收尾超时被记一次失败
    assert_eq!(b.status().await.password.failures, 0);
    b.td.shutdown().await;
}

#[tokio::test]
async fn hub_loss_drops_every_session_and_counts_the_password_attempt_in_flight() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut a = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut a).await, Out::Waiting));
    let mut p = b.newcomer(EnrollMethod::Password, false).await;
    p.init.set_password(PW);
    let h = p.init.hello();
    b.send(&p, &h).await;
    b.td.ctx.peers.on_hub_down().await;
    assert_eq!(b.td.ctx.enroll.live_count(), 0);
    assert_eq!(b.status().await.password.failures, 1);
    b.td.shutdown().await;
}

#[tokio::test]
async fn frames_that_are_not_enrollment_frames_fall_through_to_the_ik_path() {
    let mut b = Bench::new().await;
    let n = b.newcomer(EnrollMethod::Code, true).await;
    // 属于接入会话的发起方，但 body 不是 EnrollFrame：不吞掉
    assert!(!b.td.ctx.enroll.on_handshake(&n.peer, b"\x01\x02 definitely not msgpack").await);
    // 不属于任何接入会话
    let frame = rmp_serde::to_vec_named(&EnrollFrame::Cpace { share: "00".into() }).unwrap();
    assert!(!b.td.ctx.enroll.on_handshake("somebody-else", &frame).await);
    assert!(b.link.pending().is_empty());
    b.td.shutdown().await;
}

#[tokio::test]
async fn bad_hellos_are_rejected_and_a_replayed_hello_for_a_pending_request_is_idempotent() {
    let mut b = Bench::new().await;
    // 版本不对
    let n = b.newcomer(EnrollMethod::Code, true).await;
    let bad = EnrollFrame::Hello {
        v: 99,
        method: EnrollMethod::Code,
        approval: true,
        nonce: "00".repeat(16),
        hint: hint("x"),
    };
    b.send(&n, &bad).await;
    let frames = b.link.take_frames(&n.peer);
    assert!(
        matches!(&frames[..], [EnrollFrame::Reply { reason: Some(EnrollRejectReason::Invalid), .. }]),
        "{frames:?}"
    );
    // nonce 长度不对
    let n = b.newcomer(EnrollMethod::Code, true).await;
    let bad =
        EnrollFrame::Hello { v: 1, method: EnrollMethod::Code, approval: true, nonce: "00".repeat(8), hint: hint("x") };
    b.send(&n, &bad).await;
    assert!(matches!(
        &b.link.take_frames(&n.peer)[..],
        [EnrollFrame::Reply { reason: Some(EnrollRejectReason::Invalid), .. }]
    ));
    // 重发的 hello：原样再回 wait_approval，只通知一次
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    let h = n.init.hello();
    b.send(&n, &h).await;
    b.send(&n, &h).await;
    let frames = b.link.take_frames(&n.peer);
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|f| matches!(f, EnrollFrame::Reply { status: EnrollReplyStatus::WaitApproval, .. })));
    assert_eq!(b.notes(methods::NODE_ENROLL_REQUESTED).len(), 1);
    b.td.shutdown().await;
}

// ====================================================================== 通知合并、收尾槽、重新接入

#[tokio::test]
async fn security_pushes_merge_within_a_minute_but_every_event_reaches_the_clients() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "approval": false })).await;
    b.call_ok(methods::NODE_SET_ENROLL_PASSWORD, json!({})).await;
    assert_eq!(b.security_kinds().len(), 3, "one security_event per change");
    assert_eq!(b.notified(NotifyCategory::Security), 1, "one push for the three of them");
    b.td.shutdown().await;
}

#[tokio::test]
async fn every_code_request_is_announced_and_pushed_without_merging() {
    let mut b = Bench::new().await;
    for _ in 0..2 {
        let mut n = b.newcomer(EnrollMethod::Code, true).await;
        assert!(matches!(b.start(&mut n).await, Out::Waiting));
    }
    assert_eq!(b.notes(methods::NODE_ENROLL_REQUESTED).len(), 2);
    assert_eq!(b.notified(NotifyCategory::EnrollRequested), 2);
    b.td.shutdown().await;
}

#[tokio::test]
async fn two_enrollments_finishing_together_confirm_one_after_the_other() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    // 口令同时只允许一个尝试：一个用口令，一个用接入码
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut first = b.newcomer(EnrollMethod::Password, false).await;
    first.init.set_password(PW);
    let mut second = b.newcomer(EnrollMethod::Code, false).await;
    second.init.set_code(&made.code).unwrap();
    let f1 = finished(b.start(&mut first).await);
    let f2 = finished(b.start(&mut second).await);
    // 只有第一个发了 pair_confirm（pair_done 里没有会话标识，一次只能有一个等回执的）
    let controls = b.link.take_controls();
    let confirms: Vec<&String> =
        controls.iter().filter_map(|c| if let Control::PairConfirm { peer } = c { Some(peer) } else { None }).collect();
    assert_eq!(confirms, vec![&first.peer]);
    // 第一个完成之后，第二个自己接着发
    let formal1 = "formal-one".to_string();
    b.td.ctx.peers.on_control(Control::PairDone { peer: formal1.clone(), device_id: None, device_token: None }).await;
    assert_eq!(decrypt_ack(f1.transport, &b.link.take_data(&formal1)[0]).peer_id, formal1);
    let link = &mut b.link;
    let mut second_confirmed = false;
    assert!(
        eventually(T, || {
            second_confirmed |=
                link.take_controls().iter().any(|c| matches!(c, Control::PairConfirm { peer } if *peer == second.peer));
            let ok = second_confirmed;
            async move { ok }
        })
        .await,
        "the second enrollment confirms once the first is done"
    );
    let formal2 = "formal-two".to_string();
    b.td.ctx.peers.on_control(Control::PairDone { peer: formal2.clone(), device_id: None, device_token: None }).await;
    assert_eq!(decrypt_ack(f2.transport, &b.link.take_data(&formal2)[0]).peer_id, formal2);
    assert_eq!(b.td.ctx.peers.list().len(), 2);
    b.td.shutdown().await;
}

#[tokio::test]
async fn a_device_that_is_already_paired_can_enroll_again_and_a_failure_restores_its_record() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    // 先接入一次，得到一个已配对的正式设备
    let kp = Keypair::generate();
    let (kp_secret, kp_pub) = (kp.secret, kp.public);
    let mut n = b.newcomer_as(EnrollMethod::Password, false, "Old Name", kp, None).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    assert_eq!(b.td.ctx.peers.record(&formal).unwrap().name, "Old Name");
    // 它（现在是正式设备，id = formal）改个名字再接入，但在 pair_done 之前失败：恢复成原来的记录
    let mut again = b
        .newcomer_as(EnrollMethod::Password, false, "New Name", Keypair::from_secret(kp_secret), Some(formal.clone()))
        .await;
    again.init.set_password(PW);
    let fin = finished(b.start(&mut again).await);
    assert_eq!(b.td.ctx.peers.record(&formal).unwrap().name, "New Name", "the new record is in place while confirming");
    assert_eq!(b.td.ctx.peers.list().len(), 1, "an existing device stays listed");
    b.td.ctx.enroll.on_failed(Some(again.enroll_id.clone()), EnrollFailReason::PeerGone).await;
    let rec = b.td.ctx.peers.record(&formal).expect("the old record survives a failed re-enrollment");
    assert_eq!((rec.name.as_str(), rec.pubkey), ("Old Name", kp_pub));
    drop(fin);
    // 成功的话改成新的
    let mut third = b
        .newcomer_as(EnrollMethod::Password, false, "New Name", Keypair::from_secret(kp_secret), Some(formal.clone()))
        .await;
    third.init.set_password(PW);
    let fin = finished(b.start(&mut third).await);
    b.link.take_controls();
    b.td.ctx.peers.on_control(Control::PairDone { peer: formal.clone(), device_id: None, device_token: None }).await;
    let ack = decrypt_ack(fin.transport, &b.link.take_data(&formal)[0]);
    assert_eq!(ack.peer_id, formal);
    assert_eq!(b.td.ctx.peers.record(&formal).unwrap().name, "New Name");
    assert_eq!(b.td.ctx.peers.list().len(), 1);
    b.td.shutdown().await;
}

// ====================================================================== 审计里没有秘密（所有路径合在一起）

#[tokio::test]
async fn nothing_secret_ever_reaches_the_audit_log_or_the_notifications() {
    let mut b = Bench::new().await;
    let pw2 = "second secret phrase";
    b.set_password(pw2).await;
    // 口令：一次错一次对
    pw_rejected(&mut b, "a wrong phrase").await;
    pw_joins(&mut b, pw2).await;
    // 请求批准的码
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    b.start(&mut n).await;
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    let approved_code = r.code.unwrap();
    // 主动码
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    // 扫码
    let mut s = b.newcomer(EnrollMethod::Scan, false).await;
    let secret = *s.init.scan_secret().unwrap();
    b.start(&mut s).await;
    b.call_ok(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": s.enroll_id, "secret": hex::encode(secret) })).await;
    b.step(&mut s).await;
    let dump = serde_json::to_string(&b.audit().await).unwrap();
    for secret_text in [
        pw2.to_string(),
        "a wrong phrase".to_string(),
        approved_code,
        made.code,
        hex::encode(secret),
        base32_encode(&secret),
    ] {
        assert!(!dump.contains(&secret_text), "audit leaked a secret");
    }
    // 所有通知（推送 / security_event / enroll_requested）里也没有
    b.notes("none");
    let all = serde_json::to_string(&b.all_seen).unwrap();
    assert!(!all.contains(pw2) && !all.contains(&hex::encode(secret)));
    assert!(b.recorder.0.lock().unwrap().iter().all(|p| p.detail.is_none()));
    // web 的「最近接入活动」只认 detail 里字符串键 method / result / reason 和数字键 failures
    for e in b.audit().await.iter().filter(|e| e.action.starts_with("enroll.")) {
        for key in ["method", "result", "reason"] {
            if let Some(v) = e.detail.get(key) {
                assert!(v.is_string(), "{} detail.{key} must be a string: {}", e.action, e.detail);
            }
        }
        if let Some(v) = e.detail.get("failures") {
            assert!(v.is_u64(), "{} detail.failures must be a number: {}", e.action, e.detail);
        }
        match e.action.as_str() {
            "enroll.approved" => assert_eq!(e.detail["result"], "approved"),
            "enroll.completed" => assert_eq!(e.detail["result"], "completed"),
            "enroll.failed" => {
                assert!(e.detail["result"] == "failed" || e.detail["result"] == "expired", "{}", e.detail)
            }
            _ => {}
        }
    }
    // 事件日志里这些动作都在
    let audit = b.audit().await;
    let actions: std::collections::BTreeSet<&str> = audit.iter().map(|e| e.action.as_str()).collect();
    for want in ["enroll.requested", "enroll.approved", "enroll.code", "enroll.failed", "enroll.password_set"] {
        assert!(actions.contains(want), "{want} missing: {actions:?}");
    }
    let _ = LinkOut::Control(Control::PairConfirm { peer: String::new() });
    b.td.shutdown().await;
}

// ====================================================================== doctor

#[tokio::test]
async fn doctor_shows_the_node_password_and_only_a_locked_one_is_a_failure() {
    let mut b = Bench::new().await;
    async fn check(b: &mut Bench) -> DoctorCheck {
        let r: NodeDoctorResult = b.call(methods::NODE_DOCTOR, json!({})).await.unwrap();
        r.checks.into_iter().find(|c| c.id == "enroll_password").expect("a node password line in doctor")
    }
    // 没设口令：只是提示，不算失败
    let c = check(&mut b).await;
    assert!(c.ok && c.fix.is_none() && c.detail.contains("no node password"), "{c:?}");
    b.set_password(PW).await;
    let c = check(&mut b).await;
    assert!(c.ok && c.detail.contains("is set") && !c.detail.contains("failed"), "{c:?}");
    // 连续失败次数、退避
    b.td.ctx.store.enroll_password_record(4, b.td.ctx.enroll.clock().now() + 120_000, false).await.unwrap();
    let c = check(&mut b).await;
    assert!(c.ok && c.detail.contains("4 consecutive") && c.detail.contains("next attempt"), "{c:?}");
    // 口令解锁在配置里关掉了
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": false })).await;
    let c = check(&mut b).await;
    assert!(c.ok && c.detail.contains("turned off"), "{c:?}");
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": true })).await;
    // 被锁定：失败，给出下一步
    b.td.ctx.store.enroll_password_record(10, 0, true).await.unwrap();
    let c = check(&mut b).await;
    assert!(!c.ok && c.detail.contains("locked") && c.detail.contains("10"), "{c:?}");
    let fix = c.fix.expect("a fix for a locked password");
    assert!(fix.contains("anycon audit") && fix.contains("anycon passwd --enable"), "{fix}");
    b.td.shutdown().await;
}

#[tokio::test]
async fn editing_the_enroll_section_of_config_toml_is_reported_once_but_the_rpc_is_not_reported_twice() {
    let mut b = Bench::new().await;
    // `anycon config set` / 手动编辑：配置热加载发现，补报 settings_changed、记审计、推送
    anycon_daemon::config::set_value(&b.td.paths, "enroll.password", "false").unwrap();
    assert!(b.td.ctx.config.reload().unwrap());
    let c = &mut b.client;
    let mut seen = Vec::new();
    assert!(
        eventually(T, || {
            seen.extend(c.drain_now());
            let ok = seen.iter().any(|n| n.method == methods::NODE_SECURITY_EVENT);
            async move { ok }
        })
        .await,
        "a direct edit of [enroll] is announced"
    );
    b.seen.extend(seen);
    let events = b.notes(methods::NODE_SECURITY_EVENT);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (events[0]["kind"].as_str(), events[0]["by_device"].as_str()),
        (Some("settings_changed"), Some("local"))
    );
    let audit = b.audit().await;
    let row = audit.iter().find(|e| e.action == "enroll.settings").expect("audited");
    assert_eq!((row.detail["password"].as_bool(), row.detail["source"].as_str()), (Some(false), Some("config.toml")));
    assert!(b.notified(NotifyCategory::Security) >= 1);
    // 没改动（重载出同样的值）不再报
    assert!(!b.td.ctx.config.reload().unwrap());
    b.td.ctx.enroll.on_config_changed().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(b.notes(methods::NODE_SECURITY_EVENT).is_empty());
    // 经 RPC 改的：RPC 报一次，配置监视不再重复
    b.call_ok(methods::NODE_SET_ENROLL_ENABLED, json!({ "password": true })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kinds = b.security_kinds();
    assert_eq!(kinds, vec!["settings_changed".to_string()], "{kinds:?}");
    b.td.shutdown().await;
}

// ====================================================================== 接入码的计时与计次、hello 重发的 nonce、批准请求的校验、一次性码

/// 批准一个请求批准的 `code` 请求，返回 6 位码；新设备那边先走到「等输码」。
async fn approve_code_request(b: &mut Bench, n: &mut Newcomer) -> String {
    assert!(matches!(b.start(n).await, Out::Waiting));
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    assert!(matches!(b.step(n).await, Out::NeedCode), "the ready is out; the new device is waiting for the code");
    r.code.expect("the approver gets the code")
}

/// 新设备输入 `code`（`ready` 已经暂存在 initiator 里）：份额 + 第 1 条发给 daemon。
async fn type_code(b: &mut Bench, n: &mut Newcomer, code: &str) {
    let Some(Event::Send(frames)) = n.init.set_code(code).unwrap() else { panic!("the pending ready resumes") };
    for f in &frames {
        b.send(n, f).await;
    }
}

fn some_other_code(code: &str) -> &'static str {
    if code == "000000" {
        "000001"
    } else {
        "000000"
    }
}

/// 批准之后用户可以慢慢输码——等份额的阶段不计时（码自己有 5 分钟有效期），
/// 输错之后重试也一样（先发 hello 拿到 ready、再问用户）。
#[tokio::test]
async fn the_code_from_an_approval_can_be_typed_long_after_it_was_issued() {
    let mut b = Bench::new().await;
    // 批准 -> 三个多分钟之内的任何时候输码：成功
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    let code = approve_code_request(&mut b, &mut n).await;
    for _ in 0..3 {
        b.td.ctx.enroll.clock().advance(61_000);
        b.td.ctx.enroll.sweep().await;
    }
    assert_eq!(
        b.list().await[0].status,
        EnrollSessionStatus::Ready,
        "waiting for the code is not an attempt that times out"
    );
    type_code(&mut b, &mut n, &code).await;
    let fin = finished(b.step(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[&code]).await;
    // 输错之后重试：hello -> ready 之后隔很久才输对的码
    let mut m = b.newcomer(EnrollMethod::Code, true).await;
    let code = approve_code_request(&mut b, &mut m).await;
    type_code(&mut b, &mut m, some_other_code(&code)).await;
    let (reason, _, left) = rejected(b.step(&mut m).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    m.init.set_code(&code).unwrap();
    let h = m.init.hello();
    b.send(&m, &h).await;
    // 拿到 ready 之后过了一分半才把份额发出去
    let ready = b.link.take_frames(&m.peer).pop().unwrap();
    b.td.ctx.enroll.clock().advance(90_000);
    b.td.ctx.enroll.sweep().await;
    let Event::Send(frames) = m.init.handle(ready).unwrap() else { panic!("share and first message") };
    for f in &frames {
        b.send(&m, f).await;
    }
    let fin = finished(b.step(&mut m).await);
    b.finish(&m, fin).await;
    b.td.shutdown().await;
}

/// 计时从收到份额起——份额之后 61 秒不发第 3 条，这一轮结束（扣的是这一轮的一次），
/// 新设备收到 `rejected { expired, attempts_left }`，可以在同一个会话里重来。
#[tokio::test]
async fn a_device_that_goes_silent_after_its_share_costs_the_code_one_attempt_and_the_round_ends() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(&made.code).unwrap();
    // hello -> ready -> 份额 + 第 1 条；daemon 回第 2 条，新设备之后不再发第 3 条
    let h = n.init.hello();
    b.send(&n, &h).await;
    let ready = b.link.take_frames(&n.peer).pop().unwrap();
    let Event::Send(frames) = n.init.handle(ready).unwrap() else { panic!("share and first message") };
    for f in &frames {
        b.send(&n, f).await;
    }
    assert!(matches!(&b.link.take_frames(&n.peer)[..], [EnrollFrame::Noise { .. }]), "the node's second message");
    b.td.ctx.enroll.clock().advance(59_000);
    b.td.ctx.enroll.sweep().await;
    assert!(b.link.take_frames(&n.peer).is_empty(), "59 seconds after the share: still waiting");
    b.td.ctx.enroll.clock().advance(2_000);
    b.td.ctx.enroll.sweep().await;
    let (reason, _, left) = rejected(b.step(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::Expired, Some(2)), "the round is over; it cost one attempt");
    // 一轮结束了：迟到的第 3 条没有任何效果（没有 pair_confirm、没有暂存设备）
    assert!(!b.link.take_controls().iter().any(|c| matches!(c, Control::PairConfirm { .. })));
    assert!(b.td.ctx.peers.record(&n.peer).is_none());
    // 同一个会话里重来：错一次 -> 还剩 1 次（这一轮 + 超时的那一轮 = 两次）；再用对的码进来
    n.init.set_code(some_other_code(&made.code)).unwrap();
    let (reason, _, left) = rejected(b.start(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(1)));
    n.init.set_code(&made.code).unwrap();
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    // 审计里有这一次超时（不含码）
    let audit = b.audit().await;
    assert!(audit.iter().any(|e| e.action == "enroll.failed" && e.detail["reason"] == "timeout"), "{audit:?}");
    b.td.shutdown().await;
}

/// 只发 hello（拿到 ready 就不管）不扣接入码的次数，只有账号的人耗不掉别人的码。
#[tokio::test]
async fn a_device_that_only_sends_hellos_cannot_use_up_a_join_code() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    // 捣乱的：同一个会话里连发 5 个 hello，每次都拿到 ready，一个份额都不发
    let mut spam = b.newcomer(EnrollMethod::Code, false).await;
    for _ in 0..5 {
        let h = spam.init.hello();
        b.send(&spam, &h).await;
        let frames = b.link.take_frames(&spam.peer);
        assert!(matches!(&frames[..], [EnrollFrame::Reply { status: EnrollReplyStatus::Ready, .. }]), "{frames:?}");
    }
    // 另一个会话再来 3 个
    let mut spam2 = b.newcomer(EnrollMethod::Code, false).await;
    for _ in 0..3 {
        let h = spam2.init.hello();
        b.send(&spam2, &h).await;
        assert_eq!(b.link.take_frames(&spam2.peer).len(), 1);
    }
    // 拿着对的码的真用户照样进得来，而且还有完整的三次机会
    let mut real = b.newcomer(EnrollMethod::Code, false).await;
    real.init.set_code(some_other_code(&made.code)).unwrap();
    let (reason, _, left) = rejected(b.start(&mut real).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)), "the hellos above cost nothing");
    real.init.set_code(&made.code).unwrap();
    let fin = finished(b.start(&mut real).await);
    b.finish(&real, fin).await;
    b.td.shutdown().await;
}

/// 份额才是要扣次数的——每个份额一次，码被换掉之后旧码的份额得到 `no_code`，新码的次数不受影响。
#[tokio::test]
async fn a_share_for_a_join_code_that_was_replaced_meanwhile_is_refused_and_costs_the_new_code_nothing() {
    let mut b = Bench::new().await;
    let first: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut n = b.newcomer(EnrollMethod::Code, false).await;
    n.init.set_code(&first.code).unwrap();
    let h = n.init.hello();
    b.send(&n, &h).await;
    let ready = b.link.take_frames(&n.peer).pop().unwrap();
    let Event::Send(frames) = n.init.handle(ready).unwrap() else { panic!() };
    // 在新设备输码的时候，节点上生成了新的接入码
    let mut second: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    while second.code == first.code {
        second = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    }
    for f in &frames {
        b.send(&n, f).await;
    }
    let (reason, _, _) = rejected(b.step(&mut n).await);
    assert_eq!(reason, EnrollRejectReason::NoCode);
    // 新码还是整整三次
    n.init.set_code(some_other_code(&second.code)).unwrap();
    let (reason, _, left) = rejected(b.start(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    b.td.shutdown().await;
}

/// 等批准期间新设备重发 hello（新 nonce），批准后的 ready 用最新的 nonce，输对的码就能成功。
#[tokio::test]
async fn a_hello_resent_while_waiting_for_approval_carries_its_new_nonce_into_the_ready() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    // 重发：`Initiator::hello()` 每次换 nonce；daemon 再回一条 wait_approval，不再通知别的设备
    let again = n.init.hello();
    b.send(&n, &again).await;
    assert!(matches!(b.step(&mut n).await, Out::Waiting));
    assert_eq!(b.notes(methods::NODE_ENROLL_REQUESTED).len(), 1, "announced once");
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    let code = r.code.unwrap();
    assert!(matches!(b.step(&mut n).await, Out::NeedCode));
    type_code(&mut b, &mut n, &code).await;
    let fin = finished(b.step(&mut n).await);
    let (formal, _) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[&code]).await;
    // 重发的 hello 本身要合格（版本、nonce 长度）：不合格的算 invalid，和第一条 hello 一样
    let mut bad = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut bad).await, Out::Waiting));
    let broken =
        EnrollFrame::Hello { v: 1, method: EnrollMethod::Code, approval: true, nonce: "00".repeat(8), hint: hint("x") };
    b.send(&bad, &broken).await;
    assert!(matches!(
        &b.link.take_frames(&bad.peer)[..],
        [EnrollFrame::Reply { reason: Some(EnrollRejectReason::Invalid), .. }]
    ));
    b.td.shutdown().await;
}

/// Ready 阶段：批准之后输错了，重发 hello（新 nonce）拿到的 ready 也是用最新的 nonce，这次输对就成功。
#[tokio::test]
async fn a_hello_resent_after_a_wrong_code_uses_its_new_nonce_too() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    let code = approve_code_request(&mut b, &mut n).await;
    type_code(&mut b, &mut n, some_other_code(&code)).await;
    let (reason, _, left) = rejected(b.step(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    n.init.set_code(&code).unwrap();
    let fin = finished(b.start(&mut n).await);
    b.finish(&n, fin).await;
    b.td.shutdown().await;
}

/// daemon 侧：`node.enroll_approve` 对非 `scan` 请求带了 `secret` → `invalid`，
/// 不算已处理、不发 ready、不通知别的设备；随后不带 `secret` 的批准照常。
#[tokio::test]
async fn approving_a_code_request_with_a_secret_is_invalid_and_changes_nothing() {
    let mut b = Bench::new().await;
    let mut n = b.newcomer(EnrollMethod::Code, true).await;
    assert!(matches!(b.start(&mut n).await, Out::Waiting));
    b.notes(methods::NODE_ENROLL_RESOLVED);
    let e = b
        .call::<Value>(
            methods::NODE_ENROLL_APPROVE,
            json!({ "enroll_id": n.enroll_id, "secret": hex::encode([7u8; 32]) }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid), "{e:?}");
    assert_eq!(b.list().await[0].status, EnrollSessionStatus::WaitApproval, "not handled");
    assert!(b.link.take_frames(&n.peer).is_empty(), "no ready went out");
    assert!(b.notes(methods::NODE_ENROLL_RESOLVED).is_empty(), "nobody is told it was approved");
    assert!(!b.audit().await.iter().any(|e| e.action == "enroll.approved"));
    // 没有被「已处理」：不带 secret 的批准正常，码照常三次机会
    let r: NodeEnrollApproveResult =
        b.call(methods::NODE_ENROLL_APPROVE, json!({ "enroll_id": n.enroll_id })).await.unwrap();
    let code = r.code.expect("now it is approved");
    assert!(matches!(b.step(&mut n).await, Out::NeedCode));
    type_code(&mut b, &mut n, some_other_code(&code)).await;
    let (reason, _, left) = rejected(b.step(&mut n).await);
    assert_eq!((reason, left), (EnrollRejectReason::WrongSecret, Some(2)));
    b.td.shutdown().await;
}

/// 接入发出 `pair_confirm` 之后收到 `pair_failed`（别的配对的）——接入不受影响，随后 `pair_done` 正常完成。
#[tokio::test]
async fn a_pair_failed_that_belongs_to_a_pairing_does_not_touch_an_enrollment_that_is_finishing() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let fin = finished(b.start(&mut n).await);
    // 确认槽在这个接入手里（pair_confirm 已发），暂存的设备在
    assert!(b.td.ctx.peers.record(&n.peer).is_some());
    for reason in [PairFailReason::PeerGone, PairFailReason::Timeout, PairFailReason::Quota] {
        b.td.ctx.peers.on_control(Control::PairFailed { reason }).await;
    }
    assert!(b.td.ctx.peers.record(&n.peer).is_some(), "the staged device is still there");
    assert_eq!(b.td.ctx.enroll.live_count(), 1, "the enrollment is still alive");
    assert!(
        !b.link.pending().iter().any(|o| matches!(o, LinkOut::Control(Control::EnrollCancel { .. }))),
        "the enrollment did not cancel itself"
    );
    // pair_done 照常完成
    let (formal, _) = b.finish(&n, fin).await;
    assert_joined(&mut b, &n, &formal, &[PW]).await;
    b.td.shutdown().await;
}

/// 主动码一次性——设备 A 用它接入成功之后，设备 B 拿同一个码（A 的第一次就对了，码还剩两次机会）得到 `no_code`。
#[tokio::test]
async fn once_a_device_has_joined_with_a_join_code_the_same_code_is_gone_for_the_next_device() {
    let mut b = Bench::new().await;
    let made: NodeEnrollCodeResult = b.call(methods::NODE_ENROLL_CODE, json!({})).await.unwrap();
    let mut a = b.newcomer(EnrollMethod::Code, false).await;
    a.init.set_code(&made.code).unwrap();
    let fin = finished(b.start(&mut a).await);
    b.finish(&a, fin).await;
    let mut other = b.newcomer(EnrollMethod::Code, false).await;
    other.init.set_code(&made.code).unwrap();
    let (reason, _, _) = rejected(b.start(&mut other).await);
    assert_eq!(reason, EnrollRejectReason::NoCode, "a join code opens one door");
    assert!(b.td.ctx.peers.record(&other.peer).is_none());
    b.td.shutdown().await;
}

/// 修复接入码计时时顺带发现的：停滞的口令尝试被 `sweep` 收掉时，失败只记一次、`password_failures` 只发一次
/// （以前 `attempt_failed` 和随后的 `end_session` 各结算一次）。
#[tokio::test]
async fn a_stalled_password_attempt_is_settled_once() {
    let mut b = Bench::new().await;
    b.set_password(PW).await;
    pw_rejected(&mut b, "wrong wrong").await;
    pw_rejected(&mut b, "wrong wrong").await;
    b.security_kinds();
    let mut n = b.newcomer(EnrollMethod::Password, false).await;
    n.init.set_password(PW);
    let h = n.init.hello();
    b.send(&n, &h).await;
    b.link.take_frames(&n.peer);
    b.td.ctx.enroll.clock().advance(61_000);
    b.td.ctx.enroll.sweep().await;
    let st = b.status().await.password;
    assert_eq!(st.failures, 3);
    assert_eq!(b.security_kinds(), vec!["password_failures".to_string()], "reported once");
    let audit = b.audit().await;
    let third = audit
        .iter()
        .filter(|e| e.action == "enroll.failed" && e.detail["method"] == "password" && e.detail["failures"] == 3)
        .count();
    assert_eq!(third, 1, "one audit row for the third failure");
    b.td.shutdown().await;
}
