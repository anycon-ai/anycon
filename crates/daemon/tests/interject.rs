//! 插话失败不是 turn 失败（daemon.md § 7.5）：adapter 返回 `TurnEnded`（turn 刚结束、`turn_done` 还没被 daemon 读到）
//! 或任何其它错误时，daemon 把消息放回队列最前面，不 retire 进程、不让当前 turn 失败；当前 turn 结束后作为下一个 turn
//! 发出，只发一次。`UserInput.interject` 只有在 turn 进行中被送进去的那一条是 `true`，其余都是 `false`。
//!
//! 客户端看到的事件顺序（docs/spec/daemon.md § 7.5）：`user_message(m2)` → `status { queued_messages: 1 }` → `turn_done(1)` → `turn_start(2, m2)`。

use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::testutil::{eventually, InterjectFailure, TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

async fn start() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.fake.interject.store(true, Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    td
}

async fn create(td: &TestDaemon, c: &mut TestClient, prompt: &str, mid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": prompt, "message_id": mid })).await.unwrap()
}

async fn send(td: &TestDaemon, c: &mut TestClient, sid: &str, mid: &str, text: &str) -> SessionSendResult {
    c.call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": mid, "content": text })).await.unwrap()
}

async fn status(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionStatus {
    c.call::<SessionSummary>(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap().status
}

async fn wait_status(td: &TestDaemon, c: &mut TestClient, sid: &str, want: SessionStatus) -> bool {
    let t = std::time::Instant::now();
    loop {
        if status(td, c, sid).await == want {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let r: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    r.events
}

async fn wait_trace(td: &TestDaemon, c: &mut TestClient, sid: &str, pred: impl Fn(&[String]) -> bool) -> bool {
    let t = std::time::Instant::now();
    loop {
        if pred(&trace(&history(td, c, sid).await)) {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 事件的简写：`user:<id>`、`queued:<n>`、`turn_start:<n>:<message_id>`、`turn_done:<n>`、`error:<code>`。
fn trace(evs: &[SessionEvent]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match &e.event {
            Event::UserMessage { message_id, .. } => Some(format!("user:{message_id}")),
            Event::Status { queued_messages: Some(n), .. } => Some(format!("queued:{n}")),
            Event::TurnStart { turn_id, message_id, .. } => Some(format!("turn_start:{turn_id}:{message_id}")),
            Event::TurnDone { turn_id, .. } => Some(format!("turn_done:{turn_id}")),
            Event::Error { code, .. } => Some(format!("error:{code}")),
            _ => None,
        })
        .collect()
}

fn delivered(td: &TestDaemon, text: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    for h in td.fake.handles.lock().unwrap().iter() {
        for i in h.inputs.lock().unwrap().iter() {
            if i.content.contains(text) {
                out.push((i.interject, i.content.clone()));
            }
        }
    }
    out
}

#[tokio::test]
async fn turn_ended_sends_the_message_back_to_the_queue_and_it_runs_as_the_next_turn_exactly_once() {
    let td = start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold this", "m1").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    *td.fake.interject_failure.lock().unwrap() = Some(InterjectFailure::TurnEnded);
    let r = send(&td, &mut a, &s.id, "m2", "second message").await;
    assert_eq!((r.turn_id, r.queued), (Some(1), false), "optimistic: it was handed to the running turn");
    // 退回队列：客户端看到 status { queued_messages: 1 }，当前 turn 照常继续，进程没被 retire
    assert!(wait_trace(&td, &mut a, &s.id, |t| t.contains(&"queued:1".to_string())).await);
    assert_eq!(status(&td, &mut a, &s.id).await, SessionStatus::Running);
    assert_eq!(td.fake.spawn_count(), 1);
    assert_eq!(td.fake.handle(0).shutdowns.load(Ordering::SeqCst), 0, "the agent process is not retired");
    assert!(!trace(&history(&td, &mut a, &s.id).await).iter().any(|t| t.starts_with("error:")));
    // 当前 turn 结束 -> 积压的消息作为下一个 turn
    *td.fake.interject_failure.lock().unwrap() = None;
    td.fake.last_handle().release();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 2 }).await);
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let t = trace(&history(&td, &mut a, &s.id).await);
    assert_eq!(
        t,
        vec!["user:m1", "turn_start:1:m1", "user:m2", "queued:1", "turn_done:1", "turn_start:2:m2", "turn_done:2"],
        "{t:?}"
    );
    // 只发了一次，而且是作为新 turn（interject = false）发的；第一条也是 false
    assert_eq!(delivered(&td, "second message"), vec![(false, "second message".to_string())]);
    assert_eq!(delivered(&td, "hold this"), vec![(false, "hold this".to_string())]);
    assert_eq!(td.fake.spawn_count(), 1, "same process the whole time");
    td.shutdown().await;
}

#[tokio::test]
async fn any_other_interjection_error_leaves_the_current_turn_alone_too() {
    let td = start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold this", "m1").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    *td.fake.interject_failure.lock().unwrap() = Some(InterjectFailure::Other("steer timed out".into()));
    send(&td, &mut a, &s.id, "m2", "late thought").await;
    send(&td, &mut a, &s.id, "m3", "another thought").await;
    assert!(wait_trace(&td, &mut a, &s.id, |t| t.iter().filter(|x| x.starts_with("queued:")).count() >= 2).await);
    // 当前 turn 照常结束（不是 error），没有 error 事件，进程还在
    assert_eq!(status(&td, &mut a, &s.id).await, SessionStatus::Running);
    *td.fake.interject_failure.lock().unwrap() = None;
    td.fake.last_handle().release();
    assert!(eventually(T, || async { delivered(&td, "late thought").len() == 1 }).await);
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let t = trace(&history(&td, &mut a, &s.id).await);
    assert!(!t.iter().any(|x| x.starts_with("error:")), "{t:?}");
    assert!(t.contains(&"turn_done:1".to_string()));
    // 两条退回的消息按原来的顺序合并进同一个下一个 turn，各只发一次
    assert_eq!(delivered(&td, "late thought").len(), 1);
    let merged = delivered(&td, "late thought").remove(0);
    assert!(!merged.0 && merged.1.contains("another thought"), "{merged:?}");
    assert!(merged.1.find("late thought").unwrap() < merged.1.find("another thought").unwrap(), "{merged:?}");
    assert_eq!(delivered(&td, "another thought").len(), 1);
    assert_eq!(td.fake.spawn_count(), 1);
    td.shutdown().await;
}

/// 竞态本身：turn 刚结束（`turn_done` 已经在事件通道里、daemon 还没读到）时 adapter 收到插话，返回 `TurnEnded`。
/// 失败先于 `turn_done` 被处理：消息进积压，`end_turn` 看到积压就开下一个 turn。
#[tokio::test]
async fn the_turn_done_that_is_still_in_the_channel_does_not_lose_or_duplicate_the_message() {
    let td = start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold this", "m1").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    *td.fake.interject_failure.lock().unwrap() = Some(InterjectFailure::EndTurnThenTurnEnded);
    send(&td, &mut a, &s.id, "m2", "raced message").await;
    *td.fake.interject_failure.lock().unwrap() = None;
    assert!(eventually(T, || async { delivered(&td, "raced message").len() == 1 }).await);
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let t = trace(&history(&td, &mut a, &s.id).await);
    assert_eq!(t.iter().filter(|x| x.starts_with("turn_start")).count(), 2, "{t:?}");
    assert!(t.contains(&"turn_start:2:m2".to_string()), "{t:?}");
    assert_eq!(delivered(&td, "raced message"), vec![(false, "raced message".to_string())]);
    // 会话按状态机走完：用量里是两个 turn，没有悬空的 turn
    let sum: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(sum.usage.turns, 2);
    td.shutdown().await;
}

/// 用 adapter 自己的约定（没有进行中的 turn 就 `TurnEnded`）反复制造「turn 刚结束时发消息」的竞态：
/// 不管 daemon 先看到 `turn_done` 还是先看到插话失败，消息都恰好送达一次、作为新 turn，会话最终回到 idle。
#[tokio::test]
async fn sending_right_as_a_turn_ends_never_loses_the_message_or_leaves_a_phantom_turn() {
    let td = start().await;
    let mut a = td.client("a");
    for i in 0..30 {
        let first = format!("hold first-{i}");
        let late = format!("late-{i}");
        let s = create(&td, &mut a, &first, &format!("a{i}")).await;
        assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await, "round {i}");
        // 让 turn 结束的同时发消息：不让出执行权，两者几乎同时到达 daemon
        td.fake.last_handle().release();
        send(&td, &mut a, &s.id, &format!("b{i}"), &late).await;
        assert!(
            eventually(T, || async { delivered(&td, &late).len() == 1 }).await,
            "round {i}: {:?}",
            delivered(&td, &late)
        );
        assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await, "round {i}");
        // 再等一会儿：没有迟到的重复投递，没有悬空 turn
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert_eq!(delivered(&td, &late).len(), 1, "round {i}: delivered twice");
        let sum: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
        assert_eq!(sum.status, SessionStatus::Idle, "round {i}");
        let t = trace(&history(&td, &mut a, &s.id).await);
        let starts = t.iter().filter(|x| x.starts_with("turn_start")).count();
        let dones = t.iter().filter(|x| x.starts_with("turn_done")).count();
        assert_eq!(starts, dones, "round {i}: every turn is bracketed: {t:?}");
        assert!(!t.iter().any(|x| x.starts_with("error:")), "round {i}: {t:?}");
    }
    td.shutdown().await;
}

#[tokio::test]
async fn only_messages_handed_to_the_running_turn_are_marked_as_interjections() {
    let td = start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold this", "m1").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    send(&td, &mut a, &s.id, "m2", "steer please").await;
    assert!(eventually(T, || async { delivered(&td, "steer please").len() == 1 }).await);
    td.fake.last_handle().release();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    send(&td, &mut a, &s.id, "m3", "a fresh turn").await;
    assert!(eventually(T, || async { delivered(&td, "a fresh turn").len() == 1 }).await);
    assert_eq!(delivered(&td, "hold this"), vec![(false, "hold this".to_string())]);
    assert_eq!(delivered(&td, "steer please"), vec![(true, "steer please".to_string())]);
    assert_eq!(delivered(&td, "a fresh turn"), vec![(false, "a fresh turn".to_string())]);
    td.shutdown().await;
}

/// 取消已经发出、turn 还没收尾时到达的消息不插话（插进正在被中断的 turn，消息可能丢），直接入队，
/// 当前 turn 收尾后作为下一个 turn 发出。
#[tokio::test]
async fn a_message_that_arrives_while_a_cancel_is_pending_is_queued_not_interjected() {
    let td = start().await;
    *td.fake.cancel_delay.lock().unwrap() = Duration::from_millis(500);
    let mut a = td.client("a");
    let s = create(&td, &mut a, "hold this", "m1").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let _: Empty = a.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    // 取消已发出，fake 还要 500 ms 才结束这个 turn
    let r = send(&td, &mut a, &s.id, "m2", "after the cancel").await;
    assert_eq!((r.turn_id, r.queued), (None, true), "queued, not handed to the turn being cancelled: {r:?}");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(delivered(&td, "after the cancel").is_empty(), "nothing was sent to the cancelling turn");
    assert!(wait_trace(&td, &mut a, &s.id, |t| t.contains(&"queued:1".to_string())).await);
    // turn 收尾（cancelled）之后，消息作为下一个 turn 发出，只发一次，不是插话
    assert!(eventually(T, || async { delivered(&td, "after the cancel").len() == 1 }).await);
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(delivered(&td, "after the cancel"), vec![(false, "after the cancel".to_string())]);
    let t = trace(&history(&td, &mut a, &s.id).await);
    assert_eq!(
        t,
        vec!["user:m1", "turn_start:1:m1", "user:m2", "queued:1", "turn_done:1", "turn_start:2:m2", "turn_done:2"],
        "{t:?}"
    );
    td.shutdown().await;
}
