//! `ANYCON_*` 环境变量覆盖：数组值（`workspace.link_roots`）怎么写。
//!
//! 单独一个测试文件（= 单独一个进程）：要改进程环境变量，不能和别的测试并行。

use anycon_daemon::config::{Config, Paths};

#[test]
fn link_roots_can_be_set_from_the_environment_as_an_array() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(d.path());
    assert_eq!(Config::load(&paths).unwrap().workspace.link_roots, vec!["~".to_string()]);

    // 绝对路径：Windows 上要带盘符
    let abs = |p: &str| if cfg!(windows) { format!("C:{p}") } else { p.to_string() };
    // 带引号的数组；环境变量盖过配置文件
    std::fs::write(paths.config_toml(), format!("[workspace]\nlink_roots = [\"{}\"]\n", abs("/from/file"))).unwrap();
    std::env::set_var("ANYCON_WORKSPACE__LINK_ROOTS", format!("[\"{}\", \"{}\"]", abs("/tmp/a"), abs("/tmp/b")));
    assert_eq!(Config::load(&paths).unwrap().workspace.link_roots, vec![abs("/tmp/a"), abs("/tmp/b")]);

    // 单个元素
    std::env::set_var("ANYCON_WORKSPACE__LINK_ROOTS", format!("[\"{}\"]", abs("/only/one")));
    assert_eq!(Config::load(&paths).unwrap().workspace.link_roots, vec![abs("/only/one")]);

    // 空数组 = 关闭
    std::env::set_var("ANYCON_WORKSPACE__LINK_ROOTS", "[]");
    assert!(Config::load(&paths).unwrap().workspace.link_roots.is_empty());

    // 值同样要过校验
    std::env::set_var("ANYCON_WORKSPACE__LINK_ROOTS", "[\"relative\"]");
    assert!(Config::load(&paths).is_err());

    // 内置 agent 也能用环境变量关掉（`codex` 没有短横线，键名可以直接写）
    std::env::remove_var("ANYCON_WORKSPACE__LINK_ROOTS");
    std::env::set_var("ANYCON_AGENTS__CODEX__ENABLED", "false");
    let cfg = Config::load(&paths).unwrap();
    assert!(!cfg.adapter_config("codex").unwrap().enabled);
    assert!(cfg.adapter_config("claude-code").unwrap().enabled);
}
