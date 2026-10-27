//! 后台任务（Claude Code 的后台 subagent、后台命令）与 agent 自己开的 turn（daemon.md § 7.3、§ 7.5、§ 14）：
//! 后台任务在跑时不按 `idle_timeout` 回收进程、不为改设置重启进程；agent 自己开的一轮是一个没有用户消息的 turn；
//! `turn_finished` 推迟到后台任务都结束之后。用测试内的假 adapter，事件用 `HandleLog::inject` 注入。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anycon_agent_api::AgentEvent;
use anycon_daemon::notify::testing::Recorder;
use anycon_daemon::testutil::{eventually, TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

fn mid() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    format!("msg-{}", N.fetch_add(1, Ordering::SeqCst))
}

async fn create(td: &TestDaemon, c: &mut TestClient, prompt: &str) -> SessionSummary {
    c.call(td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": prompt, "message_id": mid() }))
        .await
        .unwrap()
}

async fn send(td: &TestDaemon, c: &mut TestClient, sid: &str, text: &str) -> SessionSendResult {
    c.call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": mid(), "content": text })).await.unwrap()
}

async fn get(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
}

async fn wait_for(td: &TestDaemon, c: &mut TestClient, sid: &str, f: impl Fn(&SessionSummary) -> bool) -> bool {
    let t = std::time::Instant::now();
    loop {
        if f(&get(td, c, sid).await) {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let r: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    r.events
}

fn finished(rec: &Recorder, sid: &str) -> usize {
    rec.0
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.category == NotifyCategory::TurnFinished && b.session_id.as_deref() == Some(sid))
        .count()
}

fn usage() -> Usage {
    Usage {
        input_tokens: Some(3),
        output_tokens: Some(4),
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_usd: Some(0.002),
        duration_ms: 7,
        model: None,
        ..Default::default()
    }
}

/// 模拟 CLI 自己跑的一轮：`TurnStarted` → 一段文字 → `TurnDone(completed)`。
async fn agent_turn(td: &TestDaemon, handle: usize, text: &str) {
    let h = td.fake.handle(handle);
    h.inject(AgentEvent::TurnStarted).await;
    h.inject(AgentEvent::Event(Event::TextDelta { text: text.into() })).await;
    h.inject(AgentEvent::Event(Event::TextDone {})).await;
    h.inject(AgentEvent::Event(Event::TurnDone {
        turn_id: 0,
        reason: TurnDoneReason::Completed,
        usage: Some(usage()),
    }))
    .await;
}

#[tokio::test]
async fn background_tasks_keep_the_process_alive_past_idle_timeout() {
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "launch the subagents").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 2 }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 2).await, "summary carries the count");

    // 远超 idle_timeout：后台任务还在跑，进程不能被收掉（收掉会连同后台 subagent 一起杀掉）
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(h0.shutdowns.load(Ordering::SeqCst), 0, "no idle shutdown while background tasks run");
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Idle, "the session itself is idle");

    // 后台任务都结束：从这一刻起重新计 idle_timeout，而不是从上一个 turn 结束算（那样会立刻回收）
    h0.inject(AgentEvent::Background { tasks: 0 }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 0).await);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(h0.shutdowns.load(Ordering::SeqCst), 0, "the idle clock restarts when the last task ends");
    assert!(
        eventually(Duration::from_secs(5), || async { h0.shutdowns.load(Ordering::SeqCst) == 1 }).await,
        "reclaimed after idle_timeout once nothing runs in the background"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn an_agent_started_turn_is_a_turn_without_a_user_message() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "first").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 1 }).await;
    h0.inject(AgentEvent::Background { tasks: 0 }).await;

    // CLI 自己开始了一轮：会话进入 running
    h0.inject(AgentEvent::TurnStarted).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Running).await);
    // 这时用户发的消息排队（Claude Code 不能插话），不打断这一轮
    let r = send(&td, &mut a, &s.id, "next").await;
    assert!(r.queued, "{r:?}");
    assert!(h0.input_texts().iter().all(|t| t != "next"), "not delivered during the agent's turn");

    h0.inject(AgentEvent::Event(Event::TextDelta { text: "background result".into() })).await;
    h0.inject(AgentEvent::Event(Event::TextDone {})).await;
    h0.inject(AgentEvent::Event(Event::TurnDone {
        turn_id: 0,
        reason: TurnDoneReason::Completed,
        usage: Some(usage()),
    }))
    .await;

    // 这一轮收尾之后，排队的消息作为下一个 turn 发出
    assert!(eventually(T, || async { h0.input_texts().iter().any(|t| t == "next") }).await);
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle && s.usage.turns == 3).await);

    let evs = history(&td, &mut a, &s.id).await;
    let starts: Vec<(u64, String)> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::TurnStart { turn_id, message_id } => Some((*turn_id, message_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 3, "{starts:?}");
    assert!(!starts[0].1.is_empty() && !starts[2].1.is_empty(), "user turns name their first message");
    assert_eq!(starts[1], (2, String::new()), "the agent's own turn has no user message");
    let text = evs.iter().find(|e| matches!(&e.event, Event::TextDelta { text } if text == "background result"));
    assert_eq!(text.and_then(|e| e.turn_id), Some(2), "the agent's output belongs to its turn");
    let done = evs.iter().find(|e| matches!(&e.event, Event::TurnDone { turn_id: 2, .. }));
    assert!(done.is_some(), "the agent's turn ends with its own turn_done");
    // 这一轮的用量计入会话
    // 三个 turn：两个用户 turn（假 adapter 各记 0.01）+ agent 自己的一轮（0.002）
    let cost = get(&td, &mut a, &s.id).await.usage.cost_usd.unwrap();
    assert!((cost - 0.022).abs() < 1e-9, "{cost}");
    td.shutdown().await;
}

