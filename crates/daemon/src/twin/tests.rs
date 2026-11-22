//! 分身行为测试：用可替换时钟、假判断后端和假 adapter 验证判断与执行之间的边界。

use super::*;
use crate::judge::JudgeReply;
use crate::testutil::{FakeJudgeBackend, TestDaemon};
use std::sync::atomic::Ordering;

async fn fixture(git: bool) -> (TestDaemon, Arc<FakeJudgeBackend>) {
    let overrides = [
        ("judge.triage", "'local'"),
        (
            "judge.backends.local",
            "{kind='api',format='openai',model='fake',base_url='http://localhost:1',timeout='10s'}",
        ),
    ];
    let td =
        if git { TestDaemon::start_git(&overrides).await } else { TestDaemon::start_with(&overrides, false).await };
    td.fake.declare_no_push.store(true, Ordering::SeqCst);
    td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    let fake = FakeJudgeBackend::new([]);
    let backend = fake.clone();
    td.ctx.judge.set_backend_factory(Arc::new(move |_| backend.clone()));
    td.ctx.judge.clock().set(crate::util::now_ms());
    set(
        &td.ctx,
        &Caller::local_cli(),
        TwinSetParams { projects: Some(vec!["p".into()]), agent: Some("fake".into()), ..Default::default() },
    )
    .await
    .unwrap();
    (td, fake)
}
async fn todo(td: &TestDaemon, title: &str) -> Todo {
    let mut c = td.local_cli_client();
    c.call(
        td,
        methods::TODO_ADD,
        json!({"id":uuid::Uuid::new_v4().to_string(),"title":title,"project":"p","handler":"twin"}),
    )
    .await
    .unwrap()
}
fn reply(fake: &FakeJudgeBackend, plans: Value) {
    fake.push(Ok(JudgeReply { text: json!({"items":plans}).to_string(), ..Default::default() }));
}
async fn run_now(td: &TestDaemon, dry: bool) -> TwinRun {
    start(&td.ctx, dry, vec![TwinTrigger::Manual]).await.unwrap().await.unwrap().unwrap()
}
async fn plan(td: &TestDaemon, fake: &FakeJudgeBackend, t: &Todo, action: &str) -> TwinRun {
    reply(fake, json!([{"id":t.id,"action":action,"reason":"A reason"}]));
    run_now(td, false).await
}

#[test]
fn merge_deadline_has_quiet_window_and_hard_limit() {
    let mut p = Pending::default();
    p.add(TwinTrigger::Todo, 1000);
    assert_eq!(p.deadline(), Some(61_000));
    for n in 1..=8 {
        p.add(TwinTrigger::Session, 1000 + n * 40_000);
        assert!(p.deadline().unwrap() <= 301_000);
    }
    assert_eq!(p.triggers, vec![TwinTrigger::Todo, TwinTrigger::Session]);
    assert_eq!(p.deadline(), Some(301_000));
}
#[test]
fn tolerant_parser_and_action_field_validation() {
    assert!(parse("{\"wrong\":[]} ```json\n{\"items\":[]}\n``` trailing").unwrap().is_empty());
    assert!(parse("{\"items\":{}}").is_err());
    for action in ["dispatch", "ask_user", "ask_advisor", "wait", "hand_back", "done"] {
        let mut v = json!({"id":"t","action":action,"reason":"because"});
        if action.starts_with("ask_") {
            v["question"] = json!("Which approach?");
        }
        assert!(legal(&serde_json::from_value(v.clone()).unwrap()));
        v["reason"] = json!("a".repeat(501));
        assert!(!legal(&serde_json::from_value(v).unwrap()));
    }
    for v in [
        json!({"id":"t","action":"wait","reason":"r","note":"no"}),
        json!({"id":"t","action":"ask_user","reason":"r","question":"q","options":["a","a","a","a","a","a"]}),
        json!({"id":"t","action":"dispatch","reason":"r","note":"x".repeat(2049)}),
        json!({"id":"t","action":"ask_advisor","reason":"r","question":"x".repeat(2049)}),
        json!({"id":"t","action":"ask_user","reason":"r","question":"x".repeat(1025)}),
        json!({"id":"t","action":"future","reason":"r"}),
    ] {
        assert!(!legal(&serde_json::from_value(v).unwrap()));
    }
}

