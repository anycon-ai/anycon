//! `task` / `notice` / `activity` 事件（docs/spec/daemon.md § 8.3）：
//! `task` 快照按（会话，task id）节流——前沿立即发、1 秒窗口内合并、后沿发最新的一条，状态变化不受节流；
//! agent 进程没了，还没收尾的任务各补一条 `cancelled`；大小上限。用测试内的假 adapter，事件用 `HandleLog::inject` 注入。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anycon_agent_api::AgentEvent;
use anycon_daemon::notify::testing::Recorder;
use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

fn mid() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("msg-{}", N.fetch_add(1, Ordering::SeqCst))
}

/// 建一个会话，等它回到 idle（假 adapter 的进程还活着，可以往里注入事件）。
async fn idle_session(td: &TestDaemon, c: &mut TestClient) -> SessionSummary {
    let s: SessionSummary = c
        .call(td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": mid() }))
        .await
        .unwrap();
    let t = std::time::Instant::now();
    while get(td, c, &s.id).await.status != SessionStatus::Idle {
        assert!(t.elapsed() < T, "the session never became idle");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    s
}

async fn get(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
}

/// 全部历史（`session.history` 一页按字节封顶，大事件会把一页撑满，所以翻页取完）。
async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let mut out: Vec<SessionEvent> = Vec::new();
    loop {
        let after = out.last().map_or(0, |e| e.seq);
        let r: SessionHistoryResult = c
            .call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "after_seq": after, "limit": 1000 }))
            .await
            .unwrap();
        out.extend(r.events);
        if !r.has_more {
            return out;
        }
    }
}

/// 历史里的 task 快照：(seq, ts_ms, task id, 状态, summary)。
async fn task_events(
    td: &TestDaemon,
    c: &mut TestClient,
    sid: &str,
) -> Vec<(u64, u64, String, TaskStatus, Option<String>)> {
    history(td, c, sid)
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::Task { id, status, summary, .. } => Some((e.seq, e.ts_ms, id, status, summary)),
            _ => None,
        })
        .collect()
}

fn task(id: &str, status: TaskStatus, summary: &str) -> AgentEvent {
    AgentEvent::Event(Event::Task {
        id: id.into(),
        kind: TaskKind::Subagent,
        tool_id: Some("tool-1".into()),
        title: format!("task {id}"),
        status,
        usage: None,
        last_tool: None,
        summary: Some(summary.into()),
        phases: vec![],
        agents: vec![],
    })
}

/// 等历史满足条件。
async fn wait_history(td: &TestDaemon, c: &mut TestClient, sid: &str, pred: impl Fn(&[SessionEvent]) -> bool) -> bool {
    let t = std::time::Instant::now();
    loop {
        if pred(&history(td, c, sid).await) {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 等历史里的 task 快照数达到 `n`。
async fn wait_tasks(td: &TestDaemon, c: &mut TestClient, sid: &str, n: usize) -> bool {
    let t = std::time::Instant::now();
    loop {
        if task_events(td, c, sid).await.len() >= n {
            return true;
        }
        if t.elapsed() > T {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn snapshots_of_one_task_are_throttled_leading_immediate_and_trailing_merged() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);

    // 前沿：第一条立刻发
    h.inject(task("t1", TaskStatus::Running, "v1")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 1).await);
    // 窗口内的后续快照只记下来，不发；另一个任务各算各的，它的第一条也是前沿
    h.inject(task("t1", TaskStatus::Running, "v2")).await;
    h.inject(task("t1", TaskStatus::Running, "v3")).await;
    h.inject(task("t2", TaskStatus::Running, "other")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 2).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = task_events(&td, &mut a, &s.id).await;
    let seen: Vec<_> = evs.iter().map(|e| (e.2.as_str(), e.4.as_deref().unwrap())).collect();
    assert_eq!(seen, vec![("t1", "v1"), ("t2", "other")], "v2 / v3 are held back inside the window");

    // 后沿：窗口结束发最新的一条（v3），v2 被合并掉了
    assert!(wait_tasks(&td, &mut a, &s.id, 3).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    let seen: Vec<_> = evs.iter().map(|e| (e.2.as_str(), e.4.as_deref().unwrap())).collect();
    assert_eq!(seen, vec![("t1", "v1"), ("t2", "other"), ("t1", "v3")]);
    let (lead, trail) = (evs[0].1, evs[2].1);
    assert!(
        (900..3000).contains(&(trail - lead)),
        "the trailing emit comes one window after the leading one: {}",
        trail - lead
    );

    // 后沿之后又是一个新窗口：立刻来的下一条要等；一直没有新的就不会再发
    h.inject(task("t1", TaskStatus::Running, "v4")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(task_events(&td, &mut a, &s.id).await.len(), 3, "v4 waits for the next window");
    assert!(wait_tasks(&td, &mut a, &s.id, 4).await);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let evs = task_events(&td, &mut a, &s.id).await;
    assert_eq!(evs.len(), 4, "nothing is emitted when nothing new arrived");
    assert_eq!(evs[3].4.as_deref(), Some("v4"));

    // 窗口早就过了：下一条是新的前沿，立刻发
    h.inject(task("t1", TaskStatus::Running, "v5")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 5).await);
    td.shutdown().await;
}

#[tokio::test]
async fn an_identical_snapshot_is_dropped() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);
    h.inject(task("t1", TaskStatus::Running, "same")).await;
    h.inject(task("t1", TaskStatus::Running, "same")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 1).await);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(task_events(&td, &mut a, &s.id).await.len(), 1, "no duplicate, no trailing emit of the same content");
    td.shutdown().await;
}

#[tokio::test]
async fn a_status_change_is_not_throttled_and_cancels_the_pending_trailing_emit() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);

    h.inject(task("t1", TaskStatus::Running, "v1")).await;
    h.inject(task("t1", TaskStatus::Running, "v2")).await; // 挂着等后沿
    h.inject(task("t1", TaskStatus::Completed, "done")).await; // 状态变化：立刻发，并作废后沿
    assert!(wait_tasks(&td, &mut a, &s.id, 2).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    let seen: Vec<_> = evs.iter().map(|e| (e.3, e.4.as_deref().unwrap())).collect();
    assert_eq!(seen, vec![(TaskStatus::Running, "v1"), (TaskStatus::Completed, "done")]);
    assert!(evs[1].1 - evs[0].1 < 800, "the terminal snapshot did not wait for the window");

    // 原来的后沿定时器到点了也不会把 v2 发出来（那会让终态之后又出现一条 running）
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let evs = task_events(&td, &mut a, &s.id).await;
    assert_eq!(evs.len(), 2, "{evs:?}");
    assert_eq!(evs[1].3, TaskStatus::Completed);

    // 状态变化只和**上一条发出去的**比：running → failed 同样立刻发（另一个任务，前沿之后马上就来）
    h.inject(task("t2", TaskStatus::Running, "a")).await;
    h.inject(task("t2", TaskStatus::Failed, "boom")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 4).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    assert_eq!(evs[3].3, TaskStatus::Failed);
    td.shutdown().await;
}

