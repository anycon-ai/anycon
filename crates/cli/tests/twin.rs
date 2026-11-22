//! CLI 分身命令和 agent 环境护栏；socket、判断后端和 adapter 都来自临时测试台。
mod common;

use anycon_daemon::judge::JudgeReply;
use anycon_daemon::testutil::{FakeJudgeBackend, TestDaemon};
use anycon_protocol::*;
use common::anycon;
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_twin_commands_and_todo_assignment() {
    let td = TestDaemon::start_with(
        &[
            ("judge.triage", "local"),
            (
                "judge.backends.local",
                "{kind='api',format='openai',model='fake',base_url='http://localhost:1',timeout='10s'}",
            ),
        ],
        true,
    )
    .await;
    td.fake.declare_no_push.store(true, Ordering::SeqCst);
    td.ctx.agents.handle_list(AgentListParams { refresh: true, ..Default::default() }).await;
    let fake = FakeJudgeBackend::new([]);
    let backend = fake.clone();
    td.ctx.judge.set_backend_factory(Arc::new(move |_| backend.clone()));
    td.ctx.judge.clock().set(anycon_daemon::util::now_ms());
    let status = anycon(&td.paths.home, &["twin", "status", "--json"], None, &[]).await;
    assert_eq!(status.code, 0, "{}", status.stderr);
    assert_eq!(status.json()["state"], "not_configured");
    let set = anycon(
        &td.paths.home,
        &[
            "twin",
            "set",
            "--projects",
            "p,q",
            "--agent",
            "fake",
            "--mode",
            "ask",
            "--max-sessions",
            "1",
            "--daily-dispatches",
            "3",
            "--json",
        ],
        None,
        &[],
    )
    .await;
    assert_eq!(set.code, 0, "{}", set.stderr);
    assert_eq!(set.json()["scope"]["projects"], json!(["p", "q"]));
    for cmd in ["pause", "resume"] {
        let r = anycon(&td.paths.home, &["twin", cmd], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stdout.contains(if cmd == "pause" { "paused" } else { "idle" }));
    }
    let add = anycon(&td.paths.home, &["todo", "add", "work", "--project", "p", "--twin", "--json"], None, &[]).await;
    assert_eq!(add.code, 0, "{}", add.stderr);
    let todo = add.json();
    assert_eq!(todo["handler"], "twin");
    let id = todo["id"].as_str().unwrap();
    for handler in ["me", "twin"] {
        let r = anycon(&td.paths.home, &["todo", "assign", &id[..8], handler, "--json"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert_eq!(r.json()["handler"], handler);
    }
    fake.push(Ok(JudgeReply {
        text: json!({"items":[{"id":id,"action":"wait","reason":"Wait for the result"}]}).to_string(),
        ..Default::default()
    }));
    let dry = anycon(&td.paths.home, &["twin", "dry-run"], None, &[]).await;
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert!(dry.stdout.contains("wait") && dry.stdout.contains("Wait for the result"));
    fake.push(Ok(JudgeReply {text:json!({"items":[{"id":id,"action":"ask_user","reason":"Preference","question":"Which?","options":["A","B"]}]}).to_string(),..Default::default()}));
    let wake = anycon(&td.paths.home, &["twin", "wake"], None, &[]).await;
    assert_eq!(wake.code, 0, "{}", wake.stderr);
    td.ctx.judge.clock().set(anycon_daemon::util::now_ms() + 61_000);
    td.ctx.twin.poll(&td.ctx).await.unwrap();
    td.ctx.twin.wait_idle().await;
    let questions = anycon(&td.paths.home, &["twin", "questions", "--json"], None, &[]).await;
    assert_eq!(questions.code, 0, "{}", questions.stderr);
    let questions = questions.json();
    let qid = questions["questions"][0]["id"].as_str().unwrap();
    let text = anycon(&td.paths.home, &["twin", "questions"], None, &[]).await;
    assert!(text.stdout.contains("Which?") && text.stdout.contains("A"));
    let answer = anycon(&td.paths.home, &["twin", "answer", qid, "A", "--json"], None, &[]).await;
    assert_eq!(answer.code, 0, "{}", answer.stderr);
    assert_eq!(answer.json()["status"], "answered");
    for flags in [vec![], vec!["--json"]] {
        let mut args = vec!["twin", "runs"];
        args.extend(flags);
        let r = anycon(&td.paths.home, &args, None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stdout.contains("dry_run") || r.stdout.contains("dry run"));
    }
    let clear =
        anycon(&td.paths.home, &["twin", "set", "--clear-agent", "--clear-model", "--clear-mode", "--json"], None, &[])
            .await;
    assert_eq!(clear.code, 0, "{}", clear.stderr);
    assert!(clear.json()["scope"].get("mode").is_none());
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_writes_refuse_agent_environment_before_connecting() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["twin", "pause"],
        vec!["twin", "resume"],
        vec!["twin", "wake"],
        vec!["twin", "dry-run"],
        vec!["twin", "answer", "q", "a"],
        vec!["twin", "set", "--projects", "p"],
        vec!["todo", "add", "work", "--twin"],
        vec!["todo", "assign", "t", "twin"],
    ] {
        let r = anycon(home.path(), &args, None, &[("ANYCON_AGENT", "1")]).await;
        assert_eq!(r.code, 5, "{args:?}: {}", r.stderr);
        assert!(r.stderr.contains("not available inside an anycon agent session"));
    }
}
