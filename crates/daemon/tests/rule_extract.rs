//! 提炼的 RPC、记录、规则写入与通知，以及可替换时钟驱动的自动提炼与失败退避。

use std::sync::Arc;
use std::time::Duration;

use anycon_daemon::judge::{CallError, JudgeReply, Usage};
use anycon_daemon::rule_extract::auto_tick;
use anycon_daemon::testutil::{FakeJudgeBackend, TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(10);
const DAY: u64 = 24 * 3600 * 1000;

async fn fixture(limit: u32) -> (TestDaemon, Arc<FakeJudgeBackend>) {
    let td = TestDaemon::start_with(
        &[
            (
                "judge.backends.local",
                "{kind='api',format='openai',model='fake',base_url='http://localhost:1',timeout='10s'}",
            ),
            ("judge.extract", "local"),
            ("judge.daily_calls", &limit.to_string()),
        ],
        false,
    )
    .await;
    let fake = FakeJudgeBackend::new([]);
    let backend = fake.clone();
    td.ctx.judge.set_backend_factory(Arc::new(move |_| backend.clone()));
    (td, fake)
}

async fn session(td: &TestDaemon, project: Option<&str>) -> String {
    if let Some(project) = project {
        std::fs::create_dir_all(td.workspace.join(project)).unwrap();
    }
    let mut c = td.local_cli_client();
    let s: SessionSummary =
        c.call(td, methods::SESSION_CREATE, json!({"agent":"fake","project":project})).await.unwrap();
    s.id
}

async fn point(td: &TestDaemon, sid: &str, ts: u64) -> u64 {
    let seq = td.ctx.store.max_seq(sid).await.unwrap() + 1;
    td.ctx
        .store
        .append_event(&SessionEvent {
            session_id: sid.into(),
            seq,
            ts_ms: ts,
            turn_id: None,
            event: Event::PermissionResolved {
                request_id: uuid::Uuid::new_v4().to_string(),
                option_id: Some("deny".into()),
                by_device: "d".into(),
                answers: None,
                message: Some("Always write tests first".into()),
            },
        })
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET last_seq=?, updated_ms=? WHERE id=?")
        .bind(seq as i64)
        .bind(ts as i64)
        .bind(sid)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    seq
}

fn reply(rules: Value) -> Result<JudgeReply, CallError> {
    Ok(JudgeReply {
        text: json!({"rules":rules}).to_string(),
        usage: Usage { input_tokens: Some(12), output_tokens: Some(4), cost_usd: Some(0.01) },
    })
}

async fn extract(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Result<RuleExtractResult, RpcError> {
    c.call(td, methods::RULE_EXTRACT, json!({"session_id":sid})).await
}
async fn done(td: &TestDaemon, c: &mut TestClient, sid: &str) -> ExtractRun {
    let result = extract(td, c, sid).await.unwrap();
    let id = result.run.unwrap().id;
    let note = c
        .wait_notification(T, |n| n.method == methods::RULE_EXTRACT_DONE && n.params["run"]["id"] == id)
        .await
        .unwrap();
    let done: RuleExtractDone = serde_json::from_value(note.params).unwrap();
    tokio::time::timeout(T, td.ctx.extractor.wait_idle()).await.unwrap();
    done.run
}
fn reason(e: &RpcError) -> Option<&str> {
    e.data.as_ref().and_then(|d| d.reason.as_deref())
}
fn configure(td: &TestDaemon, key: &str, value: &str) {
    anycon_daemon::config::set_value(&td.paths, key, value).unwrap();
    td.ctx.config.reload().unwrap();
}

#[tokio::test]
async fn manual_success_writes_pending_rules_source_audits_notifications_and_watermark() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, Some("project")).await;
    let seq = point(&td, &sid, 1).await;
    fake.push(reply(json!([{"text":"Use tests","seq":seq,"scope":"project"}])));
    let mut c = td.hub_client("human-device", "browser");
    let mut observer = td.local_cli_client();
    let run = done(&td, &mut c, &sid).await;
    assert_eq!(run.status, ExtractRunStatus::Done);
    assert_eq!((run.from_seq, run.to_seq, run.points), (seq, seq, 1));
    assert_eq!(run.added.len(), 1);
    assert_eq!(run.usage.as_ref().unwrap().input_tokens, Some(12));
    let rule = td.ctx.store.rule_get(&run.added[0]).await.unwrap().unwrap();
    assert_eq!(rule.project.as_deref(), Some("project"));
    assert_eq!(
        (rule.status, rule.sync, rule.source.kind),
        (RuleStatus::Pending, RuleSync::Node, RuleSourceKind::Extracted)
    );
    assert_eq!(rule.source.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(rule.source.seq, Some(seq));
    assert_eq!(rule.source.device_id.as_deref(), Some("human-device"));
    let notes = observer.drain_now();
    assert!(notes.iter().any(|n| n.method == methods::RULE_CHANGED && n.params["id"] == rule.id));
    assert!(notes.iter().any(|n| n.method == methods::RULE_EXTRACT_DONE && n.params["run"]["id"] == run.id));
    let audits = td.ctx.audit.query(None, None, 100).await;
    let add = audits.iter().find(|a| a.action == "rule.add").unwrap();
    assert_eq!(add.detail["source_kind"], "extracted");
    let audit = audits.iter().find(|a| a.action == "rule.extract").unwrap();
    assert_eq!(
        audit.detail,
        json!({"trigger":"manual","status":"done","points":1,"added":1,"dropped":{"invalid":0,"duplicate":0,"full":0},"backend":"local"})
    );
    assert!(!serde_json::to_string(&audits).unwrap().contains("Use tests"));
    assert_eq!(td.ctx.store.extract_watermark(&sid).await.unwrap(), Some(seq));
    let call_id: Option<String> = sqlx::query_scalar("SELECT call_id FROM rule_extract_runs WHERE id=?")
        .bind(&run.id)
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap();
    assert!(call_id.is_some());
    let list: RuleExtractRunsResult =
        c.call(&td, methods::RULE_EXTRACT_RUNS, json!({"session_id":sid,"limit":1000})).await.unwrap();
    assert_eq!((list.used_today, list.daily_calls, list.runs.len()), (1, 20, 1));
    assert_eq!(list.runs[0], run);
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.rule_extract);
    assert_eq!(extract(&td, &mut c, &sid).await.unwrap().points, 0);
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn synchronous_checks_have_the_specified_order_and_leave_no_records() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let mut mcp = td.local_mcp_client();
    assert_eq!(extract(&td, &mut mcp, "absent").await.unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
    assert_eq!(extract(&td, &mut c, "absent").await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    let sid = session(&td, None).await;
    let empty = extract(&td, &mut c, &sid).await.unwrap();
    assert!(empty.run.is_none() && empty.points == 0);
    point(&td, &sid, 1).await;
    assert_eq!(reason(&extract(&td, &mut c, &sid).await.unwrap_err()), Some("not_configured"));
    assert!(td.ctx.store.list_extract_runs(None, 100).await.unwrap().is_empty());
    td.shutdown().await;
    let (td, fake) = fixture(0).await;
    let sid = session(&td, None).await;
    let mut c = td.local_cli_client();
    assert_eq!(extract(&td, &mut c, &sid).await.unwrap().points, 0);
    point(&td, &sid, 1).await;
    let e = extract(&td, &mut c, &sid).await.unwrap_err();
    assert_eq!(reason(&e), Some("budget"));
    let data = e.data.unwrap();
    assert_eq!((data.used, data.limit), (Some(0), Some(0)));
    configure(&td, "judge.backends.local.base_url", "https://example.invalid");
    assert_eq!(reason(&extract(&td, &mut c, &sid).await.unwrap_err()), Some("not_configured"));
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(td.ctx.store.list_extract_runs(None, 100).await.unwrap().is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn concurrent_requests_share_one_run_and_other_sessions_are_busy() {
    let (td, fake) = fixture(1).await;
    let sid = session(&td, None).await;
    let other = session(&td, None).await;
    point(&td, &sid, 1).await;
    point(&td, &other, 1).await;
    fake.push(reply(json!([])));
    *fake.delay.lock().unwrap() = Duration::from_millis(250);
    let mut c = td.local_cli_client();
    let first = extract(&td, &mut c, &sid).await.unwrap();
    let second = extract(&td, &mut c, &sid).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(reason(&extract(&td, &mut c, &other).await.unwrap_err()), Some("busy"));
    tokio::time::timeout(T, td.ctx.extractor.wait_idle()).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    assert_eq!(reason(&extract(&td, &mut c, &other).await.unwrap_err()), Some("budget"));
    td.shutdown().await;
}

#[tokio::test]
async fn every_backend_error_and_bad_output_fail_without_advancing_and_can_retry() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, None).await;
    let seq = point(&td, &sid, 1).await;
    let mut c = td.local_cli_client();
    let errors = [
        CallError::Timeout,
        CallError::Network,
        CallError::Http(503),
        CallError::Auth,
        CallError::RateLimited,
        CallError::BadOutput,
        CallError::AgentFailed,
        CallError::Cancelled,
        CallError::Internal,
    ];
    for error in errors {
        fake.push(Err(error));
        let run = done(&td, &mut c, &sid).await;
        assert_eq!(run.status, ExtractRunStatus::Failed);
        assert_eq!(run.error, Some(anycon_daemon::judge::service::protocol_error(error)));
        assert_eq!(td.ctx.store.extract_watermark(&sid).await.unwrap(), None);
        let linked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM rule_extract_runs r JOIN judge_calls c ON c.id=r.call_id WHERE r.id=?",
        )
        .bind(&run.id)
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap();
        assert_eq!(linked, 1);
    }
    fake.push(Ok(JudgeReply { text: "private malformed reply".into(), usage: Usage::default() }));
    let run = done(&td, &mut c, &sid).await;
    assert_eq!(run.error.unwrap().code, JudgeErrorCode::BadOutput);
    fake.push(reply(json!([])));
    let run = done(&td, &mut c, &sid).await;
    assert_eq!(run.status, ExtractRunStatus::Done);
    assert_eq!(td.ctx.store.extract_watermark(&sid).await.unwrap(), Some(seq));
    assert_eq!(td.ctx.store.extract_attempts(&sid).await.unwrap().failures_since_done, 0);
    td.shutdown().await;
}

#[tokio::test]
async fn candidates_are_validated_deduplicated_and_limited_to_five() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, None).await;
    let seq = point(&td, &sid, 1).await;
    let mut c = td.local_cli_client();
    let old: Rule = c
        .call(&td, methods::RULE_ADD, json!({"id":uuid::Uuid::new_v4().to_string(),"text":"Rejected."}))
        .await
        .unwrap();
    let _: Rule = c.call(&td, methods::RULE_UPDATE, json!({"id":old.id,"status":"rejected"})).await.unwrap();
    let mut rules = vec![
        json!({"text":"ＲＥＪＥＣＴＥＤ！","seq":seq,"scope":"node"}),
        json!({"text":"bad\u{200b}","seq":seq,"scope":"node"}),
        json!({"text":"bad seq","seq":99999,"scope":"node"}),
        json!({"text":"same","seq":seq,"scope":"future"}),
        json!({"text":"SAME!","seq":seq,"scope":"project"}),
    ];
    rules.extend((0..7).map(|i| json!({"text":format!("rule {i}"),"seq":seq,"scope":"project"})));
    fake.push(reply(json!(rules)));
    let run = done(&td, &mut c, &sid).await;
    assert_eq!(run.added.len(), 5);
    assert_eq!(run.dropped, ExtractDropped { invalid: 2, duplicate: 2, full: 3 });
    for id in run.added {
        assert!(td.ctx.store.rule_get(&id).await.unwrap().unwrap().project.is_none());
    }
    td.shutdown().await;
}

