//! CLI 提炼的等待、即时返回、JSON、列表、失败与 agent 环境护栏；后端与会话都用本机测试台。
mod common;

use anycon_daemon::judge::{CallError, JudgeReply};
use anycon_daemon::testutil::{FakeJudgeBackend, TestDaemon};
use anycon_protocol::*;
use common::anycon;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

async fn fixture() -> (TestDaemon, Arc<FakeJudgeBackend>, String) {
    let td = TestDaemon::start_with(
        &[
            (
                "judge.backends.local",
                "{kind='api',format='openai',model='fake',base_url='http://localhost:1',timeout='10s'}",
            ),
            ("judge.extract", "local"),
        ],
        true,
    )
    .await;
    let fake = FakeJudgeBackend::new([]);
    let backend = fake.clone();
    td.ctx.judge.set_backend_factory(Arc::new(move |_| backend.clone()));
    let mut c = td.local_cli_client();
    let session: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({"agent":"fake"})).await.unwrap();
    (td, fake, session.id)
}
async fn point(td: &TestDaemon, sid: &str) -> u64 {
    let seq = td.ctx.store.max_seq(sid).await.unwrap() + 1;
    td.ctx
        .store
        .append_event(&SessionEvent {
            session_id: sid.into(),
            seq,
            ts_ms: td.ctx.judge.clock().now(),
            turn_id: None,
            event: Event::PermissionResolved {
                request_id: uuid::Uuid::new_v4().to_string(),
                option_id: None,
                by_device: "d".into(),
                answers: None,
                message: Some("Use tests".into()),
            },
        })
        .await
        .unwrap();
    seq
}
fn answer(seq: u64) -> Result<JudgeReply, CallError> {
    Ok(JudgeReply {
        text: json!({"rules":[{"text":"Write tests first.\nThen implement.","seq":seq,"scope":"node"}]}).to_string(),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extract_waits_prints_full_rules_and_runs_filters_by_session_prefix() {
    let (td, fake, sid) = fixture().await;
    let seq = point(&td, &sid).await;
    fake.push(answer(seq));
    *fake.delay.lock().unwrap() = Duration::from_millis(100);
    let prefix = &sid[..12];
    let r = anycon(&td.paths.home, &["rule", "extract", prefix], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Write tests first.\nThen implement."), "{}", r.stdout);
    assert!(r.stdout.contains("anycon rule confirm") && r.stdout.contains("added 1; dropped 0"));
    let r = anycon(&td.paths.home, &["rule", "runs", "--session", prefix, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!(j["used_today"], 1);
    assert_eq!(j["runs"][0]["session_id"], sid);
    assert_eq!(j["runs"][0]["status"], "done");
    let r = anycon(&td.paths.home, &["rule", "runs"], None, &[]).await;
    assert!(
        r.stdout.contains("today: 1 / 20 calls")
            && r.stdout.contains("TRIGGER")
            && r.stdout.contains("manual")
            && r.stdout.contains("done")
    );
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extract_json_no_wait_and_no_decision_points() {
    let (td, fake, sid) = fixture().await;
    for flags in [vec![], vec!["--json"]] {
        let mut args = vec!["rule", "extract", sid.as_str()];
        args.extend(flags);
        let r = anycon(&td.paths.home, &args, None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        if args.contains(&"--json") {
            assert_eq!(r.json(), json!({"points":0}));
        } else {
            assert!(r.stdout.contains("no new decision points"));
        }
    }
    assert!(fake.requests.lock().unwrap().is_empty());
    let seq = point(&td, &sid).await;
    fake.push(answer(seq));
    let r = anycon(&td.paths.home, &["rule", "extract", &sid, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["status"], "done");
    for json_output in [false, true] {
        point(&td, &sid).await;
        fake.push(Ok(JudgeReply { text: "{\"rules\":[]}".into(), ..Default::default() }));
        *fake.delay.lock().unwrap() = Duration::from_millis(500);
        let mut args = vec!["rule", "extract", &sid, "--no-wait"];
        if json_output {
            args.push("--json");
        }
        let r = anycon(&td.paths.home, &args, None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        if json_output {
            assert_eq!(r.json()["run"]["status"], "running");
        } else {
            assert!(r.stdout.contains("extraction started"));
        }
        tokio::time::timeout(Duration::from_secs(10), td.ctx.extractor.wait_idle()).await.unwrap();
    }
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_have_exit_one_budget_details_and_agent_extraction_is_denied() {
    let (td, fake, sid) = fixture().await;
    point(&td, &sid).await;
    fake.push(Err(CallError::Auth));
    let r = anycon(&td.paths.home, &["rule", "extract", &sid], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("auth") && r.stderr.contains("authentication failed"));
    fake.push(Ok(JudgeReply { text: "bad output".into(), ..Default::default() }));
    let r = anycon(&td.paths.home, &["rule", "extract", &sid, "--json"], None, &[]).await;
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("bad_output"));
    assert!(r.stdout.contains("bad_output") && r.stdout.contains("failed"));
    anycon_daemon::config::set_value(&td.paths, "judge.daily_calls", "2").unwrap();
    td.ctx.config.reload().unwrap();
    let r = anycon(&td.paths.home, &["rule", "extract", &sid], None, &[]).await;
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("budget") && r.stderr.contains("2 / 2"), "{}", r.stderr);
    for env in [[("ANYCON_AGENT", "1")], [("ANYCON_AGENT", "claude-code")]] {
        let r = anycon(&td.paths.home, &["rule", "extract", &sid], None, &env).await;
        assert_eq!(r.code, 5, "{}", r.stderr);
        assert!(r.stderr.contains("not available inside"));
    }
    assert_eq!(fake.requests.lock().unwrap().len(), 2);
    td.shutdown().await;
}