#[tokio::test]
async fn scope_validation_defaults_and_states() {
    let td = TestDaemon::start().await;
    assert_eq!(get(&td.ctx).await.unwrap().state, TwinState::NotConfigured);
    td.ctx.twin.trigger(&td.ctx, TwinTrigger::Manual).await.unwrap();
    td.ctx.twin.poll(&td.ctx).await.unwrap();
    assert!(td.ctx.twin.pending.lock().unwrap().deadline().is_none());
    assert!(td.ctx.twin.pending.lock().unwrap().next_tick.is_none());
    assert_eq!(td.ctx.judge.used_today().await.unwrap(), 0);
    let human = Caller::local_cli();
    let err = set(
        &td.ctx,
        &human,
        TwinSetParams { projects: Some(vec!["p".into()]), agent: Some("fake".into()), ..Default::default() },
    )
    .await
    .unwrap_err();
    assert_eq!(err.data.unwrap().reason.as_deref(), Some("no_push_unsupported"));
    td.shutdown().await;
    let (td, _) = fixture(false).await;
    for (params, reason) in [
        (TwinSetParams { max_sessions: Some(6), ..Default::default() }, "no_capacity"),
        (TwinSetParams { mode: Some("yolo".into()), ..Default::default() }, "dangerous"),
    ] {
        assert_eq!(set(&td.ctx, &human, params).await.unwrap_err().data.unwrap().reason.as_deref(), Some(reason));
    }
    assert!(set(&td.ctx, &human, TwinSetParams { projects: Some(vec!["bad\nname".into()]), ..Default::default() })
        .await
        .is_err());
    assert!(set(&td.ctx, &human, TwinSetParams { model: Some("missing-model".into()), ..Default::default() })
        .await
        .is_err());
    assert!(set(&td.ctx, &human, TwinSetParams { mode: Some("missing-mode".into()), ..Default::default() })
        .await
        .is_err());
    assert_eq!(settings(&td.ctx, &get(&td.ctx).await.unwrap().scope).unwrap().1.mode.as_deref(), Some("ask"));
    assert_eq!(get(&td.ctx).await.unwrap().state, TwinState::Idle);
    assert_eq!(
        set(&td.ctx, &human, TwinSetParams { paused: Some(true), ..Default::default() }).await.unwrap().state,
        TwinState::Paused
    );
    crate::config::set_value(&td.paths, "twin.enabled", "false").unwrap();
    td.ctx.config.reload().unwrap();
    assert_eq!(get(&td.ctx).await.unwrap().state, TwinState::Disabled);
    td.shutdown().await;
}

