//! `anycon rule` 对真实 daemon 的端到端：增删改查、id 前缀、退出码，以及在 anycon 拉起的 agent 里的行为。

mod common;

use common::*;
use serde_json::Value;

struct Node {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    ws: std::path::PathBuf,
    daemon: Option<DaemonProc>,
}

async fn node() -> Node {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    assert_eq!(anycon(&home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await.code, 0);
    assert_eq!(anycon(&home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await.code, 0);
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    let ws = d.path().join("ws");
    Node { _dir: d, home, ws, daemon: Some(daemon) }
}

impl Node {
    async fn cli(&self, args: &[&str]) -> Run {
        anycon(&self.home, args, None, &[]).await
    }

    async fn cli_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        anycon(&self.home, args, None, env).await
    }

    async fn stop(mut self) {
        if let Some(d) = self.daemon.take() {
            d.stop().await;
        }
    }

    async fn rules(&self, all: bool) -> Vec<Value> {
        let mut args = vec!["rule", "ls", "--json"];
        if all {
            args.push("--all");
        }
        let r = self.cli(&args).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.json()["rules"].as_array().unwrap().clone()
    }

    async fn add(&self, text: &str) -> Value {
        let r = self.cli(&["rule", "add", text, "--json"]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.json()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_ls_and_status_changes_by_id_prefix() {
    let n = node().await;
    let r = n.cli(&["rule", "add", "Write the failing test first."]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("added ") && r.stdout.contains("Write the failing test first."), "{}", r.stdout);

    let a = n.add("Ask before adding a dependency.").await;
    assert_eq!(
        (a["status"].as_str(), a["source"]["kind"].as_str(), a["sync"].as_str()),
        (Some("active"), Some("manual"), Some("node"))
    );
    let id = a["id"].as_str().unwrap().to_string();
    let p = n.cli(&["rule", "add", "Use pnpm here.", "--project", "web", "--json"]).await;
    assert_eq!(p.json()["project"], "web");

    // ls：表格带 id 前缀、状态、项目和规则；--json 带全部字段和字数预算
    let r = n.cli(&["rule", "ls"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(&id[..8]) && r.stdout.contains("active") && r.stdout.contains("web"), "{}", r.stdout);
    assert!(r.stdout.contains("a session gets at most") && r.stdout.contains("4096"), "{}", r.stdout);
    let r = n.cli(&["rule", "ls", "--json"]).await;
    let j = r.json();
    assert_eq!(j["rules"].as_array().unwrap().len(), 3);
    assert_eq!(j["budget"]["cap"], 4096);
    assert!(j["budget"]["projects"]["web"].as_u64().unwrap() > 0);

    // disable：默认的 ls 里没有了，--all 里有；再 enable
    let r = n.cli(&["rule", "disable", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("disabled "), "{}", r.stdout);
    assert_eq!(n.rules(false).await.len(), 2);
    let all = n.rules(true).await;
    assert_eq!(all.len(), 3);
    assert_eq!(all.iter().find(|r| r["id"] == id.as_str()).unwrap()["status"], "disabled");
    let r = n.cli(&["rule", "ls", "--all"]).await;
    assert!(r.stdout.contains("disabled"), "{}", r.stdout);
    let r = n.cli(&["rule", "enable", &id[..8]]).await;
    assert!(r.stdout.starts_with("enabled "), "{}", r.stdout);
    // 已经是这个状态：如实说没变
    let r = n.cli(&["rule", "enable", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("nothing changed") && r.stdout.contains("already active"), "{}", r.stdout);

    // reject：留着，但不在默认的 ls 里
    let r = n.cli(&["rule", "reject", &id[..8]]).await;
    assert!(r.stdout.starts_with("rejected "), "{}", r.stdout);
    assert_eq!(n.rules(false).await.len(), 2);
    let r = n.cli(&["rule", "confirm", &id[..8]]).await;
    assert!(r.stdout.starts_with("confirmed "), "{}", r.stdout);
    assert_eq!(n.rules(false).await.len(), 3);

    // 完整的 UUID 不用列表就能操作
    let r = n.cli(&["rule", "disable", &id]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(&id[..8]), "{}", r.stdout);

    // rm
    let r = n.cli(&["rule", "rm", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("removed "), "{}", r.stdout);
    assert_eq!(n.rules(true).await.len(), 2);
    let r = n.cli(&["rule", "rm", &id[..8]]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not_found"), "{}", r.stderr);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_input_and_ambiguous_ids_are_reported_with_the_right_exit_codes() {
    let n = node().await;
    for args in
        [vec!["rule", "add", "   "], vec!["rule", "add", "x\ty"], vec!["rule", "add", "ok", "--project", "a\nb"]]
    {
        let r = n.cli(&args).await;
        assert_eq!(r.code, 2, "{args:?}: {}", r.stderr);
    }
    let long = "a".repeat(601);
    assert_eq!(n.cli(&["rule", "add", &long]).await.code, 2);
    assert!(n.rules(true).await.is_empty(), "nothing was added");
    // 多行的规则可以
    let r = n.cli(&["rule", "add", "first line\nsecond line", "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["text"], "first line\nsecond line");
    let r = n.cli(&["rule", "ls"]).await;
    assert!(r.stdout.contains("first line / second line"), "{}", r.stdout);

    // 没有匹配
    let r = n.cli(&["rule", "confirm", "zzzz"]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not_found"), "{}", r.stderr);
    // 前缀不唯一：17 个 id 的首字符只有 16 种取值，必有两条共享
    for i in 0..16 {
        n.add(&format!("rule {i}")).await;
    }
    let all = n.rules(true).await;
    assert_eq!(all.len(), 17);
    let first: Vec<char> = all.iter().map(|t| t["id"].as_str().unwrap().chars().next().unwrap()).collect();
    let dup = first.iter().find(|c| first.iter().filter(|x| x == c).count() > 1).unwrap().to_string();
    let r = n.cli(&["rule", "rm", &dup]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("matches") && r.stderr.contains("longer prefix"), "{}", r.stderr);
    assert_eq!(n.rules(true).await.len(), 17, "an ambiguous prefix removes nothing");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adding_past_the_size_a_session_can_take_is_refused_in_plain_words() {
    let n = node().await;
    let t = "x".repeat(600);
    for _ in 0..6 {
        n.add(&t).await;
    }
    let r = n.cli(&["rule", "add", &t]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("4096 bytes") && r.stderr.contains("merge or disable"), "{}", r.stderr);
    assert_eq!(n.rules(true).await.len(), 6);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inside_an_agent_session_add_proposes_and_the_rest_is_refused() {
    let n = node().await;
    let human = n.add("a rule a person wrote").await;
    let hid = human["id"].as_str().unwrap().to_string();

    // add：待确认，来源是 agent，提示怎么确认
    let r = n.cli_env(&["rule", "add", "from the agent", "--json"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!((j["status"].as_str(), j["source"]["kind"].as_str()), (Some("pending"), Some("agent")));
    let id = j["id"].as_str().unwrap().to_string();
    let r = n.cli_env(&["rule", "add", "another one"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("proposed ") && r.stdout.contains("anycon rule confirm"), "{}", r.stdout);

    // ls 照常，标出待确认的
    let r = n.cli_env(&["rule", "ls"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("from the agent")
            && r.stdout.contains("agent (needs review)")
            && r.stdout.contains("pending"),
        "{}",
        r.stdout
    );

    // confirm / reject / disable / enable / rm：拒绝，退出码 5，说明原因，什么都不改
    for sub in ["confirm", "reject", "disable", "enable", "rm"] {
        for target in [&id[..8], &hid[..8]] {
            let r = n.cli_env(&["rule", sub, target], &[("ANYCON_AGENT", "1")]).await;
            assert_eq!(r.code, 5, "{sub}: {}", r.stderr);
            assert!(
                r.stderr.contains("not available inside an anycon agent session") && r.stderr.contains("ANYCON_AGENT"),
                "{sub}: {}",
                r.stderr
            );
        }
    }
    let after = n.rules(true).await;
    assert_eq!(after.len(), 3);
    assert_eq!(after.iter().find(|r| r["id"] == id.as_str()).unwrap()["status"], "pending");
    assert_eq!(after.iter().find(|r| r["id"] == hid.as_str()).unwrap()["status"], "active");

    // 空值和 "0" 不算「在 agent 里」（和 `anycon mcp`、`anycon todo` 是同一个判断）：和人敲的一样
    for off in ["", "0"] {
        let r = n.cli_env(&["rule", "add", "typed by a person", "--json"], &[("ANYCON_AGENT", off)]).await;
        assert_eq!(r.json()["status"], "active", "ANYCON_AGENT={off:?}");
        assert_eq!(r.json()["source"]["kind"], "manual");
    }
    // 人在自己的终端里确认它
    let r = n.cli(&["rule", "confirm", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let after = n.rules(true).await;
    assert_eq!(after.iter().find(|r| r["id"] == id.as_str()).unwrap()["status"], "active");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dispatch_preview_lists_the_rules_and_the_history_says_how_many_were_given() {
    let n = node().await;
    std::fs::create_dir_all(n.ws.join("proj")).unwrap();
    n.add("Write the failing test first.").await;
    let r = n.cli(&["todo", "add", "Tidy the changelog", "--project", "proj", "--json"]).await;
    let todo = r.json()["id"].as_str().unwrap().to_string();

    // 第一条消息还是它自己；规则不在消息里，另外列在后面
    let r = n.cli(&["todo", "run", &todo[..8], "--dry-run", "--allow-push"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("Tidy the changelog\n"), "{}", r.stdout);
    assert!(
        r.stdout.contains("Standing rules from the user.") && r.stdout.contains("- Write the failing test first."),
        "{}",
        r.stdout
    );
    let r = n.cli(&["todo", "run", &todo[..8], "--dry-run", "--allow-push", "--json"]).await;
    let j = r.json();
    assert_eq!(j["first_message"], "Tidy the changelog");
    assert!(j["rules_text"].as_str().unwrap().ends_with("- Write the failing test first."), "{j}");

    // 派发：「经过」里写着给了几条规则
    let r = n.cli(&["todo", "run", &todo[..8], "--agent", "mock", "--allow-push", "--local"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let h = n.cli(&["todo", "history", &todo[..8]]).await;
    assert!(h.stdout.contains("rules: 1 given in the system prompt"), "{}", h.stdout);
    let h = n.cli(&["todo", "history", &todo[..8], "--json"]).await;
    let ev = h.json()["events"].as_array().unwrap().iter().find(|e| e["kind"] == "dispatched").unwrap().clone();
    assert_eq!(ev["rules"].as_array().unwrap().len(), 1);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cli_help_documents_the_subcommands_in_english() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let r = anycon(&home, &["rule", "--help"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    for want in ["add", "ls", "confirm", "reject", "disable", "enable", "rm"] {
        assert!(r.stdout.contains(want), "{want}: {}", r.stdout);
    }
    let r = anycon(&home, &["rule", "add", "--help"], None, &[]).await;
    assert!(r.stdout.contains("--project") && r.stdout.contains("600 characters"), "{}", r.stdout);
    let r = anycon(&home, &["rule", "ls", "--help"], None, &[]).await;
    assert!(r.stdout.contains("--all"), "{}", r.stdout);
    let r = anycon(&home, &["--help"], None, &[]).await;
    assert!(r.stdout.contains("rule"), "{}", r.stdout);
}
