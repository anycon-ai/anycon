//! 会话状态机、排队、权限、取消、幂等、turn 合并、idle 超时与 resume、设置校验。
//! 用测试内的假 adapter（`anycon_daemon::testutil::FakeAdapter`），不依赖 adapters crate 的 mock。

use std::time::Duration;

use anycon_daemon::testutil::{eventually, AdapterErrorKind, TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

async fn create(td: &TestDaemon, c: &mut TestClient, prompt: Option<&str>) -> SessionSummary {
    let mut p = json!({ "agent": "fake" });
    if let Some(pr) = prompt {
        p["prompt"] = json!(pr);
        p["message_id"] = json!(uuid());
    }
    c.call(td, methods::SESSION_CREATE, p).await.unwrap()
}

fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("msg-{}", N.fetch_add(1, Ordering::SeqCst))
}

async fn send(td: &TestDaemon, c: &mut TestClient, sid: &str, text: &str) -> SessionSendResult {
    c.call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": uuid(), "content": text }))
        .await
        .unwrap()
}

async fn get(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
}

async fn wait_status(td: &TestDaemon, c: &mut TestClient, sid: &str, want: SessionStatus) -> bool {
    let t = std::time::Instant::now();
    loop {
        if get(td, c, sid).await.status == want {
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

fn kinds(evs: &[SessionEvent]) -> Vec<String> {
    evs.iter().map(|e| serde_json::to_value(&e.event).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn count(evs: &[SessionEvent], kind: &str) -> usize {
    kinds(evs).iter().filter(|k| *k == kind).count()
}

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("stable code")
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_events_seq_usage_title_and_changed_notifications() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let s = create(&td, &mut c, None).await;
    assert_eq!(s.status, SessionStatus::Idle);
    assert!(s.project.is_none(), "no project = scratch");
    assert!(td.workspace.join(".scratch").join(&s.id).is_dir());
    let _: Empty = c.call(&td, methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();

    let r = send(&td, &mut c, &s.id, "say hello world please").await;
    assert!(!r.duplicate);
    assert_eq!(r.turn_id, Some(1));
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await);

    let evs = history(&td, &mut c, &s.id).await;
    // seq 连续递增、从 1 开始
    assert_eq!(evs.iter().map(|e| e.seq).collect::<Vec<_>>(), (1..=evs.len() as u64).collect::<Vec<_>>());
    let k = kinds(&evs);
    assert_eq!(k[0], "settings_changed", "initial settings are the first event: {k:?}");
    for want in [
        "user_message",
        "title_changed",
        "status",
        "turn_start",
        "thinking_delta",
        "text_delta",
        "text_done",
        "turn_done",
    ] {
        assert!(k.contains(&want.to_string()), "missing {want} in {k:?}");
    }
    let um = k.iter().position(|x| x == "user_message").unwrap();
    let ts = k.iter().position(|x| x == "turn_start").unwrap();
    let td_ = k.iter().position(|x| x == "turn_done").unwrap();
    assert!(um < ts && ts < td_);
    // turn 内的事件带 turn_id
    assert!(evs.iter().filter(|e| matches!(e.event, Event::TextDelta { .. })).all(|e| e.turn_id == Some(1)));
    assert!(matches!(&evs[td_].event, Event::TurnDone { turn_id: 1, reason: TurnDoneReason::Completed, .. }));
    // 最后一个状态是 idle
    let last_status = evs
        .iter()
        .rev()
        .find_map(|e| if let Event::Status { state, .. } = &e.event { Some(*state) } else { None })
        .unwrap();
    assert_eq!(last_status, SessionStatus::Idle);

    let s2 = get(&td, &mut c, &s.id).await;
    assert_eq!(s2.title, "say hello world please");
    assert_eq!(s2.preview.as_deref(), Some("say hello world please"));
    assert_eq!(s2.last_seq, evs.last().unwrap().seq);
    assert_eq!(s2.usage.turns, 1);
    assert_eq!(s2.usage.input_tokens, 10);
    assert_eq!(s2.usage.output_tokens, 20);
    assert!((s2.usage.cost_usd.unwrap() - 0.01).abs() < 1e-9);
    assert_eq!(s2.agent_session_id.as_deref(), Some("fake-agent-1"));

    // watcher 收到的 session.event 和历史一致（顺序、seq）
    let notes = c.drain_now();
    let watched: Vec<SessionEvent> = notes
        .iter()
        .filter(|n| n.method == methods::SESSION_EVENT)
        .map(|n| serde_json::from_value(n.params.clone()).unwrap())
        .collect();
    assert!(watched.len() >= evs.len() - 1);
    let hist_by_seq: std::collections::HashMap<u64, &SessionEvent> = evs.iter().map(|e| (e.seq, e)).collect();
    for w in &watched {
        assert_eq!(hist_by_seq[&w.seq], w);
    }
    assert!(watched.windows(2).all(|p| p[0].seq < p[1].seq));
    // session.changed 发给所有客户端（没 watch 的 b 也收到），且节流：一个 turn 内数量很少，最后一条反映 idle
    let mut b = td.client("b");
    let _ = send(&td, &mut c, &s.id, "second turn").await;
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await);
    let changed: Vec<SessionChanged> = b
        .drain_for(Duration::from_millis(1800))
        .await
        .into_iter()
        .filter(|n| n.method == methods::SESSION_CHANGED)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect();
    assert!(!changed.is_empty());
    assert!(changed.len() <= 4, "session.changed is rate limited to ~1/s/session, got {}", changed.len());
    assert_eq!(changed.last().unwrap().status, SessionStatus::Idle, "trailing edge carries the final state");
    assert!(b.drain_now().iter().all(|n| n.method != methods::SESSION_EVENT), "non-watchers get no session.event");

    // 分页：after_seq / limit / has_more / before_seq
    let page: SessionHistoryResult =
        c.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "after_seq": 0, "limit": 5 })).await.unwrap();
    assert_eq!(page.events.len(), 5);
    assert!(page.has_more);
    let back: SessionHistoryResult = c
        .call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "before_seq": 10, "limit": 4 }))
        .await
        .unwrap();
    assert_eq!(back.events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![6, 7, 8, 9]);
    assert!(back.has_more);
    let tail: SessionHistoryResult =
        c.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "after_seq": s2.last_seq })).await.unwrap();
    assert!(tail.events.len() > 1 && !tail.has_more, "second turn added events");
    td.shutdown().await;
}

#[tokio::test]
async fn send_is_idempotent_on_message_id() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let s = create(&td, &mut c, None).await;
    let p = json!({ "session_id": s.id, "message_id": "same-id", "content": "ping" });
    let r1: SessionSendResult = c.call(&td, methods::SESSION_SEND, p.clone()).await.unwrap();
    assert!(!r1.duplicate);
    let r2: SessionSendResult = c.call(&td, methods::SESSION_SEND, p.clone()).await.unwrap();
    assert!(r2.duplicate);
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await);
    let r3: SessionSendResult = c.call(&td, methods::SESSION_SEND, p).await.unwrap();
    assert!(r3.duplicate);
    assert_eq!(r3.turn_id, Some(1), "duplicate after completion reports the original turn");
    let evs = history(&td, &mut c, &s.id).await;
    assert_eq!(count(&evs, "user_message"), 1);
    assert_eq!(count(&evs, "turn_start"), 1);
    assert_eq!(td.fake.last_handle().input_texts(), vec!["ping".to_string()]);
    // create 带 prompt 的幂等：同 message_id 再 send 不重复
    let s2 = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "first", "message_id": "m-create" }),
        )
        .await
        .unwrap();
    let dup: SessionSendResult = c
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s2.id, "message_id": "m-create", "content": "first" }))
        .await
        .unwrap();
    assert!(dup.duplicate);
    // 参数校验
    let e = c
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "", "content": "x" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let e = c
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": "sess_nope", "message_id": "x", "content": "x" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    td.shutdown().await;
}

#[tokio::test]
async fn messages_arriving_during_a_turn_are_merged_into_the_next_turn() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let s = create(&td, &mut c, None).await;
    let _: Empty = c.call(&td, methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();
    let r = send(&td, &mut c, &s.id, "please hold").await;
    assert_eq!(r.turn_id, Some(1));
    assert!(eventually(T, || async { td.fake.spawn_count() == 1 }).await);
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Running).await);
    let r2 = send(&td, &mut c, &s.id, "first follow-up").await;
    assert!(r2.queued && r2.turn_id.is_none());
    let r3 = send(&td, &mut c, &s.id, "never mind, second one").await;
    assert!(r3.queued);
    // 状态事件告诉客户端有多少条待发送
    let evs = history(&td, &mut c, &s.id).await;
    let queued_counts: Vec<u32> = evs
        .iter()
        .filter_map(|e| if let Event::Status { queued_messages: Some(n), .. } = &e.event { Some(*n) } else { None })
        .collect();
    assert_eq!(queued_counts, vec![1, 2]);
    // 在 turn 里不会打断 agent：它只收到了第一条
    assert_eq!(td.fake.last_handle().input_texts(), vec!["please hold".to_string()]);
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.last_handle().input_texts().len() == 2 }).await);
    let inputs = td.fake.last_handle().input_texts();
    let merged = &inputs[1];
    assert!(merged.contains("first follow-up") && merged.contains("never mind, second one"), "{merged}");
    assert!(merged.contains(" UTC] ") && merged.contains("---"), "timestamps and separators: {merged}");
    assert!(merged.find("first follow-up").unwrap() < merged.find("never mind").unwrap(), "order preserved");
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await);
    let evs = history(&td, &mut c, &s.id).await;
    assert_eq!(count(&evs, "turn_start"), 2, "merged messages produce a single extra turn");
    assert_eq!(count(&evs, "user_message"), 3, "every message is still recorded");
    td.shutdown().await;
}

