//! `node.get_config` / `node.set_config` / `node.logs` 端到端（ADR-43，docs/spec/daemon.md § 3 / § 13 / § 17）：
//! 白名单、只能收紧的开关、整体校验与原子性、写文件保留注释、热加载生效、审计、并发、日志尾部。
//! 环境变量覆盖要改进程环境，单独在 `remote_config_env.rs`（单独的进程）里测。

use std::time::Duration;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

fn item<'a>(r: &'a NodeConfigResult, key: &str) -> &'a ConfigItem {
    r.items.iter().find(|i| i.key == key).unwrap_or_else(|| panic!("{key} is not in the list"))
}

fn changes(pairs: &[(&str, Value)]) -> Value {
    json!({ "changes": pairs.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>() })
}

async fn audit(td: &TestDaemon) -> Vec<AuditEntry> {
    td.ctx.audit.query(None, None, 200).await.into_iter().filter(|e| e.action == "config.changed").collect()
}

fn file(td: &TestDaemon) -> String {
    std::fs::read_to_string(td.paths.config_toml()).unwrap()
}

fn code(e: &RpcError) -> (ErrorCode, Option<String>, Option<String>) {
    let d = e.data.clone().expect("error data");
    (d.code, d.field, d.reason)
}

#[tokio::test]
async fn get_config_lists_exactly_the_whitelist_with_the_effective_values() {
    let td = TestDaemon::start_with(&[("sessions.max_concurrent", "5"), ("log.level", "\"warn\"")], false).await;
    let mut c = td.client("web");
    let r: NodeConfigResult = c.call(&td, methods::NODE_GET_CONFIG, json!({})).await.unwrap();

    let keys: Vec<&str> = r.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "sessions.max_concurrent",
            "sessions.idle_timeout",
            "permissions.allow_dangerous_modes",
            "permissions.request_timeout",
            "worktree.default",
            "worktree.branch_prefix",
            "worktree.auto_archive_on_pr_close",
            "preview.enabled",
            "preview.ticket_ttl",
            "notify.turn_finished",
            "notify.session_idle_after",
            "log.level",
        ]
    );
    // 值是当前生效的（文件里改过的、没改的默认值）
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(5));
    assert_eq!(item(&r, "log.level").value, json!("warn"));
    assert_eq!(item(&r, "sessions.idle_timeout").value, json!("20m"));
    assert_eq!(item(&r, "worktree.default").value, json!("worktree"));
    assert_eq!(item(&r, "preview.enabled").value, json!(true));
    // 可改程度和约束
    let dangerous = item(&r, "permissions.allow_dangerous_modes");
    assert_eq!((dangerous.editable, dangerous.kind), (ConfigEditable::TightenOnly, ConfigValueKind::Bool));
    assert_eq!(item(&r, "preview.enabled").editable, ConfigEditable::TightenOnly);
    let mc = item(&r, "sessions.max_concurrent");
    assert_eq!((mc.editable, mc.kind, mc.min, mc.max), (ConfigEditable::Any, ConfigValueKind::Int, Some(1), Some(32)));
    assert_eq!(item(&r, "log.level").options, vec!["error", "warn", "info", "debug", "trace"]);
    assert!(r.items.iter().all(|i| !i.env_override));
    // 不暴露白名单之外的东西（值里也不能有：这些键不在列表里）
    let text = serde_json::to_string(&r).unwrap();
    for hidden in [
        "agents",
        "workspace",
        "hub.",
        "enroll",
        "allow_session_env",
        "allow_ports",
        "max_attachment_bytes",
        "history_retention",
    ] {
        assert!(!text.contains(hidden), "{hidden} leaked into node.get_config: {text}");
    }
    td.shutdown().await;
}

