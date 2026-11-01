//! 多个 workspace 根（ADR-45）：`anycon projects create / clone --root`、`projects` 的 LOCATION 列、`doctor` 每个根一项，
//! 对本机 daemon 的端到端。全部在临时目录里：临时 `ANYCON_HOME`、临时主根和额外根。

mod common;

use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projects_can_be_created_in_an_extra_root_and_the_listing_and_doctor_name_it() {
    let d = tempfile::tempdir().unwrap();
    let base = anycon_daemon::sys::canonicalize(d.path()).unwrap();
    let home = base.join("home");
    let (ws, ext, gone) = (base.join("ws"), base.join("ext"), base.join("not-mounted"));
    std::fs::create_dir_all(&ext).unwrap();
    init(&home, &ws, "box", None).await;
    for id in ["claude-code", "codex"] {
        let r = anycon(&home, &["config", "set", &format!("agents.{id}.enabled"), "false"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    // 额外根只在启动时读：daemon 起来之前写
    let roots = serde_json::to_string(&[&ext, &gone]).unwrap();
    let r = anycon(&home, &["config", "set", "workspace.extra_roots", &roots], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    // 没插上的盘：提示它会被跳过
    assert!(
        r.stdout.contains("saved") && r.stdout.contains("is not usable") && r.stdout.contains("skip"),
        "{}",
        r.stdout
    );
    // 配置错误：提示 daemon 会拒绝启动（配置照样写进去了，和别的非法组合的处理方式一样由启动时拦下）
    let nested = serde_json::to_string(&[&ext, &ext.join("inner")]).unwrap();
    let r = anycon(&home, &["config", "set", "workspace.extra_roots", &nested], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("refuse to start") && r.stdout.contains("is inside"), "{}", r.stdout);
    let r = anycon(&home, &["config", "set", "workspace.extra_roots", &roots, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["workspace_roots_warnings"].as_array().unwrap().len(), 1, "{}", r.stdout);
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    macro_rules! a {
        ($($x:expr),* $(,)?) => {
            anycon(&home, &[$($x),*], None, &[])
        };
    }

    // daemon 在跑时改，提示要重启才生效
    let r = a!("config", "set", "workspace.extra_roots", &roots).await;
    assert!(r.stdout.contains("needs a daemon restart"), "{}", r.stdout);

    // 缺省建在主根，列表的 LOCATION 列是空的
    let r = a!("projects", "create", "near").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(&ws.join("near").to_string_lossy().to_string()), "{}", r.stdout);
    // --root 指定额外根（路径和 daemon 的配置写法一致）
    let r = a!("projects", "create", "far", "--git", "--root", &ext.to_string_lossy()).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(&ext.join("far").to_string_lossy().to_string()), "{}", r.stdout);
    assert!(ext.join("far/.git").exists() && !ws.join("far").exists());
    // 本机 daemon：相对路径按 CLI 的当前目录解析
    let r = anycon_in(&home, Some(&base), &["projects", "create", "rel", "--root", "ext"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(ext.join("rel").is_dir());

    // 列表：只有不在主根里的项目标出所在的根
    let r = a!("projects").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let line = |name: &str| {
        r.stdout.lines().find(|l| l.starts_with(name)).unwrap_or_else(|| panic!("{}", r.stdout)).to_string()
    };
    assert!(line("far").contains(&format!("root: {}", ext.display())), "{}", r.stdout);
    assert!(line("rel").contains(&format!("root: {}", ext.display())), "{}", r.stdout);
    assert!(!line("near").contains("root:"), "{}", r.stdout);
    let j = a!("projects", "--json").await.json();
    assert_eq!(j["root"], json!(ws));
    let root_of =
        |name: &str| j["projects"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()["root"].clone();
    assert_eq!((root_of("near"), root_of("far")), (json!(ws), json!(ext)));

    // 不是任何一个根：2（invalid）；列出但没插上的盘：1（not_found）；名字全局唯一：1（exists）
    let r = a!("projects", "create", "x", "--root", &base.join("somewhere").to_string_lossy()).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("error: invalid:"), "{}", r.stderr);
    let r = a!("projects", "create", "x", "--root", &gone.to_string_lossy()).await;
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("not available"), "{}", r.stderr);
    assert!(!gone.exists());
    let r = a!("projects", "create", "near", "--root", &ext.to_string_lossy()).await;
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("exists"), "{}", r.stderr);
    // clone 同样带 --root（先校验根，再克隆）
    let r =
        a!("projects", "clone", "https://example.invalid/x/y.git", "--root", &base.join("somewhere").to_string_lossy())
            .await;
    assert_eq!(r.code, 2, "{}", r.stderr);

    // doctor 每个根一项，没插上的盘失败并提示重启
    let j = a!("doctor", "--json").await.json();
    let checks = j["checks"].as_array().unwrap();
    let check =
        |id: &str| checks.iter().find(|c| c["id"] == id).unwrap_or_else(|| panic!("no check {id}: {checks:?}")).clone();
    assert_eq!(check("workspace")["ok"], json!(true));
    assert_eq!(check(&format!("workspace:{}", ext.display()))["ok"], json!(true));
    let missing = check(&format!("workspace:{}", gone.display()));
    assert_eq!(missing["ok"], json!(false));
    assert!(missing["fix"].as_str().unwrap().contains("restart"), "{missing}");
    let r = a!("doctor").await;
    assert!(r.stdout.contains(&format!("workspace:{}", gone.display())), "{}", r.stdout);

    daemon.stop().await;
}