#[tokio::test]
async fn a_task_that_goes_back_from_completed_to_running_is_sent_at_once_and_is_open_again() {
    // Codex 的子 agent 收到新输入后会从 completed 回到 running：终态不是最终态，daemon 照常按状态变化处理
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);

    h.inject(task("t1", TaskStatus::Running, "first run")).await;
    h.inject(task("t1", TaskStatus::Completed, "PONG")).await;
    h.inject(task("t1", TaskStatus::Running, "second run")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 3).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    let seen: Vec<_> = evs.iter().map(|e| (e.3, e.4.as_deref().unwrap())).collect();
    assert_eq!(
        seen,
        vec![(TaskStatus::Running, "first run"), (TaskStatus::Completed, "PONG"), (TaskStatus::Running, "second run")]
    );
    assert!(evs[2].1 - evs[0].1 < 800, "the restart did not wait for the throttle window");

    // 它又是未收尾的任务了：进程没了就补 cancelled
    h.inject(AgentEvent::Exited { code: Some(0), error: None }).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 4).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    assert_eq!((evs[3].3, evs[3].4.as_deref()), (TaskStatus::Cancelled, Some("second run")));
    td.shutdown().await;
}

#[tokio::test]
async fn when_the_agent_exits_every_open_task_gets_a_cancelled_snapshot() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);

    h.inject(task("t1", TaskStatus::Running, "t1 v1")).await;
    h.inject(task("t2", TaskStatus::Running, "t2 v1")).await;
    h.inject(task("t2", TaskStatus::Running, "t2 v2")).await; // 还没发出去
    h.inject(task("t3", TaskStatus::Running, "t3 v1")).await;
    h.inject(task("t3", TaskStatus::Completed, "t3 done")).await; // 已经结束，不补
    assert!(wait_tasks(&td, &mut a, &s.id, 4).await);
    assert_eq!(task_events(&td, &mut a, &s.id).await.len(), 4);

    h.inject(AgentEvent::Exited { code: Some(0), error: None }).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 6).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    let tail: Vec<_> = evs[4..].iter().map(|e| (e.2.as_str(), e.3, e.4.as_deref().unwrap())).collect();
    assert_eq!(
        tail,
        vec![("t1", TaskStatus::Cancelled, "t1 v1"), ("t2", TaskStatus::Cancelled, "t2 v2")],
        "first-seen order; the content is the latest snapshot received, even the one that was still held back"
    );
    let sess = get(&td, &mut a, &s.id).await;
    assert_eq!(sess.status, SessionStatus::Ended);

    // 作废的后沿定时器不会再补发 t2 v2
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(task_events(&td, &mut a, &s.id).await.len(), 6);
    td.shutdown().await;
}