#[tokio::test]
async fn set_config_writes_the_file_applies_it_at_once_audits_and_returns_the_new_list() {
    let td = TestDaemon::start().await;
    let mut c = td.client("Pixel 9");
    let before = td.ctx.config.get();
    assert_eq!((before.sessions.max_concurrent, before.log.level.as_str()), (3, "info"));

    let r: NodeConfigResult = c
        .call(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[
                ("sessions.max_concurrent", json!(8)),
                ("sessions.idle_timeout", json!("45m")),
                ("log.level", json!("debug")),
                ("worktree.branch_prefix", json!("team")),
                ("worktree.auto_archive_on_pr_close", json!(true)),
                ("notify.turn_finished", json!("never")),
                // 和现在一样的值：空操作，不写、不审计
                ("worktree.default", json!("worktree")),
            ]),
        )
        .await
        .unwrap();

    // 返回的就是新的 node.get_config
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(8));
    assert_eq!(item(&r, "sessions.idle_timeout").value, json!("45m"));
    assert_eq!(item(&r, "log.level").value, json!("debug"));
    let again: NodeConfigResult = c.call(&td, methods::NODE_GET_CONFIG, json!({})).await.unwrap();
    assert_eq!(again, r);

    // 写进了 config.toml，字符串就是字符串（"123" 这样的前缀不会被当成整数）
    let text = file(&td);
    assert!(text.contains("max_concurrent = 8"), "{text}");
    assert!(text.contains("idle_timeout = \"45m\""), "{text}");
    assert!(text.contains("branch_prefix = \"team\""), "{text}");

    // 热加载：不用等 500 ms 的文件轮询，调用返回时 daemon 已经在用新值
    let now = td.ctx.config.get();
    assert_eq!(
        (now.sessions.max_concurrent, now.sessions.idle_timeout.as_str(), now.log.level.as_str()),
        (8, "45m", "debug")
    );
    assert_eq!(now.worktree.branch_prefix, "team");
    assert!(now.worktree.auto_archive_on_pr_close);
    assert_eq!(now.notify.turn_finished, "never");

    // 审计：每个真正变了的键一条 config.changed，带旧值、新值和发起的设备
    let entries = audit(&td).await;
    let mut targets: Vec<_> = entries.iter().map(|e| e.target.clone().unwrap()).collect();
    targets.sort();
    assert_eq!(
        targets,
        vec![
            "log.level",
            "notify.turn_finished",
            "sessions.idle_timeout",
            "sessions.max_concurrent",
            "worktree.auto_archive_on_pr_close",
            "worktree.branch_prefix",
        ]
    );
    let e = entries.iter().find(|e| e.target.as_deref() == Some("sessions.max_concurrent")).unwrap();
    assert_eq!((e.device_name.as_str(), e.device_id.as_str()), ("Pixel 9", "dev-Pixel 9"));
    assert_eq!(e.detail, json!({ "old": 3, "new": 8 }));
    let e = entries.iter().find(|e| e.target.as_deref() == Some("log.level")).unwrap();
    assert_eq!(e.detail, json!({ "old": "info", "new": "debug" }));
    td.shutdown().await;
}

#[tokio::test]
async fn a_noop_request_writes_nothing_and_audits_nothing() {
    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    let before = file(&td);
    let r: NodeConfigResult = c
        .call(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("sessions.max_concurrent", json!(3)), ("log.level", json!("info"))]),
        )
        .await
        .unwrap();
    assert_eq!(item(&r, "sessions.max_concurrent").value, json!(3));
    assert_eq!(file(&td), before);
    assert!(audit(&td).await.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn set_config_keeps_the_comments_and_the_rest_of_the_file_as_it_was() {
    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    // 手写的注释：整行注释、行内注释、段落之间的空行
    let mut text = file(&td);
    text = text.replace("max_concurrent = 3", "max_concurrent = 3 # keep this comment");
    text.push_str("\n# my own notes, do not touch\n");
    std::fs::write(td.paths.config_toml(), &text).unwrap();
    td.ctx.config.reload().unwrap();

    let _: NodeConfigResult = c
        .call(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("sessions.max_concurrent", json!(6)), ("log.level", json!("warn"))]),
        )
        .await
        .unwrap();

    let after = file(&td);
    let expected = text
        .replace("max_concurrent = 3 # keep this comment", "max_concurrent = 6 # keep this comment")
        .replace("level = \"info\"", "level = \"warn\"");
    assert_eq!(after, expected, "only the two values changed; comments and layout are untouched");
    assert!(after.contains("# my own notes, do not touch"));
    td.shutdown().await;
}