#[tokio::test]
async fn pending_and_total_capacity_share_rule_add_checks() {
    for (count, source, status) in
        [(50, RuleSourceKind::Extracted, RuleStatus::Pending), (300, RuleSourceKind::Manual, RuleStatus::Disabled)]
    {
        let (td, fake) = fixture(20).await;
        let sid = session(&td, None).await;
        let seq = point(&td, &sid, 1).await;
        for i in 0..count {
            let rule: Rule = serde_json::from_value(json!({"id":format!("existing-{i}"),"text":format!("existing {i}"),"source":{"kind":source},"status":status,"sync":"node","created_ms":1,"updated_ms":1})).unwrap();
            td.ctx.store.rule_insert(&rule).await.unwrap();
        }
        fake.push(reply(json!([{"text":"new","seq":seq,"scope":"node"},{"text":"another","seq":seq,"scope":"node"}])));
        let mut c = td.local_cli_client();
        let run = done(&td, &mut c, &sid).await;
        assert_eq!(run.status, ExtractRunStatus::Done);
        assert!(run.added.is_empty());
        assert_eq!(run.dropped.full, 2);
        assert_eq!(td.ctx.store.rule_count().await.unwrap(), count);
        td.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_cancels_and_finishes_record_before_closing_the_store() {
    let (mut td, fake) = fixture(20).await;
    let sid = session(&td, None).await;
    point(&td, &sid, 1).await;
    fake.push(reply(json!([])));
    *fake.delay.lock().unwrap() = Duration::from_secs(60);
    let mut c = td.local_cli_client();
    let run = extract(&td, &mut c, &sid).await.unwrap().run.unwrap();
    tokio::time::timeout(T, async {
        while fake.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(T, td.daemon.take().unwrap().shutdown()).await.unwrap();
    let note = c.wait_notification(T, |n| n.method == methods::RULE_EXTRACT_DONE).await.unwrap();
    assert_eq!(note.params["run"]["error"]["code"], "cancelled");
    let store = anycon_daemon::store::Store::open(&td.paths.db()).await.unwrap();
    let run = store.get_extract_run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, ExtractRunStatus::Failed);
    assert_eq!(store.extract_watermark(&sid).await.unwrap(), None);
    store.close().await;
}

#[tokio::test]
async fn auto_obeys_switch_window_order_budget_busy_and_failure_backoff() {
    let (td, fake) = fixture(20).await;
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now);
    let old = session(&td, None).await;
    let recent = session(&td, None).await;
    let ancient = session(&td, None).await;
    let empty = session(&td, None).await;
    point(&td, &old, now - DAY).await;
    point(&td, &recent, now - 1000).await;
    point(&td, &ancient, now - 8 * DAY).await;
    assert!(td.ctx.store.extract_watermark(&empty).await.unwrap().is_none());
    auto_tick(&td.ctx).await.unwrap();
    assert!(fake.requests.lock().unwrap().is_empty());
    configure(&td, "rules.extract_auto", "true");
    fake.push(Err(CallError::Network));
    auto_tick(&td.ctx).await.unwrap();
    let runs = td.ctx.store.list_extract_runs(None, 100).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].session_id, old);
    assert_eq!(runs[0].trigger, ExtractTrigger::Auto);
    assert!(runs[0].device_id.is_none());
    auto_tick(&td.ctx).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    // 推进时钟，把另外两条的最后事件移出窗口，单独验证连续失败。
    sqlx::query("UPDATE events SET ts_ms=? WHERE session_id IN (?,?)")
        .bind((now - 10 * DAY) as i64)
        .bind(&recent)
        .bind(&ancient)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    for day in 1..=2 {
        td.ctx.judge.clock().set(now + day * DAY);
        fake.push(Err(CallError::Timeout));
        auto_tick(&td.ctx).await.unwrap();
    }
    assert_eq!(fake.requests.lock().unwrap().len(), 3);
    td.ctx.judge.clock().set(now + 3 * DAY);
    auto_tick(&td.ctx).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 3);
    let mut c = td.local_cli_client();
    fake.push(reply(json!([])));
    assert_eq!(done(&td, &mut c, &old).await.status, ExtractRunStatus::Done);
    point(&td, &old, now + 2 * DAY).await;
    fake.push(reply(json!([])));
    auto_tick(&td.ctx).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 5);
    // 开关和后端选择都在每轮热加载。
    point(&td, &old, now + 2 * DAY).await;
    configure(&td, "judge.extract", "missing");
    auto_tick(&td.ctx).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 5);
    td.shutdown().await;
}