#[tokio::test]
async fn restarting_the_session_cancels_open_tasks_too() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);
    h.inject(task("t1", TaskStatus::Running, "working")).await;
    assert!(wait_tasks(&td, &mut a, &s.id, 1).await);

    // 进程是 daemon 收掉的（没有 `Exited` 事件）：任务照样不能一直显示「运行中」
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    assert!(wait_tasks(&td, &mut a, &s.id, 2).await);
    let evs = task_events(&td, &mut a, &s.id).await;
    assert_eq!((evs[1].2.as_str(), evs[1].3, evs[1].4.as_deref()), ("t1", TaskStatus::Cancelled, Some("working")));
    td.shutdown().await;
}

#[tokio::test]
async fn notices_and_activities_pass_through_unthrottled_and_never_push() {
    let td = TestDaemon::start().await;
    let rec = Arc::new(Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);
    // 建会话的那个 turn 结束时会发 turn_finished；只看后面注入的事件有没有引出通知
    rec.0.lock().unwrap().clear();

    let retrying = |n: u32| {
        AgentEvent::Event(Event::Activity {
            state: ActivityState::Retrying,
            attempt: Some(n),
            max_attempts: Some(10),
            delay_ms: Some(1000),
            reason: Some("529 overloaded_error".into()),
        })
    };
    for n in 1..=3 {
        h.inject(retrying(n)).await;
    }
    h.inject(AgentEvent::Event(Event::Activity {
        state: ActivityState::None,
        attempt: None,
        max_attempts: None,
        delay_ms: None,
        reason: None,
    }))
    .await;
    let notice = Event::Notice {
        code: "model_fallback".into(),
        level: NoticeLevel::Warning,
        message: "switched".into(),
        model_from: Some("a".into()),
        model_to: Some("b".into()),
        resets_at_ms: None,
    };
    h.inject(AgentEvent::Event(notice.clone())).await;
    h.inject(AgentEvent::Event(notice.clone())).await;

    assert!(
        wait_history(&td, &mut a, &s.id, |evs| evs.iter().filter(|e| matches!(e.event, Event::Notice { .. })).count()
            == 2)
        .await
    );
    let evs = history(&td, &mut a, &s.id).await;
    let kinds: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::Activity { state: ActivityState::Retrying, .. } => Some("retrying"),
            Event::Activity { state: ActivityState::None, .. } => Some("none"),
            Event::Notice { .. } => Some("notice"),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["retrying", "retrying", "retrying", "none", "notice", "notice"],
        "every one is kept, in order"
    );
    assert!(evs.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(rec.0.lock().unwrap().is_empty(), "notices and activities never raise a push notification");
    td.shutdown().await;
}

#[tokio::test]
async fn task_and_notice_sizes_are_bounded_before_they_are_stored_or_sent() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = idle_session(&td, &mut a).await;
    let h = td.fake.handle(0);

    let agent = |i: u32| TaskAgent {
        index: i,
        label: "x".repeat(500),
        state: if i >= 550 { TaskAgentState::Running } else { TaskAgentState::Done },
        phase_index: None,
        model: None,
        tokens: None,
        tool_calls: None,
        duration_ms: None,
        last_tool: Some("y".repeat(500)),
        error: Some("e".repeat(10_000)),
        attempt: None,
        cached: None,
    };
    h.inject(AgentEvent::Event(Event::Task {
        id: "wf".into(),
        kind: TaskKind::Workflow,
        tool_id: None,
        title: "t".repeat(500),
        status: TaskStatus::Running,
        usage: None,
        last_tool: None,
        summary: Some("s".repeat(10_000)),
        phases: vec![],
        agents: (0..600).map(agent).collect(),
    }))
    .await;
    h.inject(AgentEvent::Event(Event::Notice {
        code: "command_output".into(),
        level: NoticeLevel::Info,
        message: format!("HEAD{}TAIL", "n".repeat(300_000)),
        model_from: None,
        model_to: None,
        resets_at_ms: None,
    }))
    .await;

    assert!(wait_history(&td, &mut a, &s.id, |evs| evs.iter().any(|e| matches!(e.event, Event::Notice { .. }))).await);
    let evs = history(&td, &mut a, &s.id).await;
    let Some(Event::Task { title, summary, agents, .. }) =
        evs.iter().find_map(|e| matches!(e.event, Event::Task { .. }).then(|| e.event.clone()))
    else {
        panic!("no task event")
    };
    assert_eq!(title.chars().count(), 200);
    assert!(summary.unwrap().len() <= 4096);
    assert_eq!(agents.len(), 500);
    assert_eq!(agents.iter().filter(|a| a.state == TaskAgentState::Running).count(), 50, "running ones are kept first");
    assert!(agents.iter().all(|a| a.label.chars().count() == 200 && a.error.as_ref().unwrap().len() <= 4096));
    let Some(Event::Notice { message, .. }) =
        evs.iter().find_map(|e| matches!(e.event, Event::Notice { .. }).then(|| e.event.clone()))
    else {
        panic!("no notice event")
    };
    assert!(message.starts_with("HEAD") && message.ends_with("TAIL") && message.contains("…[truncated "));
    assert!(message.len() < 64 * 1024 + 128);
    td.shutdown().await;
}
