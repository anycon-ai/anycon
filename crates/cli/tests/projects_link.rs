//! `anycon projects link / unlink / browse` 对本机 daemon 的端到端（ADR-33）。
//! 全部在临时目录里：临时 `ANYCON_HOME`、临时 workspace、`workspace.link_roots` 指向临时目录。

mod common;

use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_browse_list_and_unlink_a_local_directory() {
    let d = tempfile::tempdir().unwrap();
    let base = anycon_daemon::sys::canonicalize(d.path()).unwrap();
    let home = base.join("home");
    let machine = base.join("machine");
    std::fs::create_dir_all(machine.join("alpha/.git")).unwrap();
    std::fs::create_dir_all(machine.join("beta")).unwrap();
    std::fs::create_dir_all(machine.join(".dot")).unwrap();
    std::fs::write(machine.join("alpha/keep.txt"), "do not delete").unwrap();
    init(&home, &base.join("ws"), "box", None).await;
    // 内置的真实 agent（claude-code、codex）没有配置段也会探测：都关掉，测试不碰真 CLI
    for id in ["claude-code", "codex"] {
        let r = anycon(&home, &["config", "set", &format!("agents.{id}.enabled"), "false"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    macro_rules! a {
        ($($x:expr),* $(,)?) => {
            anycon(&home, &[$($x),*], None, &[])
        };
    }

    // 默认 link_roots 是 `["~"]`（真实 HOME，这里不碰）；用数组值收紧到临时目录，daemon 在跑 = 立即生效
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    let r = a!("config", "set", "workspace.link_roots", &roots).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("saved and applied"), "link_roots is a hot key: {}", r.stdout);
    assert_eq!(a!("config", "get", "workspace.link_roots", "--json").await.json()["value"], json!([machine]));

    // browse：缺省列第一个 root；--json 原样；--all 带隐藏目录
    let r = a!("projects", "browse").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(&machine.to_string_lossy().to_string()), "{}", r.stdout);
    assert!(r.stdout.contains("alpha") && r.stdout.contains("beta") && !r.stdout.contains(".dot"), "{}", r.stdout);
    let j = a!("projects", "browse", "--json").await.json();
    assert_eq!(j["path"], json!(machine));
    assert_eq!(
        j["entries"].as_array().unwrap().iter().map(|e| e["name"].clone()).collect::<Vec<_>>(),
        vec![json!("alpha"), json!("beta")]
    );
    assert_eq!(j["entries"][0]["git"], json!(true));
    let all = a!("projects", "browse", "--all", "--json").await.json();
    assert!(all["entries"].as_array().unwrap().iter().any(|e| e["name"] == ".dot"));
    // 机器上恶意命名的目录（名字里带 ESC / 回车）不能把转义序列写进终端
    let evil = machine.join("evil\x1b[2K\x1b]52;c;ZXZpbA==\x07dir");
    std::fs::create_dir_all(&evil).unwrap();
    let r = a!("projects", "browse").await;
    assert!(!r.stdout.contains('\x1b') && !r.stdout.contains('\x07'), "{:?}", r.stdout);
    assert!(r.stdout.contains("evil\\x1b[2K\\x1b]52;c;ZXZpbA==\\x07dir"), "{:?}", r.stdout);
    // 路径里有控制字符的目录不能关联（invalid）；根目录里手工建出来的、名字带 ESC 的文件夹照样列出，但名字是转义后的
    let r = a!("projects", "link", &evil.to_string_lossy(), "--name", "evil-one").await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    std::fs::remove_dir_all(&evil).unwrap();
    std::fs::create_dir_all(base.join("ws").join("folder\x1b[2K\x1b]52;c;ZXZpbA==\x07x")).unwrap();
    let r = a!("projects").await;
    assert!(!r.stdout.contains('\x1b') && !r.stdout.contains('\x07'), "{:?}", r.stdout);
    assert!(r.stdout.contains("folder\\x1b[2K\\x1b]52;c;ZXZpbA==\\x07x"), "{:?}", r.stdout);
    std::fs::remove_dir_all(base.join("ws").join("folder\x1b[2K\x1b]52;c;ZXZpbA==\x07x")).unwrap();
    // 隐藏目录默认不能关联；浏览里标出原因
    let r = a!("projects", "link", &machine.join(".dot").to_string_lossy(), "--name", "dot").await;
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert!(r.stderr.contains("hidden") && r.stderr.contains("link_roots"), "{}", r.stderr);
    let r = a!("projects", "browse", "--all").await;
    assert!(r.stdout.contains(".dot") && r.stdout.contains("not linkable: hidden directory"), "{}", r.stdout);
    let j = a!("projects", "browse", "--all", "--json").await.json();
    let dot = j["entries"].as_array().unwrap().iter().find(|e| e["name"] == ".dot").unwrap().clone();
    assert_eq!((dot["linkable"].clone(), dot["blocked"].clone()), (json!(false), json!("hidden")));
    // 越界：5（被拒绝）
    let r = a!("projects", "browse", "/").await;
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert!(r.stderr.contains("error: forbidden:"), "{}", r.stderr);

    // link：相对路径按 CLI 当前目录解析成绝对路径再发（daemon 的 cwd 不是这里）
    let r = anycon_in(&home, Some(&machine), &["projects", "link", "alpha"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("linked alpha ->") && r.stdout.contains(&machine.join("alpha").to_string_lossy().to_string()),
        "{}",
        r.stdout
    );
    let r =
        anycon_in(&home, Some(&machine), &["projects", "link", "./beta", "--name", "bee", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!((j["name"].as_str(), j["kind"].as_str()), (Some("bee"), Some("linked")));
    assert_eq!(j["path"], json!(machine.join("beta")));
    // 重复 / 重名 / 越界
    let r = anycon_in(&home, Some(&machine), &["projects", "link", "alpha", "--name", "other"], None, &[]).await;
    assert_eq!((r.code, r.stderr.contains("error: exists:")), (1, true), "{}", r.stderr);
    let r = a!("projects", "link", &base.join("ws").to_string_lossy(), "--name", "w").await;
    assert_eq!(r.code, 5, "outside link_roots: {}", r.stderr);
    // browse 标出已经是项目的
    let j = a!("projects", "browse", "--json").await.json();
    assert_eq!(j["entries"][0]["project"], json!("alpha"));
    assert_eq!(j["entries"][0]["linkable"], json!(false));
    let r = a!("projects", "browse").await;
    assert!(r.stdout.contains("project alpha"), "{}", r.stdout);

    // projects 列表标 linked 和路径
    let r = a!("projects").await;
    assert!(r.stdout.contains("LOCATION") && r.stdout.contains("linked:"), "{}", r.stdout);
    assert!(r.stdout.contains(&machine.join("alpha").to_string_lossy().to_string()), "{}", r.stdout);
    let j = a!("projects", "--json").await.json();
    let alpha = j["projects"].as_array().unwrap().iter().find(|p| p["name"] == "alpha").unwrap().clone();
    assert_eq!(alpha["kind"], json!("linked"));
    assert!(alpha.get("missing").is_none(), "false is omitted: {alpha}");

    // 目录不在了 -> missing 标出来
    std::fs::remove_dir_all(machine.join("beta")).unwrap();
    let r = a!("projects").await;
    let line = r.stdout.lines().find(|l| l.starts_with("bee")).unwrap_or_default().to_string();
    assert!(line.contains("(missing)"), "{}", r.stdout);
    let j = a!("projects", "--json").await.json();
    let bee = j["projects"].as_array().unwrap().iter().find(|p| p["name"] == "bee").unwrap().clone();
    assert_eq!(bee["missing"], json!(true));
    // 关联项目不能进回收站（用 unlink）；文件原样
    let r = a!("projects", "trash", "alpha").await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("unlink"), "{}", r.stderr);
    // unlink 只从列表去掉，不删文件
    let r = a!("projects", "unlink", "bee").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("unlinked bee"), "{}", r.stdout);
    let r = a!("projects", "unlink", "alpha", "--json").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json(), json!({}));
    assert_eq!(std::fs::read_to_string(machine.join("alpha/keep.txt")).unwrap(), "do not delete");
    // 不是关联项目：2（invalid）
    let r = a!("projects", "unlink", "alpha").await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stdout.is_empty() || !r.stdout.contains("unlinked"));
    let list = a!("projects", "--json").await.json();
    assert!(list["projects"].as_array().unwrap().is_empty(), "{list}");

    daemon.stop().await;
}