#[tokio::test]
async fn messages_sent_while_the_process_is_starting_are_merged_into_the_first_turn() {
    let td = TestDaemon::start().await;
    td.fake.set_spawn_delay(Duration::from_millis(400));
    let mut c = td.client("a");
    let s = create(&td, &mut c, None).await;
    let r1 = send(&td, &mut c, &s.id, "alpha").await;
    assert_eq!(r1.turn_id, Some(1));
    assert_eq!(get(&td, &mut c, &s.id).await.status, SessionStatus::Starting);
    let r2 = send(&td, &mut c, &s.id, "beta").await;
    assert!(r2.queued);
    assert!(wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await);
    let inputs = td.fake.last_handle().input_texts();
    assert_eq!(inputs.len(), 1, "one turn for both messages: {inputs:?}");
    assert!(inputs[0].contains("alpha") && inputs[0].contains("beta"));
    td.shutdown().await;
}

#[tokio::test]
async fn max_concurrent_queue_fifo_positions_and_queue_cancel() {
    let td = TestDaemon::start_with(&[("sessions.max_concurrent", "1")], false).await;
    let mut c = td.client("a");
    let s1 = create(&td, &mut c, None).await;
    let s2 = create(&td, &mut c, None).await;
    let s3 = create(&td, &mut c, None).await;
    let s4 = create(&td, &mut c, None).await;
    let _: Empty = c.call(&td, methods::SESSION_WATCH, json!({ "session_id": s2.id })).await.unwrap();
    send(&td, &mut c, &s1.id, "hold on").await;
    assert!(wait_status(&td, &mut c, &s1.id, SessionStatus::Running).await);
    let r2 = send(&td, &mut c, &s2.id, "two").await;
    let r3 = send(&td, &mut c, &s3.id, "three").await;
    let r4 = send(&td, &mut c, &s4.id, "four").await;
    assert!(r2.queued && r3.queued && r4.queued);
    let (g2, g3, g4) = (get(&td, &mut c, &s2.id).await, get(&td, &mut c, &s3.id).await, get(&td, &mut c, &s4.id).await);
    assert_eq!((g2.status, g2.queue_position), (SessionStatus::Queued, Some(1)));
    assert_eq!((g3.status, g3.queue_position), (SessionStatus::Queued, Some(2)));
    assert_eq!((g4.status, g4.queue_position), (SessionStatus::Queued, Some(3)));
    assert_eq!(td.fake.spawn_count(), 1, "only one process while max_concurrent = 1");
    // 排队中的会话有 status 事件带 position
    let evs2 = history(&td, &mut c, &s2.id).await;
    assert!(evs2
        .iter()
        .any(|e| matches!(e.event, Event::Status { state: SessionStatus::Queued, position: Some(1), .. })));
    // 取消排队中的 s3：回到 idle，s4 前移
    let _: Empty = c.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s3.id })).await.unwrap();
    assert_eq!(get(&td, &mut c, &s3.id).await.status, SessionStatus::Idle);
    let t0 = std::time::Instant::now();
    while get(&td, &mut c, &s4.id).await.queue_position != Some(2) {
        assert!(t0.elapsed() < T, "s4 should move up the queue");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // 放行 s1：FIFO，s2 先跑，s4 随后
    td.fake.handle(0).release();
    assert!(wait_status(&td, &mut c, &s1.id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut c, &s2.id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut c, &s4.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut c, &s3.id).await.usage.turns, 0, "cancelled-in-queue session never ran");
    assert_eq!(td.fake.spawn_count(), 3);
    let (sc, procs) = td.ctx.sessions.counts().await;
    assert_eq!(sc.running + sc.queued, 0);
    assert!(procs <= 3);
    // max_concurrent 热加载：调大后排队的立即补位
    td.shutdown().await;
}

#[tokio::test]
async fn max_concurrent_hot_reload_starts_queued_sessions() {
    let td = TestDaemon::start_with(&[("sessions.max_concurrent", "1")], false).await;
    let mut c = td.client("a");
    let s1 = create(&td, &mut c, None).await;
    let s2 = create(&td, &mut c, None).await;
    send(&td, &mut c, &s1.id, "hold").await;
    assert!(wait_status(&td, &mut c, &s1.id, SessionStatus::Running).await);
    send(&td, &mut c, &s2.id, "hold too").await;
    assert_eq!(get(&td, &mut c, &s2.id).await.status, SessionStatus::Queued);
    anycon_daemon::config::set_value(&td.paths, "sessions.max_concurrent", "2").unwrap();
    td.ctx.config.reload().unwrap();
    assert!(wait_status(&td, &mut c, &s2.id, SessionStatus::Running).await);
    td.shutdown().await;
}

#[tokio::test]
async fn permission_round_trip_first_answer_wins_and_broadcast() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let s = create(&td, &mut a, None).await;
    send(&td, &mut a, &s.id, "do a permission thing").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    // 两个客户端（都没 watch）都收到 permission.request
    let na =
        a.wait_notification(T, |n| n.method == methods::PERMISSION_REQUEST).await.expect("a gets permission.request");
    let nb =
        b.wait_notification(T, |n| n.method == methods::PERMISSION_REQUEST).await.expect("b gets permission.request");
    let req: PermissionRequest = serde_json::from_value(na.params).unwrap();
    assert_eq!(serde_json::from_value::<PermissionRequest>(nb.params).unwrap(), req);
    assert_eq!(req.kind, PermissionKind::Tool);
    assert_eq!(req.session_id, s.id);
    assert_eq!(req.options.len(), 3);
    // permission.list 和摘要里的 pending_permissions
    let list: PermissionListResult = b.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
    assert_eq!(list.requests.len(), 1);
    let list2: PermissionListResult =
        b.call(&td, methods::PERMISSION_LIST, json!({ "session_id": "other" })).await.unwrap();
    assert!(list2.requests.is_empty());
    assert_eq!(get(&td, &mut a, &s.id).await.pending_permissions, 1);
    // 不存在的选项
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "nope" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // b 先答复，a 的答复随后 -> AlreadyResolved
    let _: Empty = b
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "deny" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyResolved);
    // 所有人（包括答复者）收到 permission.resolved，带 by_device
    let ra: PermissionResolved = serde_json::from_value(
        a.wait_notification(T, |n| n.method == methods::PERMISSION_RESOLVED).await.unwrap().params,
    )
    .unwrap();
    assert_eq!(
        (ra.request_id.as_str(), ra.option_id.as_deref(), ra.by_device.as_str()),
        (req.request_id.as_str(), Some("allow"), "dev-b")
    );
    assert!(b.wait_notification(T, |n| n.method == methods::PERMISSION_RESOLVED).await.is_some());
    // 完全不存在的 id
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": "ghost", "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // agent 收到的决定
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let decisions = td.fake.last_handle().decisions.lock().unwrap().clone();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].1.option_id.as_deref(), Some("allow"));
    // 事件流里有 permission、permission_resolved（by_device）、tool_call/tool_result
    let evs = history(&td, &mut a, &s.id).await;
    let k = kinds(&evs);
    assert!(
        k.contains(&"permission".into())
            && k.contains(&"permission_resolved".into())
            && k.contains(&"tool_result".into()),
        "{k:?}"
    );
    let resolved = evs
        .iter()
        .find_map(|e| {
            if let Event::PermissionResolved { by_device, option_id, .. } = &e.event {
                Some((by_device.clone(), option_id.clone()))
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(resolved, ("dev-b".to_string(), Some("allow".to_string())));
    // 状态：waiting_permission -> running -> idle
    let states: Vec<SessionStatus> = evs
        .iter()
        .filter_map(|e| if let Event::Status { state, .. } = &e.event { Some(*state) } else { None })
        .collect();
    let wp = states.iter().position(|s| *s == SessionStatus::WaitingPermission).unwrap();
    assert_eq!(states[wp + 1], SessionStatus::Running);
    assert_eq!(*states.last().unwrap(), SessionStatus::Idle);
    // 已处理的请求，重复答复在重启后仍是 AlreadyResolved（落库的 permission_resolved）
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyResolved);
    td.shutdown().await;
}

#[tokio::test]
async fn permission_deny_with_message_question_and_mode_change() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    // deny + message
    send(&td, &mut a, &s.id, "need permission").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(
            &td,
            methods::PERMISSION_RESPOND,
            json!({ "request_id": req.request_id, "option_id": "deny", "message": "not now" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let d = td.fake.last_handle().decisions.lock().unwrap().clone();
    assert_eq!((d[0].1.option_id.as_deref(), d[0].1.message.as_deref()), (Some("deny"), Some("not now")));
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::TextDelta { text } if text == "skipped")));
    // 答复附的话记进历史（客户端显示「拒绝：not now」）
    assert!(evs.iter().any(|e| matches!(&e.event,
        Event::PermissionResolved { message: Some(m), answers: None, .. } if m == "not now")));
    // question：不带 option_id，用 answers
    send(&td, &mut a, &s.id, "a question please").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    assert_eq!(req.kind, PermissionKind::Question);
    let e =
        a.call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid, "a question needs answers");
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "answers": { "q1": "blue" } }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let d = td.fake.last_handle().decisions.lock().unwrap().clone();
    assert_eq!(d.last().unwrap().1.answers.as_ref().unwrap()["q1"], json!("blue"));
    // 回答记进历史：重新打开会话、换一台设备都能看到问题和回答
    let evs = history(&td, &mut a, &s.id).await;
    let answers = evs.iter().rev().find_map(|e| match &e.event {
        Event::PermissionResolved { request_id, answers, .. } if *request_id == req.request_id => answers.clone(),
        _ => None,
    });
    assert_eq!(answers.map(serde_json::Value::Object), Some(json!({ "q1": "blue" })));
    // mode_change：同意后 daemon 切 mode 并发 settings_changed
    send(&td, &mut a, &s.id, "modechange now").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    assert_eq!(req.kind, PermissionKind::ModeChange);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s.id).await.settings.mode.as_deref(), Some("plan"));
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs
        .iter()
        .rev()
        .any(|e| matches!(&e.event, Event::SettingsChanged { settings } if settings.mode.as_deref() == Some("plan"))));
    td.shutdown().await;
}