#[tokio::test]
async fn one_bad_change_means_none_is_written() {
    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    let before = file(&td);
    let cfg_before = td.ctx.config.get();

    // 前面的改动是合法的，最后一个越界：整个请求失败、文件不动、不审计、daemon 的配置不变
    let e = c
        .call::<Value>(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[
                ("sessions.max_concurrent", json!(8)),
                ("log.level", json!("debug")),
                ("preview.ticket_ttl", json!("1m")),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), (ErrorCode::Invalid, Some("preview.ticket_ttl".into()), Some("range".into())));
    assert_eq!(file(&td), before);
    assert_eq!(*td.ctx.config.get(), *cfg_before);
    assert!(audit(&td).await.is_empty());

    // 类型不对、枚举不对、范围不对、键重复、空请求：都是 invalid，带出错的键
    for (req, field, reason) in [
        (changes(&[("sessions.max_concurrent", json!("8"))]), Some("sessions.max_concurrent"), Some("type")),
        (changes(&[("sessions.max_concurrent", json!(0))]), Some("sessions.max_concurrent"), Some("range")),
        (changes(&[("sessions.max_concurrent", json!(33))]), Some("sessions.max_concurrent"), Some("range")),
        (changes(&[("log.level", json!("verbose"))]), Some("log.level"), Some("format")),
        (changes(&[("worktree.branch_prefix", json!("main"))]), Some("worktree.branch_prefix"), Some("format")),
        (changes(&[("sessions.idle_timeout", json!("later"))]), Some("sessions.idle_timeout"), Some("format")),
        (changes(&[("log.level", json!("info")), ("log.level", json!("warn"))]), Some("log.level"), Some("duplicate")),
        (json!({ "changes": [] }), None, None),
    ] {
        let e = c.call::<Value>(&td, methods::NODE_SET_CONFIG, req.clone()).await.unwrap_err();
        assert_eq!(code(&e), (ErrorCode::Invalid, field.map(String::from), reason.map(String::from)), "{req}");
    }
    // 参数形状不对（不是 `{ changes: [...] }`）
    let e = c
        .call::<Value>(&td, methods::NODE_SET_CONFIG, json!({ "key": "log.level", "value": "warn" }))
        .await
        .unwrap_err();
    assert_eq!(e.data.unwrap().code, ErrorCode::Invalid);
    assert_eq!(file(&td), before);
    assert!(audit(&td).await.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn keys_outside_the_whitelist_are_refused_and_nothing_is_written() {
    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    let before = file(&td);
    for key in [
        "agents.claude-code.command",
        "agents.claude-code.env",
        "agents.env",
        "workspace.root",
        "workspace.link_roots",
        "permissions.allow_session_env",
        "permissions.session_env_allow",
        "preview.allow_ports",
        "hub.url",
        "enroll.approval",
        "enroll.password",
        "sessions.max_attachment_bytes",
        "sessions.history_retention",
        "log.retention_days",
    ] {
        let e = c
            .call::<Value>(&td, methods::NODE_SET_CONFIG, changes(&[("log.level", json!("warn")), (key, json!("x"))]))
            .await
            .unwrap_err();
        assert_eq!(code(&e), (ErrorCode::Forbidden, Some(key.into()), Some("not_editable".into())), "{key}");
    }
    assert_eq!(file(&td), before);
    assert_eq!(td.ctx.config.get().log.level, "info");
    assert!(audit(&td).await.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn the_security_switches_can_be_turned_off_but_not_back_on() {
    let td = TestDaemon::start().await;
    let mut c = td.client("stolen-phone");
    assert!(td.ctx.config.get().permissions.allow_dangerous_modes && td.ctx.config.get().preview.enabled);
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.preview);
    let _ = c.drain_now();

    // true -> false：放行，而且热加载走完整条路径（预览关了、agent 列表里不再有危险模式并推 agent.changed）
    let r: NodeConfigResult = c
        .call(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("permissions.allow_dangerous_modes", json!(false)), ("preview.enabled", json!(false))]),
        )
        .await
        .unwrap();
    assert_eq!(item(&r, "permissions.allow_dangerous_modes").value, json!(false));
    assert_eq!(item(&r, "preview.enabled").value, json!(false));
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(!info.capabilities.preview, "preview.enabled = false is applied immediately");
    let list: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({})).await.unwrap();
    assert!(
        list.agents.iter().flat_map(|a| &a.modes).all(|m| m.risk != ModeRisk::Dangerous),
        "dangerous modes are gone from the client view"
    );
    assert!(c.wait_notification(Duration::from_secs(5), |n| n.method == methods::AGENT_CHANGED).await.is_some());

    // false -> true：放宽，forbidden；文件和 daemon 的配置都不动，一个键也不写（同一请求里别的合法改动也不写）
    let before = file(&td);
    for key in ["permissions.allow_dangerous_modes", "preview.enabled"] {
        let e = c
            .call::<Value>(&td, methods::NODE_SET_CONFIG, changes(&[("log.level", json!("trace")), (key, json!(true))]))
            .await
            .unwrap_err();
        assert_eq!(code(&e), (ErrorCode::Forbidden, Some(key.into()), Some("tighten_only".into())), "{key}");
    }
    assert_eq!(file(&td), before);
    assert!(!td.ctx.config.get().permissions.allow_dangerous_modes && !td.ctx.config.get().preview.enabled);
    assert_eq!(td.ctx.config.get().log.level, "info");

    // 已经关着再发 false：空操作，不报错
    let _: NodeConfigResult =
        c.call(&td, methods::NODE_SET_CONFIG, changes(&[("preview.enabled", json!(false))])).await.unwrap();

    // 审计里能看到是谁、关了什么
    let entries = audit(&td).await;
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.device_name == "stolen-phone"));
    let e = entries.iter().find(|e| e.target.as_deref() == Some("permissions.allow_dangerous_modes")).unwrap();
    assert_eq!(e.detail, json!({ "old": true, "new": false }));

    // 在机器上放宽（本机 CLI 的 `anycon config set`）仍然可以：远程的限制不影响机器主人
    anycon_daemon::config::set_value_blocking(&td.paths, "preview.enabled", "true").await.unwrap();
    td.ctx.config.reload().unwrap();
    assert!(td.ctx.config.get().preview.enabled);
    td.shutdown().await;
}