#[tokio::test]
async fn mcp_writes_are_forbidden_and_reads_work() {
    let (td, _) = fixture(false).await;
    let mut c = td.local_mcp_client();
    for method in [methods::TWIN_GET, methods::TWIN_RUNS, methods::TWIN_QUESTIONS] {
        c.call::<Value>(&td, method, json!({})).await.unwrap();
    }
    for method in [methods::TWIN_SET, methods::TWIN_WAKE, methods::TWIN_DRY_RUN, methods::TWIN_ANSWER] {
        let params = if method == methods::TWIN_ANSWER { json!({"id":"q","answer":"a"}) } else { json!({}) };
        assert_eq!(c.call::<Value>(&td, method, params).await.unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn idle_and_budget_do_not_call_backend() {
    let (td, fake) = fixture(false).await;
    assert_eq!(run_now(&td, false).await.result, TwinRunResult::Idle);
    let _ = todo(&td, "work").await;
    crate::config::set_value(&td.paths, "judge.daily_calls", "0").unwrap();
    td.ctx.config.reload().unwrap();
    assert_eq!(run_now(&td, false).await.result, TwinRunResult::Budget);
    assert_eq!(get(&td.ctx).await.unwrap().state, TwinState::Budget);
    assert!(fake.requests.lock().unwrap().is_empty());
    assert_eq!(td.ctx.store.twin_runs(LOCAL, TwinRunsParams::default()).await.unwrap().len(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn another_judge_position_exhausting_budget_announces_state_without_a_tick() {
    let (td, fake) = fixture(false).await;
    let mut client = td.local_cli_client();
    crate::config::set_value(&td.paths, "judge.daily_calls", "1").unwrap();
    td.ctx.config.reload().unwrap();
    fake.push(Ok(JudgeReply { text: "advice".into(), ..Default::default() }));
    td.ctx
        .judge
        .call(Position::Extract, JudgeRequest { system: "Extract".into(), prompt: "Data".into() })
        .await
        .unwrap();
    assert!(client
        .wait_notification(Duration::from_secs(5), |n| n.method == methods::TWIN_CHANGED
            && n.params["state"] == "budget")
        .await
        .is_some());
    assert!(td.ctx.twin.pending.lock().unwrap().next_tick.is_none());
    assert!(td.ctx.store.twin_runs(LOCAL, TwinRunsParams::default()).await.unwrap().is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn agent_availability_announces_configuration_changes_without_a_timer() {
    let (td, fake) = fixture(false).await;
    let mut client = td.local_cli_client();
    crate::config::set_value(&td.paths, "judge.backends.local", "{kind='agent',agent='fake',timeout='10s'}").unwrap();
    td.ctx.config.reload().unwrap();
    assert!(client
        .wait_notification(Duration::from_secs(5), |n| n.method == methods::TWIN_CHANGED
            && n.params["state"] == "not_configured")
        .await
        .is_some());
    assert!(td.ctx.twin.pending.lock().unwrap().next_tick.is_none());
    td.fake.oneshot.store(true, Ordering::SeqCst);
    td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    assert!(client
        .wait_notification(Duration::from_secs(5), |n| n.method == methods::TWIN_CHANGED && n.params["state"] == "idle")
        .await
        .is_some());
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(td.ctx.twin.pending.lock().unwrap().next_tick.is_none());
    td.shutdown().await;
}

#[tokio::test]
async fn input_is_bounded_ordered_random_and_injects_rules() {
    let (td, _) = fixture(false).await;
    let mut c = td.local_cli_client();
    let mut todos = vec![];
    for i in 0..16 {
        let mut t = todo(&td, &format!("item {i}")).await;
        t.body = "界".repeat(5000);
        t.updated_ms = 100 + i;
        td.ctx.store.todo_save(&t).await.unwrap();
        todos.push(t);
    }
    c.call::<Value>(
        &td,
        methods::RULE_ADD,
        json!({"id":uuid::Uuid::new_v4().to_string(),"text":"Always explain the tradeoff"}),
    )
    .await
    .unwrap();
    c.call::<Value>(
        &td,
        methods::RULE_ADD,
        json!({"id":uuid::Uuid::new_v4().to_string(),"text":"Use the project's formatter","project":"p"}),
    )
    .await
    .unwrap();
    let scope = get(&td.ctx).await.unwrap().scope;
    let sorted = items(&td.ctx, &scope).await.unwrap();
    let (a, ids) = request(&td.ctx, &scope, &sorted, Some("ADVICE")).await.unwrap();
    let (b, _) = request(&td.ctx, &scope, &sorted, None).await.unwrap();
    assert!(a.system.contains("Always explain the tradeoff"));
    assert!(a.prompt.contains("Use the project's formatter"));
    assert!(a.prompt.contains("ADVICE"));
    assert_ne!(a.prompt, b.prompt);
    assert!(ids.len() < 16);
    assert_eq!(ids, todos[..ids.len()].iter().map(|t| t.id.clone()).collect::<Vec<_>>());
    let data = a.prompt.split("\nAvailable resources:").next().unwrap();
    assert!(data.len() <= INPUT_BYTES);
    for chunk in data.split("</DATA_").skip(1) {
        assert!(!chunk.is_empty());
    }
    td.shutdown().await;
}

#[tokio::test]
async fn invalid_entries_are_independent_and_unknown_ids_cannot_touch_other_todos() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    let plans = json!([
        {"id":t.id,"action":"wait","reason":"r","note":"illegal"},
        {"id":"other","action":"wait","reason":"r"},
        {"id":t.id,"action":"wait","reason":"r"},
    ]);
    reply(&fake, plans);
    let run = run_now(&td, false).await;
    assert_eq!(run.decisions[0].skip_reason, Some(TwinSkipReason::Invalid));
    assert_eq!(run.decisions[1].skip_reason, Some(TwinSkipReason::OutOfScope));
    assert_eq!(run.decisions[2].outcome, TwinDecisionOutcome::Executed);
    fake.push(Ok(JudgeReply { text: "not a plan".into(), ..Default::default() }));
    let bad = run_now(&td, false).await;
    assert_eq!(bad.error.unwrap().code, JudgeErrorCode::BadOutput);
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Twin);
    td.shutdown().await;
}

#[tokio::test]
async fn unknown_candidate_fields_are_ignored() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    reply(&fake, json!([{"id":t.id,"action":"done","reason":"Finished","confidence":0.9}]));
    let run = run_now(&td, false).await;
    assert_eq!(run.result, TwinRunResult::Done);
    assert_eq!(run.decisions.len(), 1);
    assert_eq!(run.decisions[0].action, TwinAction::Done);
    assert_eq!(run.decisions[0].outcome, TwinDecisionOutcome::Executed);
    assert_eq!(run.decisions[0].skip_reason, None);
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    td.shutdown().await;
}

#[tokio::test]
async fn questions_replace_void_answer_notify_and_push() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    let rec = Arc::new(crate::notify::testing::Recorder::default());
    td.ctx.notifier.set_out(rec.clone());
    let mut client = td.local_cli_client();
    for question in ["First?", "Second?"] {
        td.ctx.judge.clock().set(td.ctx.judge.clock().now() + 1);
        reply(
            &fake,
            json!([{"id":t.id,"action":"ask_user","reason":"Preference","question":question,"options":["a","b"]}]),
        );
        assert_eq!(run_now(&td, false).await.decisions[0].outcome, TwinDecisionOutcome::Executed);
    }
    let questions = td.ctx.store.twin_questions(LOCAL, false).await.unwrap();
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].status, TwinQuestionStatus::Void);
    assert_eq!(questions[1].status, TwinQuestionStatus::Open);
    let q = answer(&td.ctx, &Caller::local_cli(), TwinAnswerParams { id: questions[1].id.clone(), answer: "a".into() })
        .await
        .unwrap();
    assert_eq!(q.status, TwinQuestionStatus::Answered);
    assert!(td.ctx.twin.pending.lock().unwrap().triggers.contains(&TwinTrigger::Answer));
    assert!(client.drain_now().iter().any(|n| n.method == methods::TWIN_QUESTION));
    let pushes = rec.0.lock().unwrap().clone();
    assert_eq!(pushes[0].category, NotifyCategory::TwinQuestion);
    assert_eq!(pushes[0].session_id.as_deref(), Some(format!("todo_{}", t.id).as_str()));
    assert!(pushes[0].detail.is_none());
    let (req, _) = request(&td.ctx, &get(&td.ctx).await.unwrap().scope, std::slice::from_ref(&t), None).await.unwrap();
    assert!(req.prompt.contains("answer: a"));
    td.shutdown().await;
}

#[tokio::test]
async fn hand_back_and_done_leave_state_and_audit_every_decision() {
    let (td, fake) = fixture(false).await;
    for action in ["hand_back", "done"] {
        let t = todo(&td, "work").await;
        let run = plan(&td, &fake, &t, action).await;
        let saved = td.ctx.store.todo_get(&t.id).await.unwrap().unwrap();
        assert_eq!(saved.handler, TodoHandler::Me);
        assert_eq!(saved.state, TodoState::Open);
        let events = td.ctx.store.todo_history_page(&t.id, None, 200).await.unwrap().events;
        assert!(events.iter().any(|e| matches!(&e.data,TodoEventData::TwinDecision {run_id,..} if run_id==&run.id)));
        assert!(events.iter().any(|e| matches!(
            &e.data,
            TodoEventData::HandedBack { reason, .. }
                if action != "done" || reason.contains("believes this is done")
        )));
    }
    let audit = td.ctx.audit.query(None, None, 100).await;
    assert!(audit
        .iter()
        .any(|e| e.action == "twin.decision" && e.detail["actor"] == json!({"kind":"twin","id":"local"})));
    td.shutdown().await;
}

#[tokio::test]
async fn dry_run_has_plan_parameters_and_no_action_side_effects() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    reply(&fake, json!([{"id":t.id,"action":"ask_user","reason":"r","question":"Which?","options":["a"]}]));
    let result = dry_run(&td.ctx, &Caller::local_cli()).await.unwrap().run;
    assert!(result.dry_run);
    assert_eq!(result.judge_calls.len(), 1);
    assert_eq!(result.decisions[0].question.as_deref(), Some("Which?"));
    assert!(result.decisions[0].question_id.is_none());
    assert!(td.ctx.store.twin_questions(LOCAL, false).await.unwrap().is_empty());
    assert_eq!(td.ctx.store.todo_history_page(&t.id, None, 100).await.unwrap().events.len(), 1);
    reply(&fake, json!([{"id":t.id,"action":"ask_advisor","reason":"r","question":"How?"}]));
    let result = run_now(&td, true).await;
    assert_eq!(result.judge_calls.len(), 1);
    assert_eq!(fake.requests.lock().unwrap().len(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn advisor_retriages_only_that_item_and_only_once() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    let other = todo(&td, "other").await;
    reply(&fake, json!([{"id":t.id,"action":"ask_advisor","reason":"r","question":"How?"}]));
    fake.push(Ok(JudgeReply { text: "Use approach A".into(), ..Default::default() }));
    reply(&fake, json!([{"id":t.id,"action":"ask_advisor","reason":"r","question":"Again?"}]));
    let result = run_now(&td, false).await;
    assert_eq!(result.judge_calls.len(), 3);
    assert_eq!(result.decisions.len(), 2);
    assert_eq!(result.decisions[1].skip_reason, Some(TwinSkipReason::Invalid));
    let req = fake.requests.lock().unwrap()[2].clone();
    assert!(req.prompt.contains("Use approach A"));
    assert!(!req.prompt.contains(&other.id));
    td.shutdown().await;
}

#[tokio::test]
async fn advisor_retriage_failure_skips_only_that_item() {
    for next in [
        Err(CallError::Network),
        Ok(JudgeReply { text: "not a plan".into(), ..Default::default() }),
        Ok(JudgeReply { text: "{\"items\":{}}".into(), ..Default::default() }),
    ] {
        let (td, fake) = fixture(false).await;
        let t = todo(&td, "needs advice").await;
        let other = todo(&td, "finished").await;
        let plans = json!([
            {"id":t.id,"action":"ask_advisor","reason":"r","question":"How?"},
            {"id":other.id,"action":"done","reason":"Finished"},
        ]);
        reply(&fake, plans);
        fake.push(Ok(JudgeReply { text: "Use approach A".into(), ..Default::default() }));
        fake.push(next);
        let run = run_now(&td, false).await;
        assert_eq!(run.result, TwinRunResult::Done);
        assert!(run.error.is_none());
        assert_eq!(run.judge_calls.len(), 3);
        assert_eq!(run.decisions.len(), 3);
        assert_eq!(run.decisions[0].action, TwinAction::AskAdvisor);
        assert_eq!(run.decisions[0].outcome, TwinDecisionOutcome::Executed);
        assert_eq!(run.decisions[1].todo_id, t.id);
        assert_eq!(run.decisions[1].outcome, TwinDecisionOutcome::Skipped);
        assert_eq!(run.decisions[1].skip_reason, Some(TwinSkipReason::Invalid));
        assert_eq!(run.decisions[2].todo_id, other.id);
        assert_eq!(run.decisions[2].action, TwinAction::Done);
        assert_eq!(run.decisions[2].outcome, TwinDecisionOutcome::Executed);
        assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Twin);
        assert_eq!(td.ctx.store.todo_get(&other.id).await.unwrap().unwrap().handler, TodoHandler::Me);
        td.shutdown().await;
    }
}

#[tokio::test]
async fn advisor_retriage_cancellation_ends_the_wake() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "needs advice").await;
    let other = todo(&td, "finished").await;
    let plans = json!([
        {"id":t.id,"action":"ask_advisor","reason":"r","question":"How?"},
        {"id":other.id,"action":"done","reason":"Finished"},
    ]);
    reply(&fake, plans);
    fake.push(Ok(JudgeReply { text: "Use approach A".into(), ..Default::default() }));
    fake.push(Err(CallError::Cancelled));
    let run = run_now(&td, false).await;
    assert_eq!(run.result, TwinRunResult::Cancelled);
    assert_eq!(run.error.unwrap().code, JudgeErrorCode::Cancelled);
    assert_eq!(run.judge_calls.len(), 3);
    assert_eq!(run.decisions.len(), 1);
    assert_eq!(run.decisions[0].action, TwinAction::AskAdvisor);
    assert_eq!(run.decisions[0].outcome, TwinDecisionOutcome::Executed);
    assert_eq!(td.ctx.store.todo_get(&other.id).await.unwrap().unwrap().handler, TodoHandler::Twin);
    td.shutdown().await;
}