#[tokio::test]
async fn permission_request_timeout_auto_denies() {
    let td = TestDaemon::start_with(&[("permissions.request_timeout", "\"1s\"")], false).await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    send(&td, &mut a, &s.id, "permission with timeout").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    assert!(req.expires_at_ms.is_some());
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await, "auto-deny lets the turn finish");
    let d = td.fake.last_handle().decisions.lock().unwrap().clone();
    assert_eq!((d[0].1.option_id.as_deref(), d[0].1.message.as_deref()), (Some("deny"), Some("timeout")));
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs
        .iter()
        .any(|e| matches!(&e.event, Event::PermissionResolved { by_device, .. } if by_device == "daemon")));
    let e = a
        .call::<Empty>(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyResolved);
    td.shutdown().await;
}

#[tokio::test]
async fn cancel_running_turn_and_pending_permissions() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    // 取消正在输出的 turn
    send(&td, &mut a, &s.id, "go slow").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let t = std::time::Instant::now();
    let _: Empty = a.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(t.elapsed() < Duration::from_millis(900), "cancel does not wait for the slow turn to finish");
    assert_eq!(td.fake.last_handle().cancels.load(std::sync::atomic::Ordering::SeqCst), 1);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Cancelled, .. })));
    assert_eq!(td.fake.spawn_count(), 1, "cancel does not kill the process");
    // 对 idle 会话取消是幂等无操作
    let _: Empty = a.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    // 取消带未决权限的 turn：权限自动 deny
    send(&td, &mut a, &s.id, "permission cancel me").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s.id).await.pending_permissions, 0);
    let d = td.fake.last_handle().decisions.lock().unwrap().clone();
    assert!(d.iter().any(|(id, dec)| id == &req.request_id
        && dec.option_id.as_deref() == Some("deny")
        && dec.message.as_deref() == Some("cancelled")));
    // 取消之后还能继续聊
    send(&td, &mut a, &s.id, "still there?").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(history(&td, &mut a, &s.id)
        .await
        .iter()
        .any(|e| matches!(&e.event, Event::TextDelta { text } if text == "echo: still there?")));
    td.shutdown().await;
}

#[tokio::test]
async fn idle_timeout_shuts_down_the_process_and_resumes_on_next_send() {
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("first")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 1);
    let h0 = td.fake.handle(0);
    assert_eq!(h0.shutdowns.load(std::sync::atomic::Ordering::SeqCst), 0);
    // 超时后进程被 shutdown，但会话仍是 idle（对客户端不可见）
    assert!(
        eventually(Duration::from_secs(6), || async { h0.shutdowns.load(std::sync::atomic::Ordering::SeqCst) == 1 })
            .await,
        "idle timeout should shut the process down"
    );
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Idle);
    assert_eq!(td.ctx.sessions.counts().await.1, 0, "no live process");
    // 下一次 send：重新拉起并 resume 同一个 agent 会话
    send(&td, &mut a, &s.id, "second").await;
    assert!(eventually(T, || async { td.fake.spawn_count() == 2 }).await);
    let spawns = td.fake.spawns.lock().unwrap().clone();
    assert!(spawns[0].resume.is_none());
    assert_eq!(spawns[1].resume.as_ref().unwrap().agent_session_id, "fake-agent-1");
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(history(&td, &mut a, &s.id)
        .await
        .iter()
        .any(|e| matches!(&e.event, Event::TextDelta { text } if text == "echo: second")));
    // 用户无感：没有 error 事件
    assert_eq!(count(&history(&td, &mut a, &s.id).await, "error"), 0);
    td.shutdown().await;
}

#[tokio::test]
async fn idle_timeout_zero_or_non_resumable_agents_keep_the_process() {
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"0\"")], false).await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hi")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst), 0, "\"0\" = keep alive");
    td.shutdown().await;

    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    td.fake.resume_supported.store(false, std::sync::atomic::Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hi")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "agents that cannot resume are never idle-shut-down"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn settings_validation_defaults_reconcile_and_dangerous_gate() {
    let td = TestDaemon::start_with(
        &[("agents.fake.default_model", "\"fake-large\""), ("agents.fake.default_thinking", "\"low\"")],
        false,
    )
    .await;
    let mut a = td.client("a");
    // 默认值：config default_* -> AgentInfo default
    let s = create(&td, &mut a, None).await;
    assert_eq!(s.settings.model.as_deref(), Some("fake-large"));
    assert_eq!(s.settings.thinking.as_deref(), Some("low"));
    assert_eq!(s.settings.mode.as_deref(), Some("ask"));
    // 创建时校验
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "model": "nope" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    assert_eq!(e.data.unwrap().field.as_deref(), Some("model"));
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "model": "fake-small", "thinking": "low" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("thinking"));
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "mode": "nope" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("mode"));
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "options": { "mystery": 1 } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("options.mystery"));
    let e = a.call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "nope" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AgentUnavailable);
    // 只改 model：thinking 回落到新模型的默认，SettingsChanged 里是实际生效的完整设置
    let r: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "fake-small" } }))
        .await
        .unwrap();
    assert_eq!(r.settings.model.as_deref(), Some("fake-small"));
    assert_eq!(r.settings.thinking, None);
    let evs = history(&td, &mut a, &s.id).await;
    let last_sc = evs
        .iter()
        .rev()
        .find_map(|e| if let Event::SettingsChanged { settings } = &e.event { Some(settings.clone()) } else { None })
        .unwrap();
    assert_eq!(last_sc, r.settings);
    // 切回有 thinking 的模型，同时不给 thinking：回落到 high（模型默认）
    let r: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "fake-large" } }))
        .await
        .unwrap();
    assert_eq!(r.settings.model.as_deref(), Some("fake-large"));
    assert!(r.settings.thinking.is_none() || r.settings.thinking.as_deref() == Some("high"));
    // 同时给了不兼容的 model 和 thinking：报错而不是静默回落
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "model": "fake-small", "thinking": "high" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    // 持久化：重启后设置还在
    // 危险模式门禁：允许时可选，禁止时 Forbidden（create 与 set_settings 都拦）
    let ok = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "mode": "yolo" } }),
        )
        .await
        .unwrap();
    assert_eq!(ok.settings.mode.as_deref(), Some("yolo"));
    anycon_daemon::config::set_value(&td.paths, "permissions.allow_dangerous_modes", "false").unwrap();
    td.ctx.config.reload().unwrap();
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "mode": "yolo" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "yolo" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    // 受保护的环境变量
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "env": { "PATH": "/evil" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    // 审计：危险模式的选择被记录
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let create_entries: Vec<&AuditEntry> = audit.entries.iter().filter(|e| e.action == "session.create").collect();
    assert!(create_entries.iter().any(|e| e.detail["dangerous"] == json!(true)));
    td.shutdown().await;
}

#[tokio::test]
async fn set_settings_is_busy_during_a_turn_and_live_changes_reach_the_agent() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hold please")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "plan" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    td.fake.last_handle().release();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    // 空闲时改：热生效（agent 收到 set_settings，不重启）
    let _: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "plan", "options": { "verbose": true } } }),
        )
        .await
        .unwrap();
    let h = td.fake.last_handle();
    let applied = h.settings.lock().unwrap().clone();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].mode.as_deref(), Some("plan"));
    assert_eq!(applied[0].options["verbose"], json!(true));
    send(&td, &mut a, &s.id, "again").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 1, "applied live; no restart");
    // 没有变化的 set_settings 不产生事件
    let before = get(&td, &mut a, &s.id).await.last_seq;
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "mode": "plan" } }))
        .await
        .unwrap();
    assert_eq!(get(&td, &mut a, &s.id).await.last_seq, before);
    td.shutdown().await;
}

#[tokio::test]
async fn restart_required_settings_restart_lazily_with_resume() {
    let td = TestDaemon::start().await;
    td.fake.restart_on_settings.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("one")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "fake-small" } }))
        .await
        .unwrap();
    assert_eq!(td.fake.spawn_count(), 1, "restart is deferred until the next turn");
    send(&td, &mut a, &s.id, "two").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 2);
    assert_eq!(td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst), 1);
    let spawns = td.fake.spawns.lock().unwrap().clone();
    assert_eq!(spawns[1].settings.model.as_deref(), Some("fake-small"));
    assert_eq!(spawns[1].resume.as_ref().unwrap().agent_session_id, "fake-agent-1");
    td.shutdown().await;
}

#[tokio::test]
async fn explicit_restart_kills_and_resumes() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hold forever")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Idle);
    assert_eq!(td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst), 1);
    send(&td, &mut a, &s.id, "back again").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 2);
    assert!(td.fake.spawns.lock().unwrap()[1].resume.is_some());
    td.shutdown().await;
}