#[tokio::test]
async fn an_agent_turn_is_ignored_while_a_user_turn_runs() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold on").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Running).await);
    let h0 = td.fake.handle(0);
    // adapter 已经把撞上的用户消息并进了这一轮：daemon 不另开 turn
    h0.inject(AgentEvent::TurnStarted).await;
    h0.release();
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert_eq!(evs.iter().filter(|e| matches!(e.event, Event::TurnStart { .. })).count(), 1);
    assert_eq!(get(&td, &mut a, &s.id).await.usage.turns, 1);
    td.shutdown().await;
}

#[tokio::test]
async fn turn_finished_waits_until_background_tasks_are_done() {
    let td = TestDaemon::start_with(&[("notify.turn_finished", "\"always\"")], false).await;
    let rec = Arc::new(Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    let mut a = td.client("a");

    // 主 turn 派出后台任务后结束：活还没干完，不通知「完成」
    let s = create(&td, &mut a, "hold while launching").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Running).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 2 }).await;
    h0.release();
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    // 一个任务结束、agent 处理它的那一轮：还有任务在跑，仍不通知
    h0.inject(AgentEvent::Background { tasks: 1 }).await;
    agent_turn(&td, 0, "one done").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.usage.turns == 2 && s.status == SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(finished(&rec, &s.id), 0, "background work is still going on");
    // 最后一个任务结束、agent 收尾的那一轮：现在才通知，一次
    h0.inject(AgentEvent::Background { tasks: 0 }).await;
    agent_turn(&td, 0, "all done").await;
    assert!(eventually(T, || async { finished(&rec, &s.id) == 1 }).await);

    // 没有推迟的通知时，agent 自己开的一轮是已经通知过的 turn 的后续，不再通知（用另一个会话，避开 60 秒的合并窗口）
    let s2 = create(&td, &mut a, "plain").await;
    assert!(eventually(T, || async { finished(&rec, &s2.id) == 1 }).await, "a plain turn notifies as before");
    agent_turn(&td, 1, "follow-up").await;
    assert!(wait_for(&td, &mut a, &s2.id, |s| s.usage.turns == 2 && s.status == SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(finished(&rec, &s2.id), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn a_deferred_turn_finished_is_sent_when_no_agent_turn_follows() {
    let td = TestDaemon::start_with(&[("notify.turn_finished", "\"always\"")], false).await;
    td.ctx.sessions.set_finish_notify_grace(Duration::from_millis(300));
    let rec = Arc::new(Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold while launching").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Running).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 1 }).await;
    h0.release();
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(finished(&rec, &s.id), 0);
    // 任务结束了，CLI 却没有接着自己跑一轮：等一会儿兜底补发
    h0.inject(AgentEvent::Background { tasks: 0 }).await;
    assert!(eventually(T, || async { finished(&rec, &s.id) == 1 }).await);
    td.shutdown().await;
}

#[tokio::test]
async fn a_settings_restart_waits_for_background_tasks() {
    let td = TestDaemon::start().await;
    td.fake.restart_on_settings.store(true, Ordering::SeqCst);
    let mut a = td.client("a");
    let s = create(&td, &mut a, "first").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 1 }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 1).await);
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "fake-small" } }))
        .await
        .unwrap();

    // 需要重启才生效的设置：后台任务在跑，沿用当前进程（重启会杀掉后台任务）
    send(&td, &mut a, &s.id, "second").await;
    assert!(eventually(T, || async { h0.input_texts().iter().any(|t| t == "second") }).await);
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 1);
    assert_eq!(h0.shutdowns.load(Ordering::SeqCst), 0);

    // 后台任务都结束之后的下一个 turn 才重启
    h0.inject(AgentEvent::Background { tasks: 0 }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 0).await);
    send(&td, &mut a, &s.id, "third").await;
    assert!(eventually(T, || async { td.fake.spawn_count() == 2 }).await);
    assert!(eventually(T, || async { td.fake.handle(1).input_texts().iter().any(|t| t == "third") }).await);
    td.shutdown().await;
}

#[tokio::test]
async fn the_count_resets_when_the_process_goes_away() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "first").await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.status == SessionStatus::Idle).await);
    let h0 = td.fake.handle(0);
    h0.inject(AgentEvent::Background { tasks: 3 }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 3).await);
    // 进程自己退出：后台任务随它没了
    h0.inject(AgentEvent::Exited { code: Some(0), error: None }).await;
    assert!(wait_for(&td, &mut a, &s.id, |s| s.background_tasks == 0 && s.status == SessionStatus::Ended).await);
    td.shutdown().await;
}