#[tokio::test]
async fn dispatch_reuses_worktree_no_push_mode_note_and_takeover() {
    let (td, fake) = fixture(true).await;
    td.new_repo("p");
    let t = todo(&td, "hold").await;
    reply(&fake, json!([{"id":t.id,"action":"dispatch","reason":"Ready","note":"Keep the change small"}]));
    let run = run_now(&td, false).await;
    assert_eq!(run.decisions[0].outcome, TwinDecisionOutcome::Executed);
    let sid = run.decisions[0].session_id.as_ref().unwrap();
    let session = td.ctx.sessions.get_summary(sid).await.unwrap();
    assert!(session.worktree.is_some());
    assert_eq!(session.settings.options["no_push"], true);
    assert_eq!(session.settings.mode.as_deref(), Some("ask"));
    let events = td.ctx.store.events_after(sid, 0, 100).await.unwrap();
    let note = "Twin note (written by the twin):\nKeep the change small";
    assert!(events.iter().any(|e| matches!(&e.event, Event::UserMessage { content, .. } if content.contains(note))));
    assert_eq!(get(&td.ctx).await.unwrap().today.dispatches, 1);
    assert_eq!(get(&td.ctx).await.unwrap().working, vec![t.id.clone()]);
    let mut c = td.local_cli_client();
    c.call::<Value>(
        &td,
        methods::SESSION_SEND,
        json!({"session_id":sid,"message_id":uuid::Uuid::new_v4().to_string(),"content":"I will take over"}),
    )
    .await
    .unwrap();
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    assert!(td
        .ctx
        .store
        .todo_history_page(&t.id, None, 100)
        .await
        .unwrap()
        .events
        .iter()
        .any(|e| matches!(&e.data, TodoEventData::TakenOver { .. })));
    let audit = td.ctx.audit.query(None, None, 100).await;
    for action in ["todo.handler", "twin.taken_over"] {
        assert!(audit.iter().any(|e| e.action == action
            && e.target.as_deref() == Some(&t.id)
            && e.detail["actor"] == json!({"kind":"twin","id":"local"})));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn dispatch_caps_and_three_failures_hand_back() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    set(&td.ctx, &Caller::local_cli(), TwinSetParams { daily_dispatches: Some(0), ..Default::default() })
        .await
        .unwrap();
    let r = plan(&td, &fake, &t, "dispatch").await;
    assert_eq!(r.decisions[0].skip_reason, Some(TwinSkipReason::Budget));
    set(
        &td.ctx,
        &Caller::local_cli(),
        TwinSetParams { daily_dispatches: Some(10), max_sessions: Some(0), ..Default::default() },
    )
    .await
    .unwrap();
    let r = plan(&td, &fake, &t, "dispatch").await;
    assert_eq!(r.decisions[0].skip_reason, Some(TwinSkipReason::NoCapacity));
    set(&td.ctx, &Caller::local_cli(), TwinSetParams { max_sessions: Some(2), ..Default::default() }).await.unwrap();
    // 项目不存在，派发事务失败，不会留下会话；第三次由约束交回。
    for _ in 0..3 {
        let r = plan(&td, &fake, &t, "dispatch").await;
        assert_eq!(
            r.decisions.iter().find(|d| d.action == TwinAction::Dispatch).unwrap().skip_reason,
            Some(TwinSkipReason::Invalid)
        );
    }
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    td.shutdown().await;
}

#[tokio::test]
async fn stale_session_is_returned_without_cancelling_it() {
    let (td, fake) = fixture(true).await;
    td.new_repo("p");
    let t = todo(&td, "hold").await;
    let first = plan(&td, &fake, &t, "dispatch").await;
    let sid = first.decisions[0].session_id.as_ref().unwrap();
    td.ctx.judge.clock().set(crate::util::now_ms() + 121 * MINUTE);
    let r = run_now(&td, false).await;
    assert_eq!(r.result, TwinRunResult::Idle);
    assert_eq!(r.decisions[0].action, TwinAction::HandBack);
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    assert!(matches!(
        td.ctx.sessions.get_summary(sid).await.unwrap().status,
        SessionStatus::Running | SessionStatus::Starting | SessionStatus::Queued
    ));
    td.shutdown().await;
}

#[tokio::test]
async fn merged_wakes_are_serial_and_triggers_during_run_are_retained() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    *fake.delay.lock().unwrap() = Duration::from_millis(100);
    reply(&fake, json!([{"id":t.id,"action":"wait","reason":"r"}]));
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now + MINUTE - 1);
    td.ctx.twin.poll(&td.ctx).await.unwrap();
    assert!(fake.requests.lock().unwrap().is_empty());
    td.ctx.judge.clock().set(now + MINUTE);
    td.ctx.twin.poll(&td.ctx).await.unwrap();
    assert_eq!(get(&td.ctx).await.unwrap().state, TwinState::Running);
    td.ctx.twin.trigger(&td.ctx, TwinTrigger::Answer).await.unwrap();
    assert!(start(&td.ctx, true, vec![]).await.is_err());
    td.ctx.twin.wait_idle().await;
    assert!(td.ctx.twin.pending.lock().unwrap().triggers.contains(&TwinTrigger::Answer));
    td.shutdown().await;
}