#[tokio::test]
async fn agent_crash_becomes_error_and_next_send_recovers() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("please crash")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs
        .iter()
        .any(|e| matches!(&e.event, Event::Error { code, recoverable: true, .. } if code == "agent_exited")));
    assert!(evs.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Error, .. })));
    send(&td, &mut a, &s.id, "are you ok").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 2);
    assert!(td.fake.spawns.lock().unwrap()[1].resume.is_some(), "recoverable error resumes");
    // adapter 发 error 事件 + turn_done(error)：会话回到 idle，错误留在历史里
    send(&td, &mut a, &s.id, "this will error").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert!(history(&td, &mut a, &s.id)
        .await
        .iter()
        .any(|e| matches!(&e.event, Event::Error { code, .. } if code == "fake_error")));
    // 正常退出 -> ended；ended 且 agent 能 resume 的会话还能继续
    send(&td, &mut a, &s.id, "now exit").await;
    let t0 = std::time::Instant::now();
    loop {
        let st = get(&td, &mut a, &s.id).await.status;
        if st == SessionStatus::Ended {
            break;
        }
        assert!(t0.elapsed() < T, "session should end after the agent exits, got {st:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    td.shutdown().await;
}

#[tokio::test]
async fn spawn_failures_report_agent_errors_and_trigger_reprobe() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    *td.fake.fail_spawn.lock().unwrap() = Some(AdapterErrorKind::NotAuthenticated);
    let probes_before = td.fake.probe_calls.load(std::sync::atomic::Ordering::SeqCst);
    send(&td, &mut a, &s.id, "hello").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "auth_failed")));
    assert!(
        eventually(T, || async { td.fake.probe_calls.load(std::sync::atomic::Ordering::SeqCst) > probes_before }).await,
        "a failed spawn re-probes the agent"
    );
    // agent 变得不可用 -> create / send 都被拒绝，data.reason 说明原因
    *td.fake.status.lock().unwrap() = AgentStatus::NotAuthenticated;
    td.ctx.agents.refresh_all().await;
    let e = a.call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AgentUnavailable);
    assert_eq!(e.data.unwrap().reason.as_deref(), Some("notauthenticated"));
    let e = a
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "x", "content": "hi" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AgentUnavailable);
    // 恢复
    *td.fake.status.lock().unwrap() = AgentStatus::Ready;
    *td.fake.fail_spawn.lock().unwrap() = None;
    td.ctx.agents.refresh_all().await;
    send(&td, &mut a, &s.id, "now it works").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    td.shutdown().await;
}

#[tokio::test]
async fn daemon_restart_marks_running_sessions_interrupted_and_resumes_on_send() {
    let mut td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hold this")).await;
    let idle = create(&td, &mut a, Some("quick")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    assert!(wait_status(&td, &mut a, &idle.id, SessionStatus::Idle).await);
    td.restart().await;
    let mut a = td.client("a");
    let after = get(&td, &mut a, &s.id).await;
    assert_eq!(after.status, SessionStatus::Interrupted);
    assert_eq!(after.agent_session_id.as_deref(), Some("fake-agent-1"));
    assert_eq!(get(&td, &mut a, &idle.id).await.status, SessionStatus::Idle);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(matches!(evs.last().unwrap().event, Event::Status { state: SessionStatus::Interrupted, .. }));
    // 历史、seq 延续
    let seq_before = after.last_seq;
    let spawns_before = td.fake.spawn_count();
    send(&td, &mut a, &s.id, "continue").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawns = td.fake.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), spawns_before + 1);
    assert_eq!(spawns.last().unwrap().resume.as_ref().unwrap().agent_session_id, "fake-agent-1");
    assert!(get(&td, &mut a, &s.id).await.last_seq > seq_before);
    // turn id 单调递增、不重用
    let evs = history(&td, &mut a, &s.id).await;
    let turns: Vec<u64> = evs
        .iter()
        .filter_map(|e| if let Event::TurnStart { turn_id, .. } = &e.event { Some(*turn_id) } else { None })
        .collect();
    assert_eq!(turns, vec![1, 2]);
    // 在 interrupted 状态下不支持 resume 的 agent：转 ended 并报错
    td.shutdown().await;
}

#[tokio::test]
async fn interrupted_session_of_a_non_resumable_agent_becomes_ended() {
    let mut td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hold on")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    td.fake.resume_supported.store(false, std::sync::atomic::Ordering::SeqCst);
    td.restart().await;
    td.ctx.agents.refresh_all().await;
    let mut a = td.client("a");
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Interrupted);
    let e = a
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "x", "content": "hi" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Ended);
    td.shutdown().await;
}