#[tokio::test]
async fn concurrent_set_config_calls_never_leave_an_inconsistent_file() {
    let td = TestDaemon::start().await;
    let mut cs: Vec<_> = (0..6).map(|i| td.client(&format!("dev{i}"))).collect();
    let mut it = cs.iter_mut();
    let (c0, c1, c2, c3, c4, c5) = (
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
    );

    // 不同的键同时改：全部生效；同时本机 CLI 也在改一个白名单之外的键（走同一把文件锁）
    let local = anycon_daemon::config::set_value_blocking(&td.paths, "workspace.trash_retention_days", "11");
    let (a, b, c, d, e, l) = tokio::join!(
        c0.call::<NodeConfigResult>(&td, methods::NODE_SET_CONFIG, changes(&[("sessions.max_concurrent", json!(9))])),
        c1.call::<NodeConfigResult>(&td, methods::NODE_SET_CONFIG, changes(&[("sessions.idle_timeout", json!("33m"))])),
        c2.call::<NodeConfigResult>(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("notify.session_idle_after", json!("12m"))])
        ),
        c3.call::<NodeConfigResult>(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("worktree.branch_prefix", json!("conc"))])
        ),
        c4.call::<NodeConfigResult>(
            &td,
            methods::NODE_SET_CONFIG,
            changes(&[("log.level", json!("warn")), ("preview.enabled", json!(false))])
        ),
        local,
    );
    for r in [a, b, c, d, e] {
        r.unwrap();
    }
    l.unwrap();
    td.ctx.config.reload().unwrap();
    let cfg = td.ctx.config.get();
    assert_eq!(cfg.sessions.max_concurrent, 9);
    assert_eq!(cfg.sessions.idle_timeout, "33m");
    assert_eq!(cfg.notify.session_idle_after, "12m");
    assert_eq!(cfg.worktree.branch_prefix, "conc");
    assert_eq!((cfg.log.level.as_str(), cfg.preview.enabled), ("warn", false));
    assert_eq!(cfg.workspace.trash_retention_days, 11);
    // 文件是一份完整的 TOML，重新加载得到同样的东西
    let reloaded = anycon_daemon::config::Config::load(&td.paths).unwrap();
    assert_eq!(*cfg, reloaded);
    assert_eq!(audit(&td).await.len(), 6);

    // 同一个键同时改成不同的值：最后写的赢，但 daemon 的配置、文件、返回结果三者一致
    let values = [2u64, 4, 6, 10, 20, 30];
    let (r0, r1, r2, r3, r4, r5) = tokio::join!(
        set_max_concurrent(c0, &td, values[0]),
        set_max_concurrent(c1, &td, values[1]),
        set_max_concurrent(c2, &td, values[2]),
        set_max_concurrent(c3, &td, values[3]),
        set_max_concurrent(c4, &td, values[4]),
        set_max_concurrent(c5, &td, values[5]),
    );
    let seen = [r0, r1, r2, r3, r4, r5];
    assert!(seen.iter().all(|v| values.contains(v)), "{seen:?}");
    let on_disk = anycon_daemon::config::Config::load(&td.paths).unwrap().sessions.max_concurrent as u64;
    assert!(values.contains(&on_disk));
    assert_eq!(td.ctx.config.get().sessions.max_concurrent as u64, on_disk);
    // 每次真正的改动都有一条审计；调用是串行处理的，所以最后一条审计的新值就是文件里的值
    let mut chain: Vec<_> =
        audit(&td).await.into_iter().filter(|e| e.target.as_deref() == Some("sessions.max_concurrent")).collect();
    chain.sort_by_key(|e| e.ts_ms);
    assert_eq!(chain.last().unwrap().detail["new"], json!(on_disk));
    td.shutdown().await;
}

