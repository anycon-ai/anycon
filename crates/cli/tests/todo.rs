//! `anycon todo` 对真实 daemon 的端到端：增删改查、id 前缀、`--due`、退出码，以及在 anycon 拉起的 agent 里的行为。

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

    async fn todos(&self, all: bool) -> Vec<Value> {
        let mut args = vec!["todo", "ls", "--json"];
        if all {
            args.push("--all");
        }
        let r = self.cli(&args).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.json()["todos"].as_array().unwrap().clone()
    }

    async fn add(&self, title: &str) -> Value {
        let r = self.cli(&["todo", "add", title, "--json"]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.json()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_ls_and_state_changes_by_id_prefix() {
    let n = node().await;
    assert_eq!(n.todos(false).await.len(), 0);
    let empty = n.cli(&["todo", "ls"]).await;
    assert_eq!((empty.code, empty.stdout.trim()), (0, "no to-dos"));

    // add：标题、正文、项目、截止时间；人在本机敲的，来源是 local，不是 unreviewed
    let r = n
        .cli(&[
            "todo",
            "add",
            "Write the report",
            "--body",
            "# notes",
            "--project",
            "web",
            "--due",
            "2026-12-01",
            "--json",
        ])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let t = r.json();
    assert_eq!(t["title"], "Write the report");
    assert_eq!(
        (t["body"].as_str(), t["project"].as_str(), t["state"].as_str()),
        (Some("# notes"), Some("web"), Some("open"))
    );
    assert_eq!(t["due_ms"], 1_796_083_200_000u64);
    assert_eq!(t["source"]["kind"], "local");
    assert_eq!(t["unreviewed"], false);
    let id = t["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 36);
    // 人类可读的输出：短 id 和标题
    let r = n.cli(&["todo", "add", "Second item"]).await;
    assert!(r.stdout.starts_with("added ") && r.stdout.contains("Second item"), "{}", r.stdout);
    let second = n.todos(false).await.into_iter().find(|t| t["title"] == "Second item").unwrap();

    // ls 表格
    let r = n.cli(&["todo", "ls"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    for want in [
        "ID",
        "STATE",
        "DUE",
        "PROJECT",
        "TITLE",
        "Write the report",
        "web",
        "2026-12-01 00:00",
        "Second item",
        &id[..8],
    ] {
        assert!(r.stdout.contains(want), "{want:?} in:\n{}", r.stdout);
    }
    // --project 过滤
    let r = n.cli(&["todo", "ls", "--project", "web", "--json"]).await;
    assert_eq!(r.json()["todos"].as_array().unwrap().len(), 1);

    // done：用前缀；默认的 ls 不再列出，--all 才有
    let r = n.cli(&["todo", "done", &id[..6]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("done ") && r.stdout.contains("Write the report"), "{}", r.stdout);
    let open = n.todos(false).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["id"], second["id"]);
    let all = n.todos(true).await;
    assert_eq!(all.len(), 2);
    let done = all.iter().find(|t| t["id"] == id.as_str()).unwrap();
    assert_eq!(done["state"], "done");
    assert!(done["closed_ms"].is_u64());
    // 已关闭的项也能用前缀找到：reopen 清掉 closed_ms
    let r = n.cli(&["todo", "reopen", &id, "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!((j["state"].as_str(), j["closed_ms"].is_null()), (Some("open"), true));
    // drop
    let r = n.cli(&["todo", "drop", &id[..8]]).await;
    assert!(r.stdout.starts_with("dropped "), "{}", r.stdout);
    assert_eq!(n.todos(true).await.iter().find(|t| t["id"] == id.as_str()).unwrap()["state"], "dropped");
    // rm：物理删除，再删找不到
    let r = n.cli(&["todo", "rm", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("removed "), "{}", r.stdout);
    assert_eq!(n.todos(true).await.len(), 1);
    let r = n.cli(&["todo", "rm", &id]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not_found"), "{}", r.stderr);
    // 前缀不区分大小写
    let up = second["id"].as_str().unwrap()[..8].to_uppercase();
    assert_eq!(n.cli(&["todo", "done", &up]).await.code, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_input_and_ambiguous_ids_are_reported_with_the_right_exit_codes() {
    let n = node().await;
    // 标题为空 / 太长 / 多行、不合法的 --due：退出码 2
    for args in [
        vec!["todo", "add", "   "],
        vec!["todo", "add", "x\ny"],
        vec!["todo", "add", "title", "--due", "tomorrow"],
        vec!["todo", "add", "title", "--due", "2026-12-01 09:00"],
    ] {
        let r = n.cli(&args).await;
        assert_eq!(r.code, 2, "{args:?}: {}", r.stderr);
    }
    let long = "a".repeat(201);
    assert_eq!(n.cli(&["todo", "add", &long]).await.code, 2);
    assert!(n.todos(true).await.is_empty(), "nothing was added");
    // --due 的几种写法
    for (due, ms) in [
        ("2026-12-01T09:00:00Z", 1_796_115_600_000u64),
        ("2026-12-01T17:00:00+08:00", 1_796_115_600_000u64),
        ("2026-12-01T09:00Z", 1_796_115_600_000u64),
    ] {
        let r = n.cli(&["todo", "add", due, "--due", due, "--json"]).await;
        assert_eq!(r.code, 0, "{due}: {}", r.stderr);
        assert_eq!(r.json()["due_ms"], ms, "{due}");
    }
    // 没有匹配
    let r = n.cli(&["todo", "done", "zzzz"]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not_found"), "{}", r.stderr);
    // 前缀不唯一：17 个 id 的首字符只有 16 种取值，必有两项共享
    for i in 0..14 {
        n.add(&format!("item {i}")).await;
    }
    let all = n.todos(true).await;
    assert_eq!(all.len(), 17);
    let first_chars: Vec<char> = all.iter().map(|t| t["id"].as_str().unwrap().chars().next().unwrap()).collect();
    let dup = first_chars.iter().find(|c| first_chars.iter().filter(|x| x == c).count() > 1).unwrap().to_string();
    let r = n.cli(&["todo", "done", &dup]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("matches") && r.stderr.contains("longer prefix"), "{}", r.stderr);
    assert!(n.todos(false).await.len() == 17, "an ambiguous prefix changes nothing");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_pages_until_it_has_everything() {
    let n = node().await;
    // 把每项的正文填满，让一页被 1 MiB 的字节预算截断（约 60 多项），再多加一些
    let body = "x".repeat(16 * 1024);
    for i in 0..75 {
        let r = n.cli(&["todo", "add", &format!("big {i}"), "--body", &body]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    let all = n.todos(false).await;
    assert_eq!(all.len(), 75, "ls follows has_more instead of stopping at the first page");
    let mut ids: Vec<&str> = all.iter().map(|t| t["id"].as_str().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 75, "no duplicates");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inside_an_agent_session_add_is_marked_and_the_rest_is_refused() {
    let n = node().await;
    let created = n.cli(&["new", "--agent", "mock", "--no-wait", "--json", "hello"]).await;
    assert_eq!(created.code, 0, "{}", created.stderr);
    let sid = created.json()["id"].as_str().unwrap().to_string();
    let agent_env = [("ANYCON_AGENT", "1"), ("ANYCON_SESSION_ID", sid.as_str())];

    // add：带 agent 标记和来源会话
    let r = n.cli_env(&["todo", "add", "from the agent", "--json"], &agent_env).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let t = r.json();
    assert_eq!(t["source"]["kind"], "agent");
    assert_eq!(t["source"]["session_id"], sid.as_str());
    assert_eq!(t["unreviewed"], true);
    let id = t["id"].as_str().unwrap().to_string();
    // 没有 ANYCON_SESSION_ID，或者会话不存在：照样是 agent 项，只是没有来源会话
    let r = n.cli_env(&["todo", "add", "no session", "--json"], &[("ANYCON_AGENT", "1")]).await;
    assert_eq!(
        (r.json()["source"]["kind"].as_str(), r.json()["source"]["session_id"].is_null()),
        (Some("agent"), true)
    );
    let r = n
        .cli_env(
            &["todo", "add", "gone", "--json"],
            &[("ANYCON_AGENT", "1"), ("ANYCON_SESSION_ID", "sess_doesnotexist0000000000")],
        )
        .await;
    assert_eq!(
        (r.json()["source"]["kind"].as_str(), r.json()["source"]["session_id"].is_null()),
        (Some("agent"), true)
    );

    // ls 照常（表格上标出 agent 项）
    let r = n.cli_env(&["todo", "ls"], &agent_env).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("from the agent") && r.stdout.contains("agent (unreviewed)"), "{}", r.stdout);

    // done / drop / reopen / rm：拒绝，退出码 5，说明原因，什么都不改
    for sub in ["done", "drop", "reopen", "rm"] {
        let r = n.cli_env(&["todo", sub, &id[..8]], &agent_env).await;
        assert_eq!(r.code, 5, "{sub}: {}", r.stderr);
        assert!(
            r.stderr.contains("not available inside an anycon agent session") && r.stderr.contains("ANYCON_AGENT"),
            "{sub}: {}",
            r.stderr
        );
    }
    let after = n.todos(true).await;
    assert_eq!(after.len(), 3);
    let kept = after.iter().find(|t| t["id"] == id.as_str()).unwrap();
    assert_eq!((kept["state"].as_str(), kept["unreviewed"].as_bool()), (Some("open"), Some(true)));

    // 空值和 "0" 不算「在 agent 里」（和 `anycon mcp` 是同一个判断）：和人敲的一样
    for off in ["", "0"] {
        let r = n.cli_env(&["todo", "add", "typed by a person", "--json"], &[("ANYCON_AGENT", off)]).await;
        assert_eq!(r.json()["source"]["kind"], "local", "ANYCON_AGENT={off:?}");
        assert_eq!(n.cli_env(&["todo", "done", &id[..8]], &[("ANYCON_AGENT", off)]).await.code, 0);
        assert_eq!(n.cli_env(&["todo", "reopen", &id[..8]], &[("ANYCON_AGENT", off)]).await.code, 0);
    }
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cli_help_documents_the_subcommands_in_english() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let r = anycon(&home, &["todo", "--help"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    for want in ["add", "ls", "done", "drop", "reopen", "rm", "run", "history"] {
        assert!(r.stdout.contains(want), "{want}: {}", r.stdout);
    }
    let r = anycon(&home, &["todo", "add", "--help"], None, &[]).await;
    assert!(r.stdout.contains("--due") && r.stdout.contains("RFC 3339") && r.stdout.contains("--body"), "{}", r.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_state_change_that_changes_nothing_says_so() {
    let n = node().await;
    let t = n.add("steady").await;
    let id = t["id"].as_str().unwrap().to_string();
    // 已经是 open：reopen 什么都没变
    let r = n.cli(&["todo", "reopen", &id[..8]]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("nothing changed") && r.stdout.contains("already open"), "{}", r.stdout);
    // done 一次：真的改了；再 done：没变；--json 照样给出这一项
    let r = n.cli(&["todo", "done", &id[..8]]).await;
    assert!(r.stdout.starts_with("done "), "{}", r.stdout);
    let r = n.cli(&["todo", "done", &id[..8]]).await;
    assert!(r.stdout.starts_with("nothing changed") && r.stdout.contains("already done"), "{}", r.stdout);
    let r = n.cli(&["todo", "done", &id, "--json"]).await;
    assert_eq!(r.json()["state"], "done");
    // 前缀先在没关闭的项里找；找不到再找已关闭的（done 之后 drop 用前缀也行）
    let r = n.cli(&["todo", "drop", &id[..8]]).await;
    assert!(r.stdout.starts_with("dropped "), "{}", r.stdout);
    // 完整 UUID 不用列清单，已关闭的项直接 reopen；没列过就不知道原来的状态，只报告现在的状态
    let r = n.cli(&["todo", "reopen", &id]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(" is open") && !r.stdout.contains("nothing changed"), "{}", r.stdout);
    // 再来一次（daemon 当空操作）也是同一句话，不猜它变没变
    let r = n.cli(&["todo", "reopen", &id]).await;
    assert!(r.stdout.contains(" is open") && !r.stdout.contains("nothing changed"), "{}", r.stdout);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefix_matching_an_open_and_a_closed_item_picks_by_what_the_command_can_act_on() {
    let n = node().await;
    // 17 个 id 的首字符只有 16 种取值，必有两项共享；把共享同一个首字符的项收拾成「一个没关闭 + 一个已关闭」
    for i in 0..17 {
        n.add(&format!("item {i}")).await;
    }
    let all = n.todos(false).await;
    let ids: Vec<String> = all.iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
    let c = ids
        .iter()
        .map(|i| i.chars().next().unwrap())
        .find(|c| ids.iter().filter(|i| i.starts_with(*c)).count() > 1)
        .unwrap();
    let group: Vec<&String> = ids.iter().filter(|i| i.starts_with(c)).collect();
    let (open_id, closed_id) = (group[0].clone(), group[1].clone());
    for extra in &group[2..] {
        assert_eq!(n.cli(&["todo", "rm", extra]).await.code, 0);
    }
    assert_eq!(n.cli(&["todo", "done", &closed_id]).await.code, 0);
    let prefix = c.to_string();

    // reopen：用已关闭的那一项，不报歧义
    let r = n.cli(&["todo", "reopen", &prefix, "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["id"], closed_id.as_str());
    assert_eq!(r.json()["state"], "open");

    // done：用没关闭的那一项
    assert_eq!(n.cli(&["todo", "done", &closed_id]).await.code, 0);
    let r = n.cli(&["todo", "done", &prefix, "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["id"], open_id.as_str());
    assert_eq!(r.json()["state"], "done");

    // 两项都已关闭时，done 用前缀就有歧义；reopen 同理
    let r = n.cli(&["todo", "reopen", &prefix]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_hands_an_item_to_an_agent_and_history_shows_what_happened() {
    let n = node().await;
    std::fs::create_dir_all(n.ws.join("proj")).unwrap();
    let t = n.add("Tidy the changelog").await;
    let id = t["id"].as_str().unwrap().to_string();

    // 没有项目：说清楚缺什么
    let r = n.cli(&["todo", "run", &id[..8], "--agent", "mock", "--allow-push"]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("project"), "{}", r.stderr);
    // 不存在的项目 → not_found
    let r = n.cli(&["todo", "run", &id[..8], "--agent", "mock", "--project", "nope", "--allow-push"]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not_found"), "{}", r.stderr);
    // 「不 push、不开 PR」的拒绝（agent 没声明 `no_push`）在 daemon 的测试里用没有这个选项的假 agent 覆盖；mock 声明了它

    // --allow-push --local：派发成功，项目写回这一项
    let r = n
        .cli(&["todo", "run", &id[..8], "--agent", "mock", "--project", "proj", "--allow-push", "--local", "--json"])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!(j["todo"]["state"], "open");
    assert!(j["todo"]["activity"].is_string());
    assert_eq!(j["todo"]["project"], "proj");
    assert_eq!(j["session"]["todo_id"], id.as_str());
    let sid = j["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(j["todo"]["sessions"][0], sid.as_str());
    // 文字输出
    let r = n.cli(&["todo", "run", &id[..8], "--agent", "mock", "--allow-push", "--local"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.starts_with("dispatched ")
            && r.stdout.contains("project directory")
            && r.stdout.contains("may push and open"),
        "{}",
        r.stdout
    );

    // history：dispatched 和这一轮的 result 最终都在（--json 是原始事件，newest first）
    let mut events = vec![];
    for _ in 0..200 {
        let r = n.cli(&["todo", "history", &id[..8], "--json"]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        events = r.json()["events"].as_array().unwrap().clone();
        if events.iter().filter(|e| e["kind"] == "result").count() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "dispatched").count(), 2, "{kinds:?}");
    assert!(kinds.iter().filter(|k| **k == "result").count() >= 2, "{kinds:?}");
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] > w[1]), "newest first: {seqs:?}");
    let text = n.cli(&["todo", "history", &id[..8]]).await;
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(
        text.stdout.contains("dispatched to mock") && text.stdout.contains("report from session"),
        "{}",
        text.stdout
    );
    // `ls` 的表头有 ACTIVITY 列，派发过的项在这一列里有值，列对得上
    let ls = n.cli(&["todo", "ls"]).await;
    assert_eq!(ls.code, 0, "{}", ls.stderr);
    let mut lines = ls.stdout.lines();
    let header = lines.next().unwrap();
    let col = header.find("ACTIVITY").expect("an ACTIVITY column in the header");
    assert!(header.find("STATE").unwrap() < col && col < header.find("DUE").unwrap(), "{header}");
    let row = lines.find(|l| l.starts_with(&id[..8])).expect("the dispatched item is listed");
    let cell = &row[col..];
    assert!(["working", "waiting", "review"].iter().any(|w| cell.starts_with(w)), "{header}\n{row}");
    // 没有发生过什么的项
    let other = n.add("Untouched").await;
    let r = n.cli(&["todo", "history", other["id"].as_str().unwrap()]).await;
    assert!(r.stdout.contains("nothing has happened"), "{}", r.stdout);
    // 不存在的前缀
    assert_eq!(n.cli(&["todo", "history", "zzzz"]).await.code, 1);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_is_refused_inside_an_agent_session_but_history_is_not() {
    let n = node().await;
    std::fs::create_dir_all(n.ws.join("proj")).unwrap();
    let t = n.add("Guarded").await;
    let id = t["id"].as_str().unwrap().to_string();
    let agent_env = [("ANYCON_AGENT", "1")];
    let r =
        n.cli_env(&["todo", "run", &id[..8], "--agent", "mock", "--project", "proj", "--allow-push"], &agent_env).await;
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert!(
        r.stderr.contains("not available inside an anycon agent session") && r.stderr.contains("ANYCON_AGENT"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("handing a to-do to an agent") && !r.stderr.contains("closing, reopening"),
        "{}",
        r.stderr
    );
    let after = n.todos(false).await;
    assert_eq!((after[0]["state"].as_str(), after[0]["sessions"].as_array().map(Vec::len)), (Some("open"), Some(0)));
    // history 只读，照常
    assert_eq!(n.cli_env(&["todo", "history", &id[..8]], &agent_env).await.code, 0);
    // 空值和 "0" 不算在 agent 里
    for off in ["", "0"] {
        let r = n
            .cli_env(
                &["todo", "run", &id[..8], "--agent", "mock", "--project", "proj", "--allow-push", "--local"],
                &[("ANYCON_AGENT", off)],
            )
            .await;
        assert_eq!(r.code, 0, "ANYCON_AGENT={off:?}: {}", r.stderr);
    }
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_dry_run_prints_the_first_message_and_sends_nothing() {
    let n = node().await;
    std::fs::create_dir_all(n.ws.join("proj")).unwrap();
    let r = n
        .cli(&["todo", "add", "Tidy the changelog", "--body", "Keep a Changelog format", "--project", "proj", "--json"])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let id = r.json()["id"].as_str().unwrap().to_string();
    // 缺省有「不 push」那一行；--allow-push 没有。不需要选 agent、不需要 agent 声明了什么
    let r = n.cli(&["todo", "run", &id[..8], "--dry-run"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        "Tidy the changelog\n\nKeep a Changelog format\n\nWhen you are done, end your final message with a short report: what you changed, how you verified it (commands you ran and their results), and anything left undone or uncertain.\n\nLeave your changes on the branch: do not push and do not open a pull request.\n"
    );
    let r = n.cli(&["todo", "run", &id[..8], "--dry-run", "--allow-push", "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.json()["first_message"],
        "Tidy the changelog\n\nKeep a Changelog format\n\nWhen you are done, end your final message with a short report: what you changed, how you verified it (commands you ran and their results), and anything left undone or uncertain."
    );
    // 什么都没发生：没有会话、「经过」是空的、这一项没有关联
    let t = &n.todos(false).await[0];
    assert!(t["sessions"].as_array().is_none_or(|s| s.is_empty()), "{t}");
    let h = n.cli(&["todo", "history", &id[..8], "--json"]).await;
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.json()["events"].as_array().unwrap().is_empty(), "{}", h.stdout);
    // 没有的项：和别处一样的错误
    let r = n.cli(&["todo", "run", "ffffffff", "--dry-run"]).await;
    assert_ne!(r.code, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeat_is_set_shown_cleared_and_refused_inside_an_agent_session() {
    let n = node().await;
    // 规则写错：退出码 2，什么都不建
    let r = n.cli(&["todo", "add", "Bad", "--repeat", "hourly@09:00"]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("--repeat") && r.stderr.contains("weekly:mon@09:00"), "{}", r.stderr);
    assert!(n.todos(true).await.is_empty());

    let r = n.cli(&["todo", "add", "Weekly review", "--repeat", "weekly:fri@18:30", "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let t = r.json();
    assert_eq!(t["repeat"]["kind"], "weekly");
    assert_eq!((t["repeat"]["at"].as_str(), t["repeat"]["weekday"].as_u64()), (Some("18:30"), Some(5)));
    assert!(t["repeat"]["next_ms"].as_u64().unwrap() > anycon_daemon::util::now_ms());
    let id = t["id"].as_str().unwrap().to_string();

    // ls 标出规则和下一次时间
    let r = n.cli(&["todo", "ls"]).await;
    assert!(r.stdout.contains("REPEAT") && r.stdout.contains("weekly:fri@18:30 next "), "{}", r.stdout);

    // edit 改规则、再关掉
    let r = n.cli(&["todo", "edit", &id[..8], "--repeat", "daily@07:15"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("daily@07:15"), "{}", r.stdout);
    assert_eq!(n.cli(&["todo", "edit", &id[..8]]).await.code, 2, "one of --repeat / --no-repeat is required");

    // 模板可以交给 Bot（每一次复制出来的都交给它），但不能派发
    let r = n.cli(&["todo", "assign", &id[..8], "twin"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = n.cli(&["todo", "run", &id[..8], "--agent", "mock", "--project", "p", "--allow-push"]).await;
    assert!(r.code != 0 && r.stderr.contains("template"), "{}", r.stderr);

    // agent 环境里不能设置或修改规则
    let agent_env = [("ANYCON_AGENT", "1")];
    for args in [
        vec!["todo", "add", "Sneaky", "--repeat", "daily@09:00"],
        vec!["todo", "edit", &id[..8], "--repeat", "daily@09:00"],
        vec!["todo", "edit", &id[..8], "--no-repeat"],
    ] {
        let r = n.cli_env(&args, &agent_env).await;
        assert_eq!(r.code, 5, "{args:?}: {}", r.stderr);
        assert!(r.stderr.contains("not available inside an anycon agent session"), "{}", r.stderr);
    }
    assert_eq!(n.todos(true).await.len(), 1);

    let r = n.cli(&["todo", "edit", &id[..8], "--no-repeat", "--json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.json().get("repeat").is_none());
    n.stop().await;
}