#[tokio::test]
async fn watch_limit_unwatch_and_event_routing() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let mut sessions = Vec::new();
    for _ in 0..6 {
        sessions.push(create(&td, &mut a, None).await);
    }
    for s in sessions.iter().take(5) {
        let _: Empty = a.call(&td, methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();
    }
    let e = a.call::<Empty>(&td, methods::SESSION_WATCH, json!({ "session_id": sessions[5].id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let e = a.call::<Empty>(&td, methods::SESSION_WATCH, json!({ "session_id": "sess_ghost" })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let _: Empty = a.call(&td, methods::SESSION_UNWATCH, json!({ "session_id": sessions[0].id })).await.unwrap();
    let _: Empty = a.call(&td, methods::SESSION_WATCH, json!({ "session_id": sessions[5].id })).await.unwrap();
    a.drain_now();
    b.drain_now();
    // 事件只发给 watcher
    send(&td, &mut a, &sessions[1].id, "to watched").await;
    send(&td, &mut a, &sessions[0].id, "to unwatched").await;
    assert!(wait_status(&td, &mut a, &sessions[1].id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut a, &sessions[0].id, SessionStatus::Idle).await);
    let notes = a.drain_for(Duration::from_millis(200)).await;
    let ev_sessions: std::collections::HashSet<String> = notes
        .iter()
        .filter(|n| n.method == methods::SESSION_EVENT)
        .map(|n| n.params["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ev_sessions.contains(&sessions[1].id) && !ev_sessions.contains(&sessions[0].id));
    assert!(b.drain_now().iter().all(|n| n.method != methods::SESSION_EVENT));
    td.shutdown().await;
}

#[tokio::test]
async fn archive_rename_delete_and_listing() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s1 = create(&td, &mut a, Some("first session")).await;
    let s2 = create(&td, &mut a, Some("second session")).await;
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Idle).await);
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    // rename 锁定标题，之后 agent 的 Title 不覆盖
    let _: Empty =
        a.call(&td, methods::SESSION_RENAME, json!({ "session_id": s1.id, "title": "  My title  " })).await.unwrap();
    assert_eq!(get(&td, &mut a, &s1.id).await.title, "My title");
    send(&td, &mut a, &s1.id, "set a title now").await;
    assert!(wait_status(&td, &mut a, &s1.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s1.id).await.title, "My title");
    let e =
        a.call::<Empty>(&td, methods::SESSION_RENAME, json!({ "session_id": s1.id, "title": "  " })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 没锁定的会话：agent 起的标题优先
    send(&td, &mut a, &s2.id, "set a title now").await;
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s2.id).await.title, "Agent title");
    // 归档：默认列表不显示
    let _: Empty = a.call(&td, methods::SESSION_ARCHIVE, json!({ "session_id": s1.id })).await.unwrap();
    let l: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(l.sessions.len(), 1);
    assert_eq!(l.sessions[0].id, s2.id);
    let l: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({ "include_archived": true })).await.unwrap();
    assert_eq!(l.sessions.len(), 2);
    assert!(l.sessions.iter().find(|s| s.id == s1.id).unwrap().archived);
    let _: Empty = a.call(&td, methods::SESSION_UNARCHIVE, json!({ "session_id": s1.id })).await.unwrap();
    assert_eq!(a.call::<SessionListResult>(&td, methods::SESSION_LIST, json!({})).await.unwrap().sessions.len(), 2);
    // 运行中不能归档
    let s3 = create(&td, &mut a, Some("hold")).await;
    assert!(wait_status(&td, &mut a, &s3.id, SessionStatus::Running).await);
    let e = a.call::<Empty>(&td, methods::SESSION_ARCHIVE, json!({ "session_id": s3.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    // 删除：事件、scratch 目录、搜索索引都清掉；进程被关；其他客户端收到 deleted
    let mut b = td.client("b");
    let scratch = td.workspace.join(".scratch").join(&s3.id);
    assert!(scratch.is_dir());
    let _: Empty = a.call(&td, methods::SESSION_DELETE, json!({ "session_id": s3.id })).await.unwrap();
    assert!(!scratch.exists());
    assert_eq!(td.fake.last_handle().shutdowns.load(std::sync::atomic::Ordering::SeqCst), 1);
    let e = a.call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": s3.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert!(td.ctx.store.events_after(&s3.id, 0, 10).await.unwrap().is_empty());
    let ch =
        b.wait_notification(T, |n| n.method == methods::SESSION_CHANGED && n.params["deleted"] == json!(true)).await;
    assert!(ch.is_some());
    td.shutdown().await;
}

#[tokio::test]
async fn full_text_search_over_user_and_assistant_text() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("please refactor the zebra module")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let other = create(&td, &mut a, Some("something entirely different")).await;
    assert!(wait_status(&td, &mut a, &other.id, SessionStatus::Idle).await);
    let r: SessionSearchResult = a.call(&td, methods::SESSION_SEARCH, json!({ "query": "zebra" })).await.unwrap();
    // 用户消息和助手回显（"echo: please refactor the zebra module"）各命中一次
    assert_eq!(r.hits.len(), 2, "{:?}", r.hits);
    assert!(r.hits.iter().all(|h| h.session_id == s.id));
    assert!(r.hits.iter().all(|h| h.snippet.contains("<mark>") && h.title.contains("zebra")));
    assert!(r.hits.iter().any(|h| h.snippet.contains("echo")), "assistant text is indexed");
    let none: SessionSearchResult = a.call(&td, methods::SESSION_SEARCH, json!({ "query": "giraffe" })).await.unwrap();
    assert!(none.hits.is_empty());
    let e = a.call::<SessionSearchResult>(&td, methods::SESSION_SEARCH, json!({ "query": "  " })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    td.shutdown().await;
}

#[tokio::test]
async fn project_sessions_use_the_project_directory_and_reject_bad_names() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "my-app" })).await.unwrap();
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "my-app", "prompt": "hi", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert_eq!(s.project.as_deref(), Some("my-app"));
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawn = td.fake.spawns.lock().unwrap()[0].clone();
    assert_eq!(spawn.cwd, td.workspace.join("my-app"));
    let e = a
        .call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "ghost" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = a
        .call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "../etc" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 项目的 active_sessions
    let l: WorkspaceListResult = a.call(&td, methods::WORKSPACE_LIST, json!({})).await.unwrap();
    assert_eq!(l.projects[0].active_sessions, 1);
    // 活跃会话挡住项目的改名 / 回收
    let e = a
        .call::<RenamedProject>(&td, methods::WORKSPACE_RENAME, json!({ "from": "my-app", "to": "other" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "a live process pins the project");
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    let r: RenamedProject =
        a.call(&td, methods::WORKSPACE_RENAME, json!({ "from": "my-app", "to": "renamed" })).await.unwrap();
    assert_eq!(r.to, "renamed");
    assert_eq!(get(&td, &mut a, &s.id).await.project.as_deref(), Some("renamed"));
    td.shutdown().await;
}

#[tokio::test]
async fn promote_moves_the_scratch_directory_to_a_project() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("write a script")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let scratch = td.workspace.join(".scratch").join(&s.id);
    std::fs::write(scratch.join("script.sh"), "echo hi").unwrap();
    let _: Empty =
        a.call(&td, methods::SESSION_PROMOTE, json!({ "session_id": s.id, "name": "my-script" })).await.unwrap();
    assert!(!scratch.exists());
    assert_eq!(std::fs::read_to_string(td.workspace.join("my-script/script.sh")).unwrap(), "echo hi");
    let after = get(&td, &mut a, &s.id).await;
    assert_eq!(after.project.as_deref(), Some("my-script"));
    assert_eq!(
        td.fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "process in the old cwd is shut down"
    );
    // 之后的 send 在新目录里 resume
    send(&td, &mut a, &s.id, "continue in the project").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawns.lock().unwrap()[1].cwd, td.workspace.join("my-script"));
    // 已是项目会话 / 名字冲突 / 非法名字
    let e = a
        .call::<Empty>(&td, methods::SESSION_PROMOTE, json!({ "session_id": s.id, "name": "again" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let s2 = create(&td, &mut a, Some("another")).await;
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    let e = a
        .call::<Empty>(&td, methods::SESSION_PROMOTE, json!({ "session_id": s2.id, "name": "my-script" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists);
    let e = a
        .call::<Empty>(&td, methods::SESSION_PROMOTE, json!({ "session_id": s2.id, "name": ".hidden" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    td.shutdown().await;
}

#[tokio::test]
async fn interject_capable_agents_receive_messages_during_a_turn() {
    let td = TestDaemon::start().await;
    td.fake.interject.store(true, std::sync::atomic::Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hold on")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let r = send(&td, &mut a, &s.id, "interjection").await;
    assert_eq!(r.turn_id, Some(1));
    assert!(!r.queued);
    assert!(eventually(T, || async { td.fake.last_handle().input_texts().len() == 2 }).await);
    td.fake.last_handle().release();
    td.shutdown().await;
}

#[tokio::test]
async fn spawn_environment_layers_config_and_session_env() {
    let td = TestDaemon::start_with(
        &[
            ("permissions.allow_session_env", "true"),
            ("permissions.session_env_allow", "[\"SESSION_VAR\", \"AGENT_VAR\", \"GLOBAL_VAR\"]"),
            ("agents.env", "{ GLOBAL_VAR = \"g\" }"),
            ("agents.fake.env", "{ AGENT_VAR = \"a\", GLOBAL_VAR = \"agent-wins\" }"),
        ],
        false,
    )
    .await;
    let mut a = td.client("a");
    let s: SessionSummary = a
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": "m", "settings": { "env": { "SESSION_VAR": "s" } } }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let env = td.fake.spawns.lock().unwrap()[0].env.clone();
    assert_eq!(env["GLOBAL_VAR"], "agent-wins");
    assert_eq!(env["AGENT_VAR"], "a", "operator config is never overridden by a session");
    assert_eq!(env["SESSION_VAR"], "s");
    assert!(env.contains_key("PATH"));
    assert!(!env.contains_key("CLAUDECODE"), "agent-session markers are stripped (once, by the daemon)");
    // 会话 env 不能覆盖运维配置的键
    for key in ["AGENT_VAR", "GLOBAL_VAR"] {
        let e = a
            .call::<SessionSummary>(
                &td,
                methods::SESSION_CREATE,
                json!({ "agent": "fake", "settings": { "env": { key: "session-wins" } } }),
            )
            .await
            .unwrap_err();
        assert_eq!(e.data.unwrap().field.as_deref(), Some(format!("env.{key}").as_str()));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn session_env_is_rejected_by_default_and_filtered_per_key_class_when_enabled() {
    // 默认关闭：任何非空 env 都是 invalid_settings { field: "env" }（创建和 set_settings 都一样）
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "env": { "FOO": "bar" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    assert_eq!(e.data.unwrap().field.as_deref(), Some("env"));
    let s = create(&td, &mut a, None).await;
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "env": { "FOO": "bar" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("env"));
    // 空 env 照常
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "env": {} } }))
        .await
        .unwrap();
    td.shutdown().await;

    // 开关打开但允许列表为空（默认）：任何键都被拒，reason 指向允许列表
    let td = TestDaemon::start_with(&[("permissions.allow_session_env", "true")], false).await;
    let mut a = td.client("a");
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "env": { "MY_FLAG": "1" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    let data = e.data.unwrap();
    assert_eq!(data.field.as_deref(), Some("env.MY_FLAG"));
    assert!(data.reason.as_deref().unwrap_or("").contains("session_env_allow"), "{data:?}");
    td.shutdown().await;

    // 允许列表 + 黑名单：列表里的合规键通过；不在列表里的被拒（原因是允许列表）；黑名单里的键即使运维写进了允许列表也被拒
    // （原因是黑名单）——包括能执行代码的 SHELLOPTS / PS4 / LESSOPEN 等
    let td = TestDaemon::start_with(
        &[
            ("permissions.allow_session_env", "true"),
            (
                "permissions.session_env_allow",
                r#"["MY_FLAG", "THIRD", "SHELLOPTS", "PS4", "LESSOPEN", "LUA_INIT", "npm_config_registry", "RUSTC_WRAPPER", "PATH", "HOME", "SHELL", "USER", "TMPDIR", "BASH_ENV", "ENV", "ZDOTDIR", "PROMPT_COMMAND", "LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH", "NODE_OPTIONS", "NODE_PATH", "ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR", "CLAUDECODE", "ANYCON_HOME", "HTTP_PROXY", "https_proxy", "ALL_PROXY", "GIT_SSH_COMMAND", "GIT_CONFIG_GLOBAL", "PYTHONPATH", "PYTHONSTARTUP", "RUBYOPT", "PERL5OPT", "XDG_CONFIG_HOME", "SSL_CERT_FILE", "EDITOR", "AWS_PROFILE"]"#,
            ),
        ],
        false,
    )
    .await;
    let mut a = td.client("a");
    // 不在允许列表里：被拒，原因是允许列表
    let e = a
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "env": { "OTHER": "1" } } }),
        )
        .await
        .unwrap_err();
    let data = e.data.unwrap();
    assert_eq!(data.field.as_deref(), Some("env.OTHER"));
    assert!(data.reason.as_deref().unwrap_or("").contains("session_env_allow"), "{data:?}");
    for key in [
        "PATH",
        "HOME",
        "SHELL",
        "USER",
        "TMPDIR",
        "BASH_ENV",
        "ENV",
        "ZDOTDIR",
        "PROMPT_COMMAND",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "NODE_OPTIONS",
        "NODE_PATH",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CONFIG_DIR",
        "CLAUDECODE",
        "ANYCON_HOME",
        "HTTP_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "GIT_SSH_COMMAND",
        "GIT_CONFIG_GLOBAL",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "RUBYOPT",
        "PERL5OPT",
        "XDG_CONFIG_HOME",
        "SSL_CERT_FILE",
        "EDITOR",
        "AWS_PROFILE",
        // 已知能执行代码的几个，和同类（Lua / 包管理器 / 编译器 wrapper）
        "SHELLOPTS",
        "PS4",
        "LESSOPEN",
        "LUA_INIT",
        "npm_config_registry",
        "RUSTC_WRAPPER",
        "1BAD",
        "BAD-KEY",
        "BAD KEY",
        "",
    ] {
        let e = a
            .call::<SessionSummary>(
                &td,
                methods::SESSION_CREATE,
                json!({ "agent": "fake", "settings": { "env": { key: "x" } } }),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidSettings, "{key:?}");
        let data = e.data.unwrap();
        assert_eq!(data.field.as_deref(), Some(format!("env.{key}").as_str()), "{key:?}");
        // 黑名单的键即使在允许列表里，原因也是黑名单（不是「不在允许列表」）；不在列表里的非法名才会走允许列表
        let reason = data.reason.unwrap_or_default();
        if ["1BAD", "BAD-KEY", "BAD KEY", ""].contains(&key) {
            assert!(
                reason.contains("invalid variable name") || reason.contains("session_env_allow"),
                "{key:?}: {reason}"
            );
        } else {
            assert!(reason.contains("denylisted") && !reason.contains("session_env_allow"), "{key:?}: {reason}");
        }
    }
    // 合规的键通过；审计记键名不记值
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "settings": { "env": { "MY_FLAG": "secret-value", "THIRD": "1" } } }),
        )
        .await
        .unwrap();
    assert_eq!(s.settings.env.len(), 2);
    let r: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "env": { "MY_FLAG": "2" } } }),
        )
        .await
        .unwrap();
    assert_eq!(r.settings.env.len(), 2, "merged into the existing env");
    // 在允许列表之外的键，set_settings 同样被拒
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "env": { "OTHER": "3" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("env.OTHER"));
    // `clear: ["env"]` 清空整个 env
    let r: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": {}, "clear": ["env"] }))
        .await
        .unwrap();
    assert!(r.settings.env.is_empty());
    let r: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "env": { "MY_FLAG": "secret-value", "THIRD": "1" } } }),
        )
        .await
        .unwrap();
    assert_eq!(r.settings.env.keys().cloned().collect::<Vec<_>>(), vec!["MY_FLAG", "THIRD"]);
    // set_settings 里的违规键同样被拒
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "env": { "NODE_OPTIONS": "--require /x" } } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("env.NODE_OPTIONS"));
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let created = audit.entries.iter().find(|e| e.action == "session.create").unwrap();
    let mut keys: Vec<String> = serde_json::from_value(created.detail["env_keys"].clone()).unwrap();
    keys.sort();
    assert_eq!(keys, vec!["MY_FLAG", "THIRD"]);
    assert!(audit.entries.iter().any(|e| e.detail["cleared"] == json!(["env"])), "clear is audited");
    assert!(!audit.entries.iter().any(|e| e.detail.to_string().contains("secret-value")), "values are never audited");
    let set = audit.entries.iter().find(|e| e.action == "session.set_settings").unwrap();
    assert!(set.detail["env_keys"].is_array());
    // 关掉开关之后，已有 env 的会话下一个 turn 也不能再带着它跑（拉起前复核）
    anycon_daemon::config::set_value(&td.paths, "permissions.allow_session_env", "false").unwrap();
    td.ctx.config.reload().unwrap();
    send(&td, &mut a, &s.id, "hello").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "invalid_settings")));
    assert_eq!(td.fake.spawn_count(), 0, "nothing was spawned with a now-forbidden env");
    td.shutdown().await;
}