/// `anycon config set workspace.link_roots …` 之后打印 daemon 的判定：被忽略的项（经过符号链接）和目录不存在的项，各带原因。
/// 以前只打印「saved and applied」，目录明明在、却被 daemon 忽略的用户看不到任何提示。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_set_link_roots_prints_which_entries_the_daemon_ignores_and_why() {
    let d = tempfile::tempdir().unwrap();
    let base = anycon_daemon::sys::canonicalize(d.path()).unwrap();
    let home = base.join("home");
    let machine = base.join("machine");
    std::fs::create_dir_all(machine.join("real-code")).unwrap();
    std::fs::create_dir_all(machine.join("fine")).unwrap();
    anycon_daemon::testutil::symlink_dir(&machine.join("real-code"), &machine.join("code")).unwrap();
    init(&home, &base.join("ws"), "box", None).await;
    for id in ["claude-code", "codex"] {
        let r = anycon(&home, &["config", "set", &format!("agents.{id}.enabled"), "false"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    let code_root = machine.join("code").to_string_lossy().into_owned();
    let nope_root = machine.join("nope").to_string_lossy().into_owned();
    let fine_root = machine.join("fine").to_string_lossy().into_owned();
    let roots = serde_json::to_string(&[&code_root, &nope_root, &fine_root]).unwrap();
    let r = anycon(&home, &["config", "set", "workspace.link_roots", &roots], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("saved and applied"), "{}", r.stdout);
    let warn_code = r.stdout.lines().find(|l| l.contains(&code_root)).unwrap_or_default().to_string();
    assert!(
        warn_code.starts_with("warning:") && warn_code.contains("ignored") && warn_code.contains("symbolic link"),
        "{}",
        r.stdout
    );
    assert!(
        warn_code.contains(&machine.join("real-code").to_string_lossy().to_string())
            && warn_code.contains("write the real path"),
        "the admin sees where the link points: {warn_code}"
    );
    let warn_nope = r.stdout.lines().find(|l| l.contains(&nope_root)).unwrap_or_default().to_string();
    assert!(warn_nope.contains("not usable") && warn_nope.contains("does not exist"), "{}", r.stdout);
    assert!(!r.stdout.contains(&fine_root), "the usable entry is not mentioned: {}", r.stdout);
    // --json 带结构化的列表
    let r = anycon(&home, &["config", "set", "workspace.link_roots", &roots, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    let un = j["link_roots_unusable"].as_array().unwrap();
    assert_eq!(un.len(), 2, "{j}");
    assert_eq!((un[0]["entry"].as_str(), un[0]["missing"].as_bool()), (Some(code_root.as_str()), Some(false)));
    assert_eq!((un[1]["entry"].as_str(), un[1]["missing"].as_bool()), (Some(nope_root.as_str()), Some(true)));
    // 只有可用的项：没有警告
    let ok = serde_json::to_string(&[&fine_root]).unwrap();
    let r = anycon(&home, &["config", "set", "workspace.link_roots", &ok], None, &[]).await;
    assert!(r.stdout.contains("saved and applied") && !r.stdout.contains("warning"), "{}", r.stdout);
    // 别的键不打印这些
    let r = anycon(&home, &["config", "set", "sessions.max_concurrent", "4"], None, &[]).await;
    assert!(!r.stdout.contains("warning"), "{}", r.stdout);
    daemon.stop().await;
}