async fn set_max_concurrent(c: &mut TestClient, td: &TestDaemon, v: u64) -> u64 {
    let r: NodeConfigResult =
        c.call(td, methods::NODE_SET_CONFIG, changes(&[("sessions.max_concurrent", json!(v))])).await.unwrap();
    item(&r, "sessions.max_concurrent").value.as_u64().unwrap()
}

#[tokio::test]
async fn node_logs_returns_the_tail_of_the_daemon_log_file() {
    let td = TestDaemon::start().await;
    let mut c = td.client("web");
    let dir = td.paths.logs_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let body = [
        "2026-10-30T10:00:00.000000Z  INFO anycon_daemon::daemon: daemon started",
        "2026-10-30T10:00:01.000000Z  WARN anycon_daemon::peers: peer stalled peer=abc",
        "2026-10-30T10:00:02.000000Z ERROR anycon_daemon::store: cannot write error=disk full",
        "2026-10-30T10:00:03.000000Z  WARN anycon_adapters::codex::handle: codex: turn/interrupt failed error=SECRET",
        "2026-10-30T10:00:04.000000Z  WARN anycon_daemon::sessions: cancel failed session=sess_1 error=agent exited: SECRET",
    ]
    .join("\n");
    std::fs::write(dir.join("daemon.log.2026-10-30"), format!("{body}\n")).unwrap();

    let r: NodeLogsResult = c.call(&td, methods::NODE_LOGS, json!({})).await.unwrap();
    assert!(!r.truncated);
    assert_eq!(r.lines.len(), 4, "the adapter line is withheld: {:?}", r.lines);
    assert!(!r.lines.join("\n").contains("SECRET"));
    assert!(r.lines[3].ends_with("error=[withheld]"));

    let r: NodeLogsResult = c.call(&td, methods::NODE_LOGS, json!({ "lines": 1, "level": "warn" })).await.unwrap();
    assert_eq!(r.lines.len(), 1);
    assert!(r.lines[0].contains("cancel failed"));
    assert!(r.truncated);

    let r: NodeLogsResult = c.call(&td, methods::NODE_LOGS, json!({ "level": "error" })).await.unwrap();
    assert_eq!(r.lines.len(), 1);
    assert!(r.lines[0].contains("cannot write"));

    // 级别不是已知的值：invalid
    let e = c.call::<Value>(&td, methods::NODE_LOGS, json!({ "level": "loud" })).await.unwrap_err();
    assert_eq!(e.data.unwrap().code, ErrorCode::Invalid);
    td.shutdown().await;
}