#[tokio::test]
async fn plan_approval_switches_the_mode_back_and_never_to_a_dangerous_one() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    assert_eq!(s.settings.mode.as_deref(), Some("ask"));
    // ask -> plan（记下进入 plan 之前的模式），批准计划后回到 ask，并发 settings_changed + 审计
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "mode": "plan" } }))
        .await
        .unwrap();
    send(&td, &mut a, &s.id, "make a plan please").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    assert_eq!(req.kind, PermissionKind::Plan);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "implement" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s.id).await.settings.mode.as_deref(), Some("ask"));
    let evs = history(&td, &mut a, &s.id).await;
    let resolved_at = evs.iter().position(|e| matches!(e.event, Event::PermissionResolved { .. })).unwrap();
    assert!(evs[resolved_at..]
        .iter()
        .any(|e| matches!(&e.event, Event::SettingsChanged { settings } if settings.mode.as_deref() == Some("ask"))));
    assert_eq!(
        td.fake.last_handle().settings.lock().unwrap().last().unwrap().mode.as_deref(),
        Some("ask"),
        "the agent is told to leave plan mode"
    );
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    assert!(audit.entries.iter().any(|e| e.action == "session.set_settings"
        && e.detail["reason"] == json!("permission_approved")
        && e.detail["mode"] == json!("ask")));

    // 拒绝计划（继续规划）：留在 plan
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "mode": "plan" } }))
        .await
        .unwrap();
    send(&td, &mut a, &s.id, "another plan").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(
            &td,
            methods::PERMISSION_RESPOND,
            json!({ "request_id": req.request_id, "option_id": "keep_planning", "message": "more detail" }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s.id).await.settings.mode.as_deref(), Some("plan"));

    // 进入 plan 之前是 dangerous：批准后不回到 dangerous，落到默认模式
    let s2: SessionSummary =
        a.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "settings": { "mode": "yolo" } })).await.unwrap();
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s2.id, "settings": { "mode": "plan" } }))
        .await
        .unwrap();
    send(&td, &mut a, &s2.id, "plan it").await;
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "implement" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s2.id).await.settings.mode.as_deref(), Some("ask"));
    td.shutdown().await;
}

#[tokio::test]
async fn adapter_reported_exit_error_does_not_produce_a_duplicate_error_event() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("crashevent now")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let evs = history(&td, &mut a, &s.id).await;
    let exit_errors =
        evs.iter().filter(|e| matches!(&e.event, Event::Error { code, .. } if code == "agent_exited")).count();
    assert_eq!(exit_errors, 1);
    assert!(
        evs.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Error, .. })),
        "the daemon closes the turn itself"
    );
    td.shutdown().await;
}

// ---------------------------------------------------------------------------
// 生命周期与边界：危险模式复核、模式切换重启、ended 回收、归档、idle 通知、幂等创建、事件大小

#[tokio::test]
async fn dangerous_mode_is_rechecked_before_every_spawn_and_turn() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    // 允许时以危险模式创建并跑一个 turn
    let s: SessionSummary =
        a.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "settings": { "mode": "yolo" } })).await.unwrap();
    send(&td, &mut a, &s.id, "first").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 1);
    // 之后管理员禁止危险模式（热加载）
    anycon_daemon::config::set_value(&td.paths, "permissions.allow_dangerous_modes", "false").unwrap();
    td.ctx.config.reload().unwrap();
    // 进程还活着（idle 保活）：下一个 turn 也不会再投递给它
    send(&td, &mut a, &s.id, "second").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(
        evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "forbidden")),
        "the turn fails with forbidden: {:?}",
        kinds(&evs)
    );
    assert_eq!(td.fake.last_handle().input_texts(), vec!["first"], "the live process never saw the second message");
    // 显式重启（resume 路径）同样不会带着危险模式拉起
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    send(&td, &mut a, &s.id, "third").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    assert_eq!(td.fake.spawn_count(), 1, "no new process was spawned in the forbidden mode");
    // 切回安全模式之后可以继续（recoverable）
    let _: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "mode": "ask" } }))
        .await
        .unwrap();
    send(&td, &mut a, &s.id, "fourth").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn set_agent_defaults_is_gated_and_audit_computes_dangerous_from_the_actual_mode() {
    let td = TestDaemon::start_with(&[("permissions.allow_dangerous_modes", "false")], false).await;
    let mut a = td.client("a");
    let e = a
        .call::<Empty>(&td, methods::NODE_SET_AGENT_DEFAULTS, json!({ "agent": "fake", "mode": "yolo" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    let e = a
        .call::<Empty>(
            &td,
            methods::NODE_SET_AGENT_DEFAULTS,
            json!({ "agent": "fake", "options": { "verbose": "maybe" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidSettings);
    // 禁用危险模式时，新会话的默认值没有被「毒化」
    let s = create(&td, &mut a, None).await;
    assert_eq!(s.settings.mode.as_deref(), Some("ask"));
    td.shutdown().await;

    // 允许危险模式：批准 mode_change 切到 yolo，审计里 dangerous 按实际模式算（以前写死 false）
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("modechange yolo")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(get(&td, &mut a, &s.id).await.settings.mode.as_deref(), Some("yolo"));
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let sw = audit.entries.iter().find(|e| e.action == "session.set_settings").expect("mode switch audited");
    assert_eq!(sw.detail["mode"], json!("yolo"));
    assert_eq!(sw.detail["dangerous"], json!(true));
    // 切到非危险模式：false
    let s2 = create(&td, &mut a, Some("modechange please")).await;
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    let audit: NodeAuditResult = a.call(&td, methods::NODE_AUDIT, json!({})).await.unwrap();
    let plain = audit
        .entries
        .iter()
        .find(|e| e.action == "session.set_settings" && e.target.as_deref() == Some(s2.id.as_str()))
        .unwrap();
    assert_eq!(plain.detail["dangerous"], json!(false));
    td.shutdown().await;
}

#[tokio::test]
async fn a_mode_switch_that_needs_a_restart_restarts_before_the_next_turn() {
    let td = TestDaemon::start().await;
    td.fake.restart_on_settings.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("modechange please")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::WaitingPermission).await);
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    let _: Empty = a
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    // adapter 说要重启才能生效 -> 标记 needs_restart（以前被丢掉了）
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(td.fake.spawn_count(), 1);
    send(&td, &mut a, &s.id, "after the switch").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    assert_eq!(td.fake.spawn_count(), 2, "restarted lazily before the next turn");
    assert_eq!(td.fake.spawns.lock().unwrap()[1].settings.mode.as_deref(), Some("plan"));
    assert!(td.fake.spawns.lock().unwrap()[1].resume.is_some(), "and resumed");
    td.shutdown().await;
}

#[tokio::test]
async fn exited_is_processed_once_even_when_the_adapter_closes_its_channel_right_after() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    // 崩溃 + 立刻关闭事件通道（真实 claude adapter 就是这样）：只有一条 agent_exited 错误，状态是 error
    let s = create(&td, &mut a, Some("crash closechannel")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let evs = history(&td, &mut a, &s.id).await;
    assert_eq!(
        evs.iter().filter(|e| matches!(&e.event, Event::Error { code, .. } if code == "agent_exited")).count(),
        1,
        "{:?}",
        kinds(&evs)
    );
    assert_eq!(count(&evs, "error"), 1, "no second error event");
    assert_eq!(count(&evs, "turn_done"), 1);
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Error);
    // 正常退出（Ended）+ 关闭通道：不会被后来的「通道关闭」翻成 error
    let s2 = create(&td, &mut a, Some("exit closechannel")).await;
    assert!(wait_status(&td, &mut a, &s2.id, SessionStatus::Ended).await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(get(&td, &mut a, &s2.id).await.status, SessionStatus::Ended, "stays ended");
    let evs = history(&td, &mut a, &s2.id).await;
    assert_eq!(count(&evs, "error"), 0, "{:?}", kinds(&evs));
    td.shutdown().await;
}

#[tokio::test]
async fn max_turns_ends_the_session_and_a_later_send_resumes_it() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("maxturns reached")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Ended).await);
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::TurnDone { reason: TurnDoneReason::MaxTurns, .. })));
    // ended 的会话可以再发一条（resume）
    send(&td, &mut a, &s.id, "continue").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    td.shutdown().await;
}

