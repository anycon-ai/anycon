//! 被 `ANYCON_*` 环境变量覆盖的键：`node.get_config` 标成只读，`node.set_config` 拒绝（ADR-43）。
//!
//! 单独一个测试文件（= 单独一个进程）：要改进程环境变量，不能和别的测试并行；所以只有一个测试函数，分阶段改环境。

use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use serde_json::{json, Value};

fn item<'a>(r: &'a NodeConfigResult, key: &str) -> &'a ConfigItem {
    r.items.iter().find(|i| i.key == key).unwrap()
}

fn changes(pairs: &[(&str, Value)]) -> Value {
    json!({ "changes": pairs.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>() })
}

#[tokio::test]
async fn keys_set_by_the_environment_are_read_only_remotely() {
    std::env::set_var("ANYCON_SESSIONS__MAX_CONCURRENT", "2");
    std::env::set_var("ANYCON_PREVIEW__ENABLED", "true");
    // `ANYCON_LOG` 接管日志过滤：`log.level` 改了也不生效
    std::env::set_var("ANYCON_LOG", "debug");

    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    let r: NodeConfigResult = c.call(&td, methods::NODE_GET_CONFIG, json!({})).await.unwrap();
    for key in ["sessions.max_concurrent", "preview.enabled", "log.level"] {
        let i = item(&r, key);
        assert!(i.env_override, "{key}");
        assert_eq!(i.editable, ConfigEditable::No, "{key}");
    }
    // 环境变量盖过文件：给出的是生效值
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(2));
    // 没被覆盖的键照常
    for key in
        ["sessions.idle_timeout", "permissions.allow_dangerous_modes", "notify.turn_finished", "worktree.branch_prefix"]
    {
        assert!(!item(&r, key).env_override, "{key}");
        assert_ne!(item(&r, key).editable, ConfigEditable::No, "{key}");
    }

    // 改被覆盖的键：forbidden，同一请求里别的合法改动也不写
    let before = std::fs::read_to_string(td.paths.config_toml()).unwrap();
    for (key, value) in
        [("sessions.max_concurrent", json!(5)), ("preview.enabled", json!(false)), ("log.level", json!("warn"))]
    {
        let e = c
            .call::<Value>(
                &td,
                methods::NODE_SET_CONFIG,
                changes(&[("notify.turn_finished", json!("never")), (key, value)]),
            )
            .await
            .unwrap_err();
        let d = e.data.unwrap();
        assert_eq!(
            (d.code, d.field.as_deref(), d.reason.as_deref()),
            (ErrorCode::Forbidden, Some(key), Some("env_override"))
        );
    }
    assert_eq!(std::fs::read_to_string(td.paths.config_toml()).unwrap(), before);
    assert_eq!(td.ctx.config.get().notify.turn_finished, "when_unwatched");

    // 别的键不受影响
    let r: NodeConfigResult =
        c.call(&td, methods::NODE_SET_CONFIG, changes(&[("notify.turn_finished", json!("never"))])).await.unwrap();
    assert_eq!(item(&r, "notify.turn_finished").value, json!("never"));
    // 被覆盖的键的生效值没有被改动
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(2));

    // 环境变量没了，键就又能改了（每次调用现查，不缓存）
    std::env::remove_var("ANYCON_SESSIONS__MAX_CONCURRENT");
    std::env::remove_var("ANYCON_PREVIEW__ENABLED");
    std::env::remove_var("ANYCON_LOG");
    let r: NodeConfigResult = c
        .call(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("sessions.max_concurrent", json!(5)), ("log.level", json!("warn"))]),
        )
        .await
        .unwrap();
    assert!(r.items.iter().all(|i| !i.env_override));
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(5));
    assert_eq!(item(&r, "log.level").value, json!("warn"));
    td.shutdown().await;
}