#[tokio::test]
async fn a_contending_dry_run_cannot_consume_a_scheduled_wake() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    let first = td.ctx.twin.pending.lock().unwrap().first;
    let deadline = td.ctx.twin.pending.lock().unwrap().deadline();
    td.ctx.judge.clock().set(deadline.unwrap());
    let guard = td.ctx.twin.gate.lock().await;
    assert!(td.ctx.twin.poll(&td.ctx).await.is_err());
    assert_eq!(td.ctx.twin.pending.lock().unwrap().first, first);
    assert_eq!(td.ctx.twin.pending.lock().unwrap().deadline(), deadline);
    assert!(fake.requests.lock().unwrap().is_empty());
    drop(guard);
    reply(&fake, json!([{"id":t.id,"action":"wait","reason":"r"}]));
    td.ctx.twin.poll(&td.ctx).await.unwrap();
    td.ctx.twin.wait_idle().await;
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_call_and_finishes_record() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    *fake.delay.lock().unwrap() = Duration::from_secs(30);
    reply(&fake, json!([{"id":t.id,"action":"wait","reason":"r"}]));
    let rx = start(&td.ctx, false, vec![TwinTrigger::Manual]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !fake.requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    td.ctx.shutdown_tx.send(true).unwrap();
    let r = rx.await.unwrap().unwrap();
    assert_eq!(r.result, TwinRunResult::Cancelled);
    assert_eq!(r.error.unwrap().code, JudgeErrorCode::Cancelled);
    assert_eq!(r.judge_calls.len(), 1);
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM judge_calls WHERE id=?")
        .bind(&r.judge_calls[0])
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("cancelled"));
    td.ctx.twin.wait_idle().await;
    td.shutdown().await;
}