#[tokio::test]
async fn an_ended_session_whose_process_is_still_alive_is_reclaimed_after_idle_timeout() {
    // 回合数 / 预算用完后会话是 ended，但进程还在；以前 sweep 只处理 idle，这个进程会一直占着
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("maxturns reached")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Ended).await);
    let h0 = td.fake.handle(0);
    assert!(
        eventually(Duration::from_secs(6), || async { h0.shutdowns.load(std::sync::atomic::Ordering::SeqCst) == 1 })
            .await,
        "the idle timeout also closes the process of an ended session"
    );
    assert_eq!(get(&td, &mut a, &s.id).await.status, SessionStatus::Ended, "it stays ended");
    assert_eq!(td.ctx.sessions.counts().await.1, 0, "no live process");
    // 之后照样可以接着发一条（resume 同一个 agent 会话）
    send(&td, &mut a, &s.id, "continue").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawns = td.fake.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[1].resume.as_ref().unwrap().agent_session_id, "fake-agent-1");
    td.shutdown().await;
}

#[tokio::test]
async fn archive_is_refused_while_a_turn_runs_or_waits_for_an_answer_and_delete_resolves_pending_permissions() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let s = create(&td, &mut a, Some("hold please")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Running).await);
    let e = a.call::<Empty>(&td, methods::SESSION_ARCHIVE, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    td.fake.last_handle().release();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);

    // 等待审批时也不能归档（spec § 7.1：先停止或答复；归档状态下不跑 turn）——所有会话，不只 worktree 会话
    let p = create(&td, &mut a, Some("permission please")).await;
    assert!(wait_status(&td, &mut a, &p.id, SessionStatus::WaitingPermission).await);
    let e = a.call::<Empty>(&td, methods::SESSION_ARCHIVE, json!({ "session_id": p.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let st: SessionSummary = a.call(&td, methods::SESSION_GET, json!({ "session_id": p.id })).await.unwrap();
    assert!(!st.archived && st.status == SessionStatus::WaitingPermission, "nothing changed");
    // 删除时：所有客户端都收到未决请求的 permission.resolved
    let req =
        a.call::<PermissionListResult>(&td, methods::PERMISSION_LIST, json!({})).await.unwrap().requests.remove(0);
    b.drain_now();
    let _: Empty = a.call(&td, methods::SESSION_DELETE, json!({ "session_id": p.id })).await.unwrap();
    let n = b
        .wait_notification(T, |n| n.method == methods::PERMISSION_RESOLVED)
        .await
        .expect("other clients are told the request is gone");
    let r: PermissionResolved = serde_json::from_value(n.params).unwrap();
    assert_eq!(
        (r.request_id.as_str(), r.session_id.as_str(), r.by_device.as_str()),
        (req.request_id.as_str(), p.id.as_str(), "daemon")
    );
    td.shutdown().await;
}

#[tokio::test]
async fn the_start_timeout_also_covers_a_spawn_that_hangs() {
    let td = TestDaemon::start().await;
    td.ctx.sessions.set_start_timeout(Duration::from_millis(400));
    *td.fake.spawn_delay.lock().unwrap() = Duration::from_secs(3);
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("hello")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Error).await, "times out while spawning");
    let evs = history(&td, &mut a, &s.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "agent_start_timeout")));
    // 迟到的进程拉起来之后没人要了：被关掉
    let fake = td.fake.clone();
    assert!(
        eventually(Duration::from_secs(8), || async {
            fake.spawn_count() == 1 && fake.last_handle().shutdowns.load(std::sync::atomic::Ordering::SeqCst) >= 1
        })
        .await,
        "the late process is shut down"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn session_idle_notifies_once_when_unwatched_and_never_for_archived_or_watched_sessions() {
    use anycon_daemon::notify::testing::Recorder;
    let td = TestDaemon::start_with(&[("notify.session_idle_after", "\"1s\"")], false).await;
    let rec = std::sync::Arc::new(Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    let mut a = td.client("a");
    let plain = create(&td, &mut a, Some("hi")).await;
    let archived = create(&td, &mut a, Some("hi")).await;
    let watched = create(&td, &mut a, Some("hi")).await;
    for s in [&plain, &archived, &watched] {
        assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    }
    let _: Empty = a.call(&td, methods::SESSION_ARCHIVE, json!({ "session_id": archived.id })).await.unwrap();
    // idle 开始之后才 watch（之后不再有新的 watch 调用）
    let _: Empty = a.call(&td, methods::SESSION_WATCH, json!({ "session_id": watched.id })).await.unwrap();
    // 在 idle **开始之前**就一直 watch 着——turn 进行中订阅，turn 结束进入 idle 时订阅还在，也算「看过」
    let early = create(&td, &mut a, None).await;
    let _: Empty = a.call(&td, methods::SESSION_WATCH, json!({ "session_id": early.id })).await.unwrap();
    send(&td, &mut a, &early.id, "hi").await;
    assert!(eventually(T, || async { td.fake.spawn_count() >= 4 }).await);
    assert!(wait_status(&td, &mut a, &early.id, SessionStatus::Idle).await);
    tokio::time::sleep(Duration::from_millis(2800)).await;
    let got: Vec<String> = rec
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.category == NotifyCategory::SessionIdle)
        .filter_map(|b| b.session_id.clone())
        .collect();
    assert_eq!(got, vec![plain.id.clone()], "only the unwatched, unarchived idle session, once");
    assert!(
        !got.contains(&early.id),
        "a client that was watching before the idle period began counts as having seen it"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn a_restart_does_not_fire_a_burst_of_session_idle_notifications() {
    use anycon_daemon::notify::testing::Recorder;
    let mut td = TestDaemon::start_with(&[("notify.session_idle_after", "\"1s\"")], false).await;
    let mut a = td.client("a");
    for _ in 0..3 {
        let s = create(&td, &mut a, Some("hi")).await;
        assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    }
    td.restart().await;
    let rec = std::sync::Arc::new(Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(rec.0.lock().unwrap().iter().all(|b| b.category != NotifyCategory::SessionIdle));
    td.shutdown().await;
}

#[tokio::test]
async fn create_with_a_prompt_is_idempotent_on_message_id() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let params = json!({ "agent": "fake", "prompt": "do it once", "message_id": "client-retry-1" });
    let first: SessionSummary = a.call(&td, methods::SESSION_CREATE, params.clone()).await.unwrap();
    // 客户端没收到响应，原样重试
    let second: SessionSummary = a.call(&td, methods::SESSION_CREATE, params.clone()).await.unwrap();
    assert_eq!(first.id, second.id, "the retry returns the session created by the first call");
    assert!(wait_status(&td, &mut a, &first.id, SessionStatus::Idle).await);
    let all: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 1);
    assert_eq!(td.fake.spawn_count(), 1, "the agent ran once");
    // 并发的重试也只创建一个
    let params = json!({ "agent": "fake", "prompt": "race", "message_id": "client-retry-2" });
    let (x, y) = {
        let mut c2 = td.client("b");
        tokio::join!(
            a.call::<SessionSummary>(&td, methods::SESSION_CREATE, params.clone()),
            c2.call::<SessionSummary>(&td, methods::SESSION_CREATE, params.clone())
        )
    };
    assert_eq!(x.unwrap().id, y.unwrap().id);
    let all: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 2);
    // 同一个 message_id 配不同的 prompt / agent / project 不是重试，返回 invalid，而不是悄悄返回第一个会话
    let _: Project = a.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "alt", "git_init": false })).await.unwrap();
    for (what, p) in [
        ("another prompt", json!({ "agent": "fake", "prompt": "something else", "message_id": "client-retry-1" })),
        (
            "another project",
            json!({ "agent": "fake", "prompt": "do it once", "message_id": "client-retry-1", "project": "alt" }),
        ),
    ] {
        let e = a.call::<SessionSummary>(&td, methods::SESSION_CREATE, p).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{what}: {e:?}");
        assert!(e.message.contains("already used"), "{what}: {}", e.message);
    }
    let all: SessionListResult = a.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 2, "no extra session was created");
    // session.send 同理：同一个 message_id 不同内容 -> invalid；原样重发仍是 duplicate
    let s = create(&td, &mut a, None).await;
    let ok = json!({ "session_id": s.id, "message_id": "send-1", "content": "ping" });
    let _: SessionSendResult = a.call(&td, methods::SESSION_SEND, ok.clone()).await.unwrap();
    assert!(a.call::<SessionSendResult>(&td, methods::SESSION_SEND, ok).await.unwrap().duplicate);
    let e = a
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "send-1", "content": "pong" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid, "{e:?}");
    td.shutdown().await;
}

