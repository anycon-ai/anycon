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
    for sub in ["confirm", "reject", "disable", "enable", "restore", "rm"] {
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
    assert_eq!(
        j["first_message"],
        "Tidy the changelog\n\nWhen you are done, end your final message with a short report: what you changed, how you verified it (commands you ran and their results), and anything left undone or uncertain."
    );
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
    for want in ["add", "ls", "show", "confirm", "reject", "disable", "enable", "restore", "rm"] {
        assert!(r.stdout.contains(want), "{want}: {}", r.stdout);
    }
    let r = anycon(&home, &["rule", "add", "--help"], None, &[]).await;
    assert!(r.stdout.contains("--project") && r.stdout.contains("600 characters"), "{}", r.stdout);
    let r = anycon(&home, &["rule", "ls", "--help"], None, &[]).await;
    assert!(r.stdout.contains("--all"), "{}", r.stdout);
    let r = anycon(&home, &["--help"], None, &[]).await;
    assert!(r.stdout.contains("rule"), "{}", r.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn show_prints_the_whole_rule_and_confirm_prints_the_text_it_activates() {
    let n = node().await;
    let long = format!("{} {}", "word".repeat(40), "END-OF-THE-RULE");
    let r = n.cli_env(&["rule", "add", &format!("{long}\nsecond line"), "--json"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let id = r.json()["id"].as_str().unwrap().to_string();
    // ls 把规则截成一行；show 给全文、来源、项目、状态
    let ls = n.cli(&["rule", "ls"]).await;
    assert!(!ls.stdout.contains("END-OF-THE-RULE"), "{}", ls.stdout);
    let r = n.cli(&["rule", "show", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    for want in ["END-OF-THE-RULE", "second line", "pending", "proposed by an agent", "(all projects)"] {
        assert!(r.stdout.contains(want), "{want}: {}", r.stdout);
    }
    assert_eq!(n.cli(&["rule", "show", &id[..8], "--json"]).await.json()["id"], id.as_str());
    // 在 agent 里也能看（只读）
    assert_eq!(n.cli_env(&["rule", "show", &id[..8]], &[("ANYCON_AGENT", "1")]).await.code, 0);
    assert_eq!(n.cli(&["rule", "show", "zzzz"]).await.code, 1);
    // confirm 把全文给人看
    let r = n.cli(&["rule", "confirm", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.starts_with("confirmed ") && r.stdout.contains("END-OF-THE-RULE") && r.stdout.contains("second line"),
        "{}",
        r.stdout
    );
    // enable 同样
    n.cli(&["rule", "disable", &id[..8]]).await;
    let r = n.cli(&["rule", "enable", &id[..8]]).await;
    assert!(r.stdout.starts_with("enabled ") && r.stdout.contains("END-OF-THE-RULE"), "{}", r.stdout);
    // 终端转义在 show 里显示成转义形式，不被解释（agent 写的待确认规则：控制字符进不来，但不可见字符也被拒绝）
    let bad = n.cli(&["rule", "add", "sneaky\u{202E}text"]).await;
    assert_eq!(bad.code, 2, "{}", bad.stderr);
    assert!(bad.stderr.contains("cannot be reviewed"), "{}", bad.stderr);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_rule_records_the_session_it_was_proposed_in() {
    let n = node().await;
    let created = n.cli(&["new", "--agent", "mock", "--no-wait", "--json", "hello"]).await;
    assert_eq!(created.code, 0, "{}", created.stderr);
    let sid = created.json()["id"].as_str().unwrap().to_string();
    let r = n
        .cli_env(
            &["rule", "add", "proposed in a session", "--json"],
            &[("ANYCON_AGENT", "1"), ("ANYCON_SESSION_ID", sid.as_str())],
        )
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["source"]["session_id"], sid.as_str());
    let id = r.json()["id"].as_str().unwrap().to_string();
    let shown = n.cli(&["rule", "show", &id[..8]]).await;
    assert!(shown.stdout.contains(&sid), "{}", shown.stdout);
    // 没有 ANYCON_SESSION_ID：照常提议，只是没有来源会话
    let r = n.cli_env(&["rule", "add", "no session", "--json"], &[("ANYCON_AGENT", "1")]).await;
    assert!(r.json()["source"]["session_id"].is_null());
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_flag_marks_one_node_and_scope_is_visible() {
    let n = node().await;
    let r = n.cli(&["rule", "add", "shared rule", "--group", "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["sync"], "group");
    assert!(r.stderr.contains("Web") && r.stderr.contains("one node only"), "{}", r.stderr);
    let id = r.json()["id"].as_str().unwrap().to_string();
    let shown = n.cli(&["rule", "show", &id]).await;
    assert!(shown.stdout.contains("sync:    group"), "{}", shown.stdout);
    let ls = n.cli(&["rule", "ls"]).await;
    assert!(ls.stdout.contains("SYNC") && ls.stdout.contains("group"), "{}", ls.stdout);
    let bad = n.cli(&["rule", "add", "project", "--group", "--project", "web"]).await;
    assert_ne!(bad.code, 0);
    let agent = n.cli_env(&["rule", "add", "agent", "--group", "--json"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(agent.json()["sync"], "node");
    assert_eq!(agent.json()["status"], "pending");
    n.stop().await;
}

/// 停掉 daemon，直接把这条规则标成过期（过期由 daemon 每天标，没有客户端入口），再起 daemon。
async fn mark_stale_offline(n: &mut Node, id: &str) {
    n.daemon.take().unwrap().stop().await;
    let store = anycon_daemon::store::Store::open(&anycon_daemon::config::Paths::new(&n.home).db()).await.unwrap();
    let mut rule = store.rule_get(id).await.unwrap().unwrap();
    rule.stale = true;
    rule.updated_ms += 1;
    store.rule_save(&rule).await.unwrap();
    store.pool().close().await;
    n.daemon = Some(start_daemon(&n.home, &[("ANYCON_MOCK_AGENT", "1")]).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_rule_is_marked_in_ls_and_restore_gives_it_back() {
    let mut n = node().await;
    let keep = n.add("keep this one").await["id"].as_str().unwrap().to_string();
    let old = n.add("a forgotten habit").await["id"].as_str().unwrap().to_string();
    mark_stale_offline(&mut n, &old).await;

    // ls：过期的标 STALE，不算进一个会话拿到的字节数；--json 带 stale
    let r = n.cli(&["rule", "ls"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let line = |id: &str| r.stdout.lines().find(|l| l.contains(&id[..8])).unwrap_or_default().to_string();
    assert!(line(&old).contains("STALE"), "{}", r.stdout);
    assert!(!line(&keep).contains("STALE"), "{}", r.stdout);
    let rules = n.rules(false).await;
    assert_eq!(rules.iter().find(|r| r["id"] == old.as_str()).unwrap()["stale"], true);
    assert!(rules.iter().find(|r| r["id"] == keep.as_str()).unwrap().get("stale").is_none());
    let budget = n.cli(&["rule", "ls", "--json"]).await.json()["budget"]["node_bytes"].as_u64().unwrap();
    let header = anycon_daemon::rules::TEMPLATE_HEADER.len() as u64;
    assert_eq!(budget, header + 3 + "keep this one".len() as u64, "only the rule that is not stale counts");
    // show 写明它为什么不再交给 agent
    let r = n.cli(&["rule", "show", &old[..8]]).await;
    assert!(r.stdout.contains("STALE") && r.stdout.contains("restore"), "{}", r.stdout);

    // 在 agent 里：拒绝，退出码 5，什么都不改
    let r = n.cli_env(&["rule", "restore", &old[..8]], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert!(r.stderr.contains("not available inside an anycon agent session"), "{}", r.stderr);
    assert!(n.rules(false).await.iter().find(|r| r["id"] == old.as_str()).unwrap()["stale"] == true);

    // 不是过期的：什么都不改
    let r = n.cli(&["rule", "restore", &keep[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("nothing changed") && r.stdout.contains("not stale"), "{}", r.stdout);

    // 恢复：前缀就行，打印全文
    let r = n.cli(&["rule", "restore", &old[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("restored ") && r.stdout.contains("a forgotten habit"), "{}", r.stdout);
    let rules = n.rules(false).await;
    assert!(rules.iter().find(|r| r["id"] == old.as_str()).unwrap().get("stale").is_none());
    let r = n.cli(&["rule", "ls"]).await;
    assert!(!r.stdout.contains("STALE"), "{}", r.stdout);
    n.stop().await;
}