#[tokio::test]
async fn mode_falls_back_to_agent_default_without_unattended_and_clear_restores_defaults() {
    let (td, _) = fixture(false).await;
    td.fake.unattended.store(false, Ordering::SeqCst);
    crate::config::set_value(&td.paths, "agents.fake.default_mode", "plan").unwrap();
    td.ctx.config.reload().unwrap();
    td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    let scope = get(&td.ctx).await.unwrap().scope;
    assert_eq!(settings(&td.ctx, &scope).unwrap().1.mode.as_deref(), Some("plan"));
    let r = set(
        &td.ctx,
        &Caller::local_cli(),
        TwinSetParams { mode: Some("ask".into()), model: Some("fake-small".into()), ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(settings(&td.ctx, &r.scope).unwrap().1.mode.as_deref(), Some("ask"));
    let r = set(
        &td.ctx,
        &Caller::local_cli(),
        TwinSetParams {
            clear: vec![ClearableTwinField::Agent, ClearableTwinField::Model, ClearableTwinField::Mode],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(r.scope.agent.is_none() && r.scope.model.is_none() && r.scope.mode.is_none());
    assert_eq!(settings(&td.ctx, &r.scope).unwrap().1.mode.as_deref(), Some("plan"));
    td.shutdown().await;
}

#[tokio::test]
async fn closing_or_reassigning_voids_questions_and_invalidates_late_answers() {
    let (td, fake) = fixture(false).await;
    for field in ["state", "handler"] {
        let t = todo(&td, "work").await;
        reply(&fake, json!([{"id":t.id,"action":"ask_user","reason":"r","question":"Which?"}]));
        run_now(&td, false).await;
        let q = td.ctx.store.twin_questions(LOCAL, true).await.unwrap().pop().unwrap();
        let mut c = td.local_cli_client();
        let mut p = json!({"id":t.id});
        p[field] = json!(if field == "state" { "done" } else { "me" });
        c.call::<Value>(&td, methods::TODO_UPDATE, p).await.unwrap();
        assert!(td.ctx.store.twin_questions(LOCAL, true).await.unwrap().is_empty());
        assert!(answer(&td.ctx, &Caller::local_cli(), TwinAnswerParams { id: q.id, answer: "late".into() })
            .await
            .is_err());
    }
    td.shutdown().await;
}

#[tokio::test]
async fn retention_keeps_open_questions_and_scope_but_prunes_old_runs() {
    let (td, fake) = fixture(false).await;
    let t = todo(&td, "work").await;
    reply(&fake, json!([{"id":t.id,"action":"ask_user","reason":"r","question":"Which?"}]));
    run_now(&td, false).await;
    set(&td.ctx, &Caller::local_cli(), TwinSetParams { paused: Some(true), ..Default::default() }).await.unwrap();
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now + 91 * 24 * 60 * MINUTE);
    td.ctx.judge.cleanup().await.unwrap();
    assert!(td.ctx.store.twin_runs(LOCAL, TwinRunsParams::default()).await.unwrap().is_empty());
    assert_eq!(td.ctx.store.twin_questions(LOCAL, true).await.unwrap().len(), 1);
    assert!(!get(&td.ctx).await.unwrap().scope.projects.is_empty());
    assert_eq!(get(&td.ctx).await.unwrap().today.judge_calls, 0);
    td.shutdown().await;
}

#[tokio::test]
async fn dry_run_reserves_planned_slots_and_local_day_resets_dispatch_budget() {
    let (td, fake) = fixture(false).await;
    let a = todo(&td, "a").await;
    let b = todo(&td, "b").await;
    set(&td.ctx, &Caller::local_cli(), TwinSetParams { max_sessions: Some(1), ..Default::default() }).await.unwrap();
    reply(&fake, json!([{"id":a.id,"action":"dispatch","reason":"r"},{"id":b.id,"action":"dispatch","reason":"r"}]));
    let r = run_now(&td, true).await;
    assert_eq!(r.decisions[0].outcome, TwinDecisionOutcome::Executed);
    assert_eq!(r.decisions[1].skip_reason, Some(TwinSkipReason::NoCapacity));
    let now = td.ctx.judge.clock().now();
    sqlx::query("INSERT INTO twin_sessions VALUES ('old', ?, 'local', ?)")
        .bind(&a.id)
        .bind(now as i64)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    assert_eq!(get(&td.ctx).await.unwrap().today.dispatches, 1);
    td.ctx.judge.clock().set(now + 2 * 24 * 60 * MINUTE);
    assert_eq!(get(&td.ctx).await.unwrap().today.dispatches, 0);
    td.shutdown().await;
}

#[tokio::test]
async fn three_failed_sessions_are_counted_once_and_hand_back() {
    let (td, fake) = fixture(true).await;
    td.new_repo("p");
    let t = todo(&td, "crash").await;
    for _ in 0..3 {
        let r = plan(&td, &fake, &t, "dispatch").await;
        let sid = r.decisions.iter().find_map(|d| d.session_id.as_ref()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    td.ctx.sessions.get_summary(sid).await.unwrap().status,
                    SessionStatus::Error | SessionStatus::Interrupted
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    let r = run_now(&td, false).await;
    assert_eq!(r.result, TwinRunResult::Idle);
    assert!(r.decisions.iter().any(|d| d.action == TwinAction::HandBack));
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    let mut client = td.local_cli_client();
    client.call::<Todo>(&td, methods::TODO_UPDATE, json!({"id":t.id,"handler":"twin"})).await.unwrap();
    let r = plan(&td, &fake, &t, "wait").await;
    assert_eq!(r.decisions[0].action, TwinAction::Wait);
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Twin);
    td.shutdown().await;
}

#[tokio::test]
async fn old_success_does_not_repeatedly_reset_later_dispatch_failures() {
    let (td, fake) = fixture(true).await;
    let repo = td.new_repo("p");
    let t = todo(&td, "work").await;
    let first = plan(&td, &fake, &t, "dispatch").await;
    let sid = first.decisions[0].session_id.as_ref().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if td.ctx.sessions.get_summary(sid).await.unwrap().status == SessionStatus::Idle {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    plan(&td, &fake, &t, "wait").await;
    std::fs::remove_dir_all(repo).unwrap();
    for _ in 0..3 {
        plan(&td, &fake, &t, "dispatch").await;
    }
    assert_eq!(td.ctx.store.todo_get(&t.id).await.unwrap().unwrap().handler, TodoHandler::Me);
    td.shutdown().await;
}