#[tokio::test]
async fn big_tool_results_are_truncated_and_streamed_output_is_capped() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    let _: Empty = a.call(&td, methods::SESSION_WATCH, json!({ "session_id": s.id })).await.unwrap();
    a.drain_now();

    // 300 KB 的 tool_result：存储和发送都只保留前 48 KiB + 后 16 KiB
    send(&td, &mut a, &s.id, "bigresult").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let live: Vec<SessionEvent> = a
        .drain_for(Duration::from_millis(300))
        .await
        .into_iter()
        .filter(|n| n.method == methods::SESSION_EVENT)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect();
    let stored = history(&td, &mut a, &s.id).await;
    for (what, evs) in [("live", &live), ("stored", &stored)] {
        let out = evs
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolResult { output, .. } => Some(output.as_str().unwrap().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{what}: no tool_result"));
        assert!(out.starts_with("BIGSTART") && out.ends_with("BIGEND"), "{what}");
        assert!(out.len() < 64 * 1024 + 64, "{what}: {} bytes", out.len());
        let removed = 8 + "line of output\n".len() * 20_000 + 6 - 48 * 1024 - 16 * 1024;
        assert!(out.contains(&format!("…[truncated {removed} bytes]")), "{what}: marker");
        // 结构化标记：丢掉的字节数（实时事件和落库的历史都带）；这个调用没有流式输出，所以不标 live_output_truncated
        let detail = evs
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolResult { detail, .. } => detail.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{what}: tool_result.detail is set"));
        assert_eq!(detail.output_truncated_bytes, Some(removed as u64), "{what}");
        assert_eq!(detail.live_output_truncated, None, "{what}");
    }

    // 流式输出：每个工具调用累计转发不超过 64 KiB，超出的丢弃，补一条截断提示
    a.drain_now();
    send(&td, &mut a, &s.id, "bigdelta").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let live: Vec<SessionEvent> = a
        .drain_for(Duration::from_millis(300))
        .await
        .into_iter()
        .filter(|n| n.method == methods::SESSION_EVENT)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect();
    for evs in [&live, &history(&td, &mut a, &s.id).await] {
        let deltas: Vec<&String> = evs
            .iter()
            .filter_map(|e| match &e.event {
                Event::ToolOutputDelta { id, text } if id == "t-delta" => Some(text),
                _ => None,
            })
            .collect();
        let total: usize = deltas.iter().map(|t| t.len()).sum();
        assert!(total < 64 * 1024 + 256, "forwarded {total} bytes of 200 KB");
        assert!(deltas.last().unwrap().contains("truncated"), "a notice marks the cut");
        // 最终结果很小，没有头尾截断；但流式输出被截断过，detail 里结构化地标出来
        let result = evs
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolResult { output, detail, .. } if output == &json!("done") => Some(detail.clone()),
                _ => None,
            })
            .expect("the final tool_result");
        let d = result.expect("detail carries the live-output marker");
        assert_eq!((d.live_output_truncated, d.output_truncated_bytes), (Some(true), None));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn history_pages_are_capped_by_bytes_and_have_more_until_everything_is_read() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    // 4 条 150 KB 的文本事件
    for _ in 0..4 {
        send(&td, &mut a, &s.id, "bigtext").await;
        assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    }
    let mut after = 0;
    let mut pages = 0;
    let mut seen = Vec::new();
    loop {
        let r: SessionHistoryResult = a
            .call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "after_seq": after, "limit": 1000 }))
            .await
            .unwrap();
        let bytes: usize = r.events.iter().map(|e| serde_json::to_vec(e).unwrap().len()).sum();
        assert!(bytes <= 192 * 1024 || r.events.len() == 1, "page of {} events is {bytes} bytes", r.events.len());
        assert!(!r.events.is_empty());
        after = r.events.last().unwrap().seq;
        seen.extend(r.events.iter().map(|e| e.seq));
        pages += 1;
        if !r.has_more {
            break;
        }
        assert!(pages < 50, "no progress");
    }
    assert!(pages >= 3, "{pages} pages: 600 KB of text cannot fit in one 192 KiB page");
    let all = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), all, "no event appears twice");
    assert_eq!(seen, (1..=seen.len() as u64).collect::<Vec<_>>(), "no gaps");
    // 向前翻页（before_seq）保留最新的，同样封顶
    let last = *seen.last().unwrap();
    let r: SessionHistoryResult = a
        .call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "before_seq": last + 1, "limit": 1000 }))
        .await
        .unwrap();
    assert!(r.has_more);
    assert_eq!(r.events.last().unwrap().seq, last);
    let bytes: usize = r.events.iter().map(|e| serde_json::to_vec(e).unwrap().len()).sum();
    assert!(bytes <= 192 * 1024 || r.events.len() == 1);
    td.shutdown().await;
}

#[tokio::test]
async fn resume_waits_for_the_old_process_to_finish_shutting_down() {
    let td = TestDaemon::start_with(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    *td.fake.shutdown_delay.lock().unwrap() = Duration::from_millis(1800);
    let mut a = td.client("a");
    let s = create(&td, &mut a, Some("first")).await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    // idle 超时 -> 旧进程开始（缓慢地）关闭；就在这时来了一条新消息
    let fake = td.fake.clone();
    assert!(
        eventually(Duration::from_secs(5), || async {
            fake.handle(0).shutdowns.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await
    );
    send(&td, &mut a, &s.id, "second").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let order = td.fake.order.lock().unwrap().clone();
    assert_eq!(
        order,
        vec!["spawn:1", "shutdown_done:1", "spawn:2"],
        "never two processes for one agent session: {order:?}"
    );
    assert!(td.fake.spawns.lock().unwrap()[1].resume.is_some());
    td.shutdown().await;
}

#[tokio::test]
async fn set_settings_clear_resets_fields_and_survives_restart_and_resume() {
    let mut td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s = create(&td, &mut a, None).await;
    let r: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "system_prompt_append": "be brief", "max_turns": 7, "max_budget_usd": 1.5 } }),
        )
        .await
        .unwrap();
    assert_eq!((r.settings.max_turns, r.settings.max_budget_usd), (Some(7), Some(1.5)));
    // 空串 / 0 不是「清除」，只有 `clear` 才是
    let r: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": {}, "clear": ["system_prompt_append", "max_turns"] }),
        )
        .await
        .unwrap();
    assert_eq!(r.settings.system_prompt_append, None);
    assert_eq!(r.settings.max_turns, None);
    assert_eq!(r.settings.max_budget_usd, Some(1.5), "not named -> kept");
    // session.get 看不到它；settings_changed 事件里也是清除后的完整设置
    assert_eq!(get(&td, &mut a, &s.id).await.settings, r.settings);
    let evs = history(&td, &mut a, &s.id).await;
    let last = evs
        .iter()
        .rev()
        .find_map(|e| if let Event::SettingsChanged { settings } = &e.event { Some(settings.clone()) } else { None })
        .unwrap();
    assert_eq!(last, r.settings);
    // daemon 重启 + resume 之后也不复活
    td.restart().await;
    let mut a = td.client("a");
    assert_eq!(get(&td, &mut a, &s.id).await.settings.system_prompt_append, None);
    send(&td, &mut a, &s.id, "after the restart").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let spawn = td.fake.spawns.lock().unwrap().last().unwrap().clone();
    assert_eq!(spawn.settings.system_prompt_append, None);
    assert_eq!(spawn.settings.max_turns, None);
    assert_eq!(spawn.settings.max_budget_usd, Some(1.5));
    // clear model：回到默认模型；thinking 按新模型回落
    let r: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "fake-small" } }))
        .await
        .unwrap();
    assert_eq!(r.settings.model.as_deref(), Some("fake-small"));
    let r: SessionSetSettingsResult = a
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": {}, "clear": ["model"] }))
        .await
        .unwrap();
    assert_eq!(r.settings.model.as_deref(), Some("fake-large"), "back to the agent default");
    // 清除后的完整设置重新校验：危险模式被禁用时，不能靠 settings 之外的途径溜进去
    anycon_daemon::config::set_value(&td.paths, "permissions.allow_dangerous_modes", "false").unwrap();
    td.ctx.config.reload().unwrap();
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "yolo" }, "clear": ["env"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    // 非法数值
    let e = a
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "max_budget_usd": -3.0 } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().field.as_deref(), Some("max_budget_usd"));
    // 过大的消息
    let big = "x".repeat(1024 * 1024 + 1);
    let e = a
        .call::<SessionSendResult>(
            &td,
            methods::SESSION_SEND,
            json!({ "session_id": s.id, "message_id": "big-1", "content": big }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    td.shutdown().await;
}

#[tokio::test]
async fn a_cleared_setting_does_not_come_back_through_launch_args_when_the_process_is_gone() {
    // 没有存活进程时 `clear` 只改库里的 settings，launch_args（上次进程启动时的快照）里还有旧值；
    // adapter 回落到它就会让旧值复活。daemon 在没有 driver 时同步改写 launch_args。
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let s: SessionSummary = a
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({
                "agent": "fake", "prompt": "first", "message_id": "m-1",
                "settings": { "system_prompt_append": "OLD-SECRET-PROMPT", "max_turns": 9, "max_budget_usd": 3.0 }
            }),
        )
        .await
        .unwrap();
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    {
        let first = td.fake.spawns.lock().unwrap()[0].clone();
        assert_eq!(first.settings.system_prompt_append.as_deref(), Some("OLD-SECRET-PROMPT"));
    }
    // 进程退出（懒重启：session.restart 关掉进程，下次 send 才拉起）
    let _: Empty = a.call(&td, methods::SESSION_RESTART, json!({ "session_id": s.id })).await.unwrap();
    assert!(
        eventually(T, || async { td.fake.last_handle().shutdowns.load(std::sync::atomic::Ordering::SeqCst) >= 1 })
            .await
    );
    // 库里的快照确实带着旧值（这是 bug 的前提）
    let row = td.ctx.store.get_session(&s.id).await.unwrap().unwrap();
    assert!(row.launch_args.to_string().contains("OLD-SECRET-PROMPT"), "{}", row.launch_args);
    // 没有 driver 时 clear
    let r: SessionSetSettingsResult = a
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": {}, "clear": ["system_prompt_append", "max_turns"] }),
        )
        .await
        .unwrap();
    assert_eq!((r.settings.system_prompt_append.clone(), r.settings.max_turns), (None, None));
    let row = td.ctx.store.get_session(&s.id).await.unwrap().unwrap();
    assert!(
        !row.launch_args.to_string().contains("OLD-SECRET-PROMPT"),
        "launch_args was rewritten: {}",
        row.launch_args
    );
    assert!(row.launch_args.get("system_prompt_append").is_none() && row.launch_args.get("max_turns").is_none());
    assert_eq!(row.launch_args["max_budget_usd"], json!(3.0), "not cleared -> kept");
    // resume：交给 adapter 的设置和 resume 句柄里都没有旧值
    send(&td, &mut a, &s.id, "second").await;
    assert!(wait_status(&td, &mut a, &s.id, SessionStatus::Idle).await);
    let second = td.fake.spawns.lock().unwrap().last().unwrap().clone();
    assert_eq!(second.settings.system_prompt_append, None);
    assert_eq!(second.settings.max_turns, None);
    let resume = second.resume.expect("a resume, not a fresh spawn");
    assert!(!resume.launch_args.to_string().contains("OLD-SECRET-PROMPT"), "{}", resume.launch_args);
    assert_eq!(resume.agent_session_id, "fake-agent-1", "the agent session id is still taken from the prior handle");
    td.shutdown().await;
}