#[tokio::test]
async fn automatic_sessions_run_oldest_first_stop_at_budget_and_yield_to_manual_work() {
    let (td, fake) = fixture(1).await;
    configure(&td, "rules.extract_auto", "true");
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now);
    let newer = session(&td, None).await;
    let older = session(&td, None).await;
    point(&td, &newer, now - DAY).await;
    point(&td, &older, now - 2 * DAY).await;
    fake.push(reply(json!([])));
    auto_tick(&td.ctx).await.unwrap();
    let runs = td.ctx.store.list_extract_runs(None, 100).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].session_id, older);
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    configure(&td, "judge.daily_calls", "10");
    point(&td, &older, now - DAY).await;
    fake.push(reply(json!([])));
    *fake.delay.lock().unwrap() = Duration::from_secs(1);
    let mut c = td.local_cli_client();
    extract(&td, &mut c, &older).await.unwrap();
    tokio::time::timeout(Duration::from_millis(500), auto_tick(&td.ctx)).await.unwrap().unwrap();
    assert_eq!(td.ctx.store.list_extract_runs(None, 100).await.unwrap().len(), 2);
    tokio::time::timeout(T, td.ctx.extractor.wait_idle()).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn extraction_advances_only_to_the_prefix_included_in_the_call() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, None).await;
    let mut last = 0;
    for _ in 0..41 {
        last = point(&td, &sid, 1).await;
    }
    fake.push(reply(json!([])));
    fake.push(reply(json!([])));
    let mut c = td.local_cli_client();
    let first = done(&td, &mut c, &sid).await;
    assert_eq!(first.points, 40);
    assert_eq!(first.to_seq, last - 1);
    let second = done(&td, &mut c, &sid).await;
    assert_eq!(second.points, 1);
    assert_eq!(second.to_seq, last);
    td.shutdown().await;
}

#[tokio::test]
async fn automatic_extraction_skips_live_running_queued_and_permission_sessions() {
    let (td, fake) = fixture(20).await;
    configure(&td, "rules.extract_auto", "true");
    configure(&td, "sessions.max_concurrent", "2");
    let mut c = td.local_cli_client();
    let mut sessions = Vec::new();
    for prompt in ["hold", "permission", "hold"] {
        let s: SessionSummary = c
            .call(
                &td,
                methods::SESSION_CREATE,
                json!({
                    "agent":"fake", "prompt":prompt, "message_id":uuid::Uuid::new_v4().to_string()
                }),
            )
            .await
            .unwrap();
        sessions.push(s.id);
    }
    for (id, expected) in
        sessions.iter().zip([SessionStatus::Running, SessionStatus::WaitingPermission, SessionStatus::Queued])
    {
        tokio::time::timeout(T, async {
            while td.ctx.sessions.get_summary(id).await.unwrap().status != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        point(&td, id, td.ctx.judge.clock().now() - DAY).await;
    }
    auto_tick(&td.ctx).await.unwrap();
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(td.ctx.store.list_extract_runs(None, 20).await.unwrap().is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn shutdown_after_backend_reply_finishes_rules_and_advances_watermark() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, None).await;
    let seq = point(&td, &sid, 1).await;
    fake.push(reply(json!([{"text":"Use tests","seq":seq,"scope":"node"}])));
    let lock = td.ctx.store.rule_lock().lock().await;
    let mut c = td.local_cli_client();
    let run = extract(&td, &mut c, &sid).await.unwrap().run.unwrap();
    tokio::time::timeout(T, async {
        while fake.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        td.ctx.judge.wait_idle().await;
    })
    .await
    .unwrap();
    td.ctx.shutdown_tx.send(true).unwrap();
    drop(lock);
    tokio::time::timeout(T, td.ctx.extractor.wait_idle()).await.unwrap();
    let run = td.ctx.store.get_extract_run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, ExtractRunStatus::Done);
    assert!(run.error.is_none());
    assert_eq!(run.added.len(), 1);
    assert_eq!(td.ctx.store.rule_count().await.unwrap(), 1);
    assert_eq!(td.ctx.store.extract_watermark(&sid).await.unwrap(), Some(seq));
    td.shutdown().await;
}

#[tokio::test]
async fn malformed_candidates_are_dropped_individually_and_missing_scope_defaults_to_node() {
    let (td, fake) = fixture(20).await;
    let sid = session(&td, Some("project")).await;
    let seq = point(&td, &sid, 1).await;
    let mut c = td.local_cli_client();
    for invalid in [json!({"seq":seq}), json!({"text":42,"seq":seq}), json!({"text":"bad","seq":seq.to_string()})] {
        let batch = uuid::Uuid::new_v4().to_string();
        fake.push(reply(json!([
            {"text":format!("First {batch}"),"seq":seq},
            invalid,
            {"text":format!("Second {batch}"),"seq":seq,"scope":"unknown"},
            {"text":format!("Third {batch}"),"seq":seq,"scope":42}
        ])));
        // 独立会话避免前一次成功推进水位，仍复用同一组候选字段。
        let current = session(&td, Some("project")).await;
        assert_eq!(point(&td, &current, 1).await, seq);
        let run = done(&td, &mut c, &current).await;
        assert_eq!(run.status, ExtractRunStatus::Done);
        assert_eq!(run.added.len(), 3);
        assert_eq!(run.dropped.invalid, 1);
        for id in &run.added {
            assert!(td.ctx.store.rule_get(id).await.unwrap().unwrap().project.is_none());
        }
    }
    td.shutdown().await;
}

#[tokio::test]
async fn automatic_empty_scan_is_cached_until_last_seq_changes_and_manual_scan_bypasses_cache() {
    let (td, fake) = fixture(20).await;
    configure(&td, "rules.extract_auto", "true");
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now);
    let sid = session(&td, None).await;
    let seq = point(&td, &sid, now - DAY).await;
    let empty_event = json!({"type":"text_delta","text":"No decision here"}).to_string();
    sqlx::query("UPDATE events SET event=? WHERE session_id=? AND seq=?")
        .bind(&empty_event)
        .bind(&sid)
        .bind(seq as i64)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    auto_tick(&td.ctx).await.unwrap();
    // 破坏已扫描事件：若第二轮仍读取事件，反序列化必然失败。
    sqlx::query("UPDATE events SET event='invalid json' WHERE session_id=? AND seq=?")
        .bind(&sid)
        .bind(seq as i64)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    auto_tick(&td.ctx).await.unwrap();
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(td.ctx.store.list_extract_runs(None, 100).await.unwrap().is_empty());
    let mut c = td.local_cli_client();
    assert_eq!(extract(&td, &mut c, &sid).await.unwrap_err().stable_code(), Some(ErrorCode::Internal));
    let next = point(&td, &sid, now - DAY).await;
    assert_eq!(auto_tick(&td.ctx).await.unwrap_err().stable_code(), Some(ErrorCode::Internal));
    sqlx::query("UPDATE events SET event=? WHERE session_id=? AND seq=?")
        .bind(&empty_event)
        .bind(&sid)
        .bind(seq as i64)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    fake.push(reply(json!([])));
    auto_tick(&td.ctx).await.unwrap();
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    assert_eq!(td.ctx.store.extract_watermark(&sid).await.unwrap(), Some(next));
    td.shutdown().await;
}

#[tokio::test]
async fn automatic_scan_filters_session_update_time_before_reading_events() {
    let (td, fake) = fixture(20).await;
    configure(&td, "rules.extract_auto", "true");
    let now = td.ctx.judge.clock().now();
    td.ctx.judge.clock().set(now);
    let sid = session(&td, None).await;
    point(&td, &sid, now - DAY).await;
    // 构造时间不一致的旧会话；若未先按 updated_ms 过滤，后续扫描会读到损坏的事件。
    sqlx::query("UPDATE sessions SET updated_ms=? WHERE id=?")
        .bind((now - 7 * DAY - 1) as i64)
        .bind(&sid)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE events SET event='invalid json' WHERE session_id=?")
        .bind(&sid)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    auto_tick(&td.ctx).await.unwrap();
    assert!(fake.requests.lock().unwrap().is_empty());
    sqlx::query("UPDATE sessions SET updated_ms=? WHERE id=?")
        .bind((now - 7 * DAY) as i64)
        .bind(&sid)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    assert_eq!(auto_tick(&td.ctx).await.unwrap_err().stable_code(), Some(ErrorCode::Internal));
    td.shutdown().await;
}
