//! Codex 探测：用假 `codex`（`tests/bin/fake-codex`）回答 `--version`、`login status` 和 `app-server`
//! （`initialize` / `model/list` / `config/read`）。模型列表是真实 0.157.1 的录制（`tests/fixtures/codex/model_list.json`）。

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_adapters::codex::probe::{self, AuthState};
use anycon_adapters::CodexAdapter;
use anycon_agent_api::{Adapter, AdapterConfig, ProbeContext};
use anycon_protocol::{AgentInfo, AgentStatus, ConfigOptionKind, ModeRisk, ModelInfo};
use common::*;

fn ids(models: &[ModelInfo]) -> Vec<&str> {
    models.iter().map(|m| m.id.as_str()).collect()
}

async fn probe_with(h: &CodexHarness) -> AgentInfo {
    CodexAdapter::new().probe(&h.probe_ctx()).await
}

#[tokio::test]
async fn ready_when_installed_and_logged_in() {
    let h = CodexHarness::new();
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert!(info.enabled && info.allow_custom_model);
    assert_eq!((info.id.as_str(), info.label.as_str(), info.driver.as_str()), ("codex", "Codex", "codex-app-server"));
    assert_eq!(info.version.as_deref(), Some("0.157.1"));
    assert_eq!(info.path.as_deref(), Some(fake_codex().to_str().unwrap()));
    assert!(info.issue.is_none());
    assert!(info.probed_at_ms > 0);

    // 模型：来自 model/list（真实录制），一个默认，思考等级挂在模型上
    assert_eq!(
        ids(&info.models),
        vec!["gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"]
    );
    assert_eq!(
        info.models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["gpt-6-astra"]
    );
    let astra = &info.models[0];
    assert_eq!(astra.label, "GPT-6-Astra");
    assert_eq!(astra.description.as_deref(), Some("Frontier intelligence for the most demanding work."));
    let t = astra.thinking.as_ref().unwrap();
    assert_eq!(
        t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        vec!["low", "medium", "high", "xhigh", "max", "ultra"]
    );
    assert_eq!(t.default.as_deref(), Some("low"));
    assert_eq!(t.options[3].label, "Extra high");
    // 小一点的模型没有 ultra，默认等级也不同
    let luna = info.models.iter().find(|m| m.id == "gpt-6-luna").unwrap().thinking.as_ref().unwrap();
    assert!(!luna.options.iter().any(|o| o.id == "ultra"));
    assert_eq!(luna.default.as_deref(), Some("medium"));
    let old = info.models.iter().find(|m| m.id == "gpt-5.5").unwrap().thinking.as_ref().unwrap();
    assert_eq!(old.options.len(), 4);

    // 权限模式：四档
    let modes: Vec<(&str, ModeRisk, bool)> = info.modes.iter().map(|m| (m.id.as_str(), m.risk, m.default)).collect();
    assert_eq!(
        modes,
        vec![
            ("read_only", ModeRisk::Safe, false),
            ("workspace", ModeRisk::Normal, true),
            ("auto_review", ModeRisk::Normal, false),
            ("full_access", ModeRisk::Dangerous, false),
        ]
    );
    // 其他选项
    assert_eq!(
        info.options.iter().map(|o| (o.id.as_str(), o.kind)).collect::<Vec<_>>(),
        vec![("fast_mode", ConfigOptionKind::Bool), ("model_reasoning_summary", ConfigOptionKind::Select),]
    );
    // 能力
    let c = &info.capabilities;
    assert!(c.resume && c.interrupt && c.interject && c.thinking_stream && c.usage && c.env);
    assert!(!c.rewind);
    assert_eq!(c.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp", "*/*"]);
}

#[tokio::test]
async fn the_probe_session_runs_in_the_scratch_dir_with_the_given_env() {
    let h = CodexHarness::new().var("ANYCON_TEST_VAR", "from-daemon-env");
    probe_with(&h).await;
    let a = h.argv_entry();
    assert!(a["args"].as_array().unwrap().iter().any(|x| x == "app-server"));
    assert!(same_path(std::path::Path::new(a["cwd"].as_str().unwrap()), h.dir.path()), "{a}");
    assert_eq!(a["env"]["ANYCON_TEST_VAR"], "from-daemon-env");
    // 握手顺序：initialize → initialized → model/list → config/read，且只声明稳定版 API
    let methods: Vec<String> =
        h.stdin_messages().iter().filter_map(|m| m["method"].as_str().map(str::to_string)).collect();
    assert_eq!(methods, vec!["initialize", "initialized", "model/list", "config/read", "account/read"]);
    let init = &h.requests("initialize")[0];
    assert_eq!(init["params"]["clientInfo"]["name"], "anycon");
    assert_eq!(init["params"]["capabilities"]["experimentalApi"], false);
    // 不要的大通知（整个 turn 的累计 diff 等）让 app-server 别发
    let opt_out = init["params"]["capabilities"]["optOutNotificationMethods"].as_array().unwrap();
    assert!(opt_out.iter().any(|m| m == "turn/diff/updated"));
}

#[tokio::test]
async fn model_list_is_paged() {
    let h = CodexHarness::new().var("FAKE_CODEX_MODEL_PAGES", "3");
    let info = probe_with(&h).await;
    assert_eq!(info.models.len(), 7);
    assert_eq!(h.requests("model/list").len(), 3);
    let cursors: Vec<Option<String>> =
        h.requests("model/list").iter().map(|m| m["params"]["cursor"].as_str().map(str::to_string)).collect();
    assert_eq!(cursors, vec![None, Some("page-1".into()), Some("page-2".into())]);
}

#[tokio::test]
async fn hidden_models_are_skipped_and_ids_deduplicated() {
    let h = CodexHarness::new();
    let mut list: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(codex_fixture_path("model_list.json")).unwrap()).unwrap();
    let mut hidden = list["data"][1].clone();
    hidden["id"] = "secret-model".into();
    hidden["model"] = "secret-model".into();
    hidden["hidden"] = true.into();
    list["data"].as_array_mut().unwrap().push(hidden);
    let dup = list["data"][0].clone();
    list["data"].as_array_mut().unwrap().push(dup);
    let path = h.dir.path().join("models.json");
    std::fs::write(&path, list.to_string()).unwrap();
    let h = h.var("FAKE_CODEX_MODELS", &path.to_string_lossy());
    let info = probe_with(&h).await;
    assert_eq!(info.models.len(), 7);
    assert!(!info.models.iter().any(|m| m.id == "secret-model"));
}

#[tokio::test]
async fn config_model_and_effort_become_the_defaults() {
    // 用户 config.toml 里 model = gpt-5.5、model_reasoning_effort = high（真实 config/read 的录制）
    let h =
        CodexHarness::new().var("FAKE_CODEX_CONFIG", &codex_fixture_path("config_read_custom.json").to_string_lossy());
    let info = probe_with(&h).await;
    assert_eq!(info.models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["gpt-5.5"]);
    let m = info.models.iter().find(|m| m.id == "gpt-5.5").unwrap();
    assert_eq!(m.thinking.as_ref().unwrap().default.as_deref(), Some("high"));
    assert_eq!(info.models.len(), 7);
}

#[tokio::test]
async fn config_model_missing_from_the_list_is_appended_as_the_default() {
    let h = CodexHarness::new();
    let cfg =
        serde_json::json!({"config": {"model": "my-private-model", "model_reasoning_effort": "high"}, "origins": {}});
    let path = h.dir.path().join("config.json");
    std::fs::write(&path, cfg.to_string()).unwrap();
    let h = h.var("FAKE_CODEX_CONFIG", &path.to_string_lossy());
    let info = probe_with(&h).await;
    assert_eq!(info.models.len(), 8);
    let last = info.models.last().unwrap();
    assert_eq!((last.id.as_str(), last.default), ("my-private-model", true));
    // 自定义提供方的模型：取列表模型共有的等级，默认用配置的
    let t = last.thinking.as_ref().unwrap();
    assert_eq!(t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "medium", "high", "xhigh"]);
    assert_eq!(t.default.as_deref(), Some("high"));
    assert_eq!(info.models.iter().filter(|m| m.default).count(), 1);
}

#[tokio::test]
async fn additional_models_from_the_config_are_appended() {
    let mut h = CodexHarness::new();
    h.config.additional_models = vec![ModelInfo {
        id: "corp-gateway-model".into(),
        label: "Corp".into(),
        description: None,
        default: true,
        thinking: None,
        tags: vec![],
    }];
    let info = probe_with(&h).await;
    assert_eq!(info.models.len(), 8);
    assert_eq!(
        info.models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["gpt-6-astra"]
    );
}

#[tokio::test]
async fn model_list_failure_keeps_the_previous_models_and_status() {
    let h = CodexHarness::new();
    let previous = probe_with(&h).await;
    assert_eq!(previous.models.len(), 7);

    let h2 = CodexHarness::new().var("FAKE_CODEX_MODEL_LIST", "error");
    let mut ctx = h2.probe_ctx();
    ctx.previous = Some(previous.clone());
    let info = CodexAdapter::new().probe(&ctx).await;
    // 不因此把 status 打成 error
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(ids(&info.models), ids(&previous.models));

    // 没有上一次的结果：空列表 + 允许自定义模型
    let info = probe_with(&h2).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert!(info.models.is_empty());
    assert!(info.allow_custom_model);
    // 模式和选项仍然在
    assert_eq!(info.modes.len(), 4);

    // model/list 失败但 config/read 成功：至少有 config.toml 里的那个模型
    let h3 = CodexHarness::new()
        .var("FAKE_CODEX_MODEL_LIST", "error")
        .var("FAKE_CODEX_CONFIG", &codex_fixture_path("config_read_custom.json").to_string_lossy());
    let info = probe_with(&h3).await;
    assert_eq!(ids(&info.models), vec!["gpt-5.5"]);
    assert!(info.models[0].default);
}

#[tokio::test]
async fn app_server_that_dies_at_startup_does_not_break_the_probe() {
    let h = CodexHarness::new().var("FAKE_CODEX_INIT", "fail");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert!(info.models.is_empty() && info.allow_custom_model);
}

#[tokio::test]
async fn config_read_failure_still_gives_models() {
    let h = CodexHarness::new().var("FAKE_CODEX_CONFIG_ERROR", "1");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert_eq!(info.models.len(), 7);
    assert!(info.models[0].default);
}

#[tokio::test]
async fn cheap_probe_does_not_start_app_server_while_the_login_status_is_unchanged() {
    let h = CodexHarness::new();
    // 同一个 adapter 实例：cheap 探测要沿用它上一次的认证判定
    let adapter = CodexAdapter::new();
    let previous = adapter.probe(&h.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::Ready);
    let _ = std::fs::remove_file(&h.log);
    let mut ctx = h.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous.clone());
    let info = adapter.probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert_eq!(ids(&info.models), ids(&previous.models));
    assert!(h.log_entries().iter().all(|e| e["kind"] != "argv"), "no app-server should have been started");
}

#[tokio::test]
async fn cheap_probe_asks_account_read_only_when_the_login_status_changed() {
    let h = CodexHarness::new();
    let adapter = CodexAdapter::new();
    let previous = adapter.probe(&h.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::Ready);

    // 退出登录了：login status 变了 → 补跑一次 account/read（只做这一步，不拉模型列表）
    let out = CodexHarness::new().var("FAKE_CODEX_AUTH", "out");
    let mut ctx = out.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous.clone());
    let info = adapter.probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated, "{:?}", info.issue);
    assert_eq!(info.issue.unwrap().hint.as_deref(), Some("codex login"));
    assert_eq!(ids(&info.models), ids(&previous.models), "the model list still comes from the previous probe");
    let methods: Vec<String> =
        out.stdin_messages().iter().filter_map(|m| m["method"].as_str().map(str::to_string)).collect();
    assert_eq!(methods, vec!["initialize", "initialized", "account/read"]);

    // 再来一次，login status 还是「未登录」：沿用判定，不再拉起 app-server
    let _ = std::fs::remove_file(&out.log);
    let blocked = adapter.probe(&ctx).await;
    assert_eq!(blocked.status, AgentStatus::NotAuthenticated);
    assert!(out.log_entries().iter().all(|e| e["kind"] != "argv"));

    // 重新登录：又变了 → 再补跑一次 account/read，恢复
    let back = CodexHarness::new();
    let mut ctx = back.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous);
    assert_eq!(adapter.probe(&ctx).await.status, AgentStatus::Ready);
    assert_eq!(back.requests("account/read").len(), 1);
    assert!(back.requests("model/list").is_empty() && back.requests("config/read").is_empty());
}

#[tokio::test]
async fn cheap_probe_without_a_memo_or_previous_result_checks_account_read() {
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out");
    let previous = CodexAdapter::new().probe(&h.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::NotAuthenticated);
    let _ = std::fs::remove_file(&h.log);
    // 另一个 adapter 实例（没有记录）：不能凭 `previous` 里的状态猜
    let mut ctx = h.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous);
    let info = CodexAdapter::new().probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    assert_eq!(h.requests("account/read").len(), 1);
    // `previous` 为空也一样
    let _ = std::fs::remove_file(&h.log);
    let adapter = CodexAdapter::new();
    adapter.probe(&h.probe_ctx()).await;
    let _ = std::fs::remove_file(&h.log);
    let mut ctx = h.probe_ctx();
    ctx.cheap = true;
    assert_eq!(adapter.probe(&ctx).await.status, AgentStatus::NotAuthenticated);
    assert_eq!(h.requests("account/read").len(), 1);
}

#[tokio::test]
async fn cheap_probe_does_not_reuse_a_verdict_that_came_from_login_status() {
    // account/read 用不了时退回 login status 得出的判定不是权威的：cheap 探测每次都再试 account/read
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
    let adapter = CodexAdapter::new();
    let previous = adapter.probe(&h.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::NotAuthenticated);
    let _ = std::fs::remove_file(&h.log);
    let mut ctx = h.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous);
    adapter.probe(&ctx).await;
    assert_eq!(h.requests("account/read").len(), 1);
}

/// 完整探测里 `account/read` 用不了（超时 / 报错）：退回**同一个二进制上一次权威的判定**，再不行才是 `login status`。
#[tokio::test]
async fn full_probe_keeps_the_last_authoritative_verdict_when_account_read_fails() {
    let adapter = CodexAdapter::new();
    // 自定义提供方：login status 说未登录，account/read 说不需要登录 → ready，这条判定是权威的
    let first = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "custom");
    let previous = adapter.probe(&first.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::Ready, "{:?}", previous.issue);

    // 下一次完整探测里 account/read 超时：不能因此退回 login status 的「未登录」把会话挡掉
    let hang = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "hang");
    let mut ctx = hang.probe_ctx();
    ctx.deadline = Duration::from_secs(4);
    ctx.previous = Some(previous.clone());
    let started = Instant::now();
    let info = adapter.probe(&ctx).await;
    assert!(started.elapsed() < Duration::from_secs(9), "{:?}", started.elapsed());
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.models.len(), 7, "the model list does not depend on account/read");

    // 报错也一样；可以连续多次（沿用判定时不改记录，所以不会「用着用着变成 login status 的判定」）
    for _ in 0..2 {
        let err = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
        assert_eq!(adapter.probe(&err.probe_ctx()).await.status, AgentStatus::Ready);
    }
    // app-server 整个起不来也一样
    let dead = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_INIT", "fail");
    assert_eq!(adapter.probe(&dead.probe_ctx()).await.status, AgentStatus::Ready);

    // 记录没被改：login status 还是「未登录」时，cheap 探测照旧沿用，不拉起 app-server
    let _ = std::fs::remove_file(&hang.log);
    let mut cheap = hang.probe_ctx();
    cheap.cheap = true;
    cheap.previous = Some(previous);
    assert_eq!(adapter.probe(&cheap).await.status, AgentStatus::Ready);
    assert!(hang.log_entries().iter().all(|e| e["kind"] != "argv"), "the verdict is still the authoritative one");

    // 对照：没有上一次权威判定的 adapter 实例，同样的情况退回 login status → 未登录
    let hang = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "hang");
    let mut ctx = hang.probe_ctx();
    ctx.deadline = Duration::from_secs(4);
    assert_eq!(CodexAdapter::new().probe(&ctx).await.status, AgentStatus::NotAuthenticated);
}

#[tokio::test]
async fn the_last_authoritative_verdict_beats_an_unchanged_login_status_in_both_directions() {
    // 上一次 account/read 说要登录（挡），login status 说已登录（比如钥匙串里的旧凭证失效）；
    // 这次 account/read 报错，login status 还是「已登录」→ 沿用权威的判定，仍然挡
    let adapter = CodexAdapter::new();
    let blocked = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "out");
    assert_eq!(adapter.probe(&blocked.probe_ctx()).await.status, AgentStatus::NotAuthenticated);
    let flaky = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "error");
    assert_eq!(adapter.probe(&flaky.probe_ctx()).await.status, AgentStatus::NotAuthenticated);
    // account/read 恢复了：以新的答案为准
    let back = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "chatgpt");
    assert_eq!(adapter.probe(&back.probe_ctx()).await.status, AgentStatus::Ready);
}

/// `login status` 和当时记下的不一样了（用户刚登录 / 退出），旧的权威判定多半是错的，`account/read` 又问不到 → 用 `login status`。
#[tokio::test]
async fn full_probe_falls_back_to_login_status_when_it_changed_and_account_read_fails() {
    // 刚 `codex login`：上一次 account/read 说要登录（挡，当时 login status = 未登录），现在 login status = 已登录，account/read 报错 → 可用
    let adapter = CodexAdapter::new();
    let before = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "out");
    assert_eq!(adapter.probe(&before.probe_ctx()).await.status, AgentStatus::NotAuthenticated);
    let logged_in = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "error");
    let info = adapter.probe(&logged_in.probe_ctx()).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);

    // 反方向：上一次 account/read 说可用（当时 login status = 已登录），现在 login status = 未登录，account/read 超时 → 挡
    let adapter = CodexAdapter::new();
    let before = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "chatgpt");
    assert_eq!(adapter.probe(&before.probe_ctx()).await.status, AgentStatus::Ready);
    let logged_out = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "hang");
    let mut ctx = logged_out.probe_ctx();
    ctx.deadline = Duration::from_secs(4);
    let info = adapter.probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated, "{:?}", info.issue);
    assert_eq!(info.models.len(), 7, "the model list does not depend on account/read");

    // 登录状态没变时照旧沿用（自定义提供方：login status 一直是未登录，account/read 一直说不需要）
    let adapter = CodexAdapter::new();
    let custom = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "custom");
    assert_eq!(adapter.probe(&custom.probe_ctx()).await.status, AgentStatus::Ready);
    let flaky = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
    assert_eq!(adapter.probe(&flaky.probe_ctx()).await.status, AgentStatus::Ready);
}

#[tokio::test]
async fn an_authoritative_verdict_is_only_kept_for_the_same_binary() {
    let adapter = CodexAdapter::new();
    let first = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "custom");
    assert_eq!(adapter.probe(&first.probe_ctx()).await.status, AgentStatus::Ready);

    // 换了一个二进制（用户改了 `command`）：上一次的判定不属于它，退回 login status
    let other = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
    let wrapper = other.dir.path().join("codex-other");
    std::fs::write(&wrapper, format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", fake_codex().display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut ctx = other.probe_ctx();
    ctx.config.command = Some(wrapper.to_string_lossy().into_owned());
    assert_eq!(adapter.probe(&ctx).await.status, AgentStatus::NotAuthenticated);
    // 回到原来的二进制：新的记录是 login status 得出的（不权威），所以也没有可沿用的
    assert_eq!(adapter.probe(&other.probe_ctx()).await.status, AgentStatus::NotAuthenticated);
}

#[tokio::test]
async fn cheap_probe_trusts_a_changed_login_status_over_an_older_verdict() {
    // cheap 探测是因为 login status 变了才来问 account/read 的：问不到时，旧判定多半就是错的，直接用 login status
    let adapter = CodexAdapter::new();
    let first = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "chatgpt");
    let previous = adapter.probe(&first.probe_ctx()).await;
    assert_eq!(previous.status, AgentStatus::Ready);
    let logged_out = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
    let mut ctx = logged_out.probe_ctx();
    ctx.cheap = true;
    ctx.previous = Some(previous);
    assert_eq!(adapter.probe(&ctx).await.status, AgentStatus::NotAuthenticated);
}

#[tokio::test]
async fn not_logged_in_is_not_authenticated_with_a_login_hint() {
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "login_required");
    assert_eq!(issue.hint.as_deref(), Some("codex login"));
    // 模型 / 模式仍然给出，设置面板可以先配
    assert_eq!(info.models.len(), 7);
    assert_eq!(info.modes.len(), 4);
}

#[tokio::test]
async fn unclear_auth_does_not_block() {
    // auth.json 损坏之类：退出码 1 但不是 "Not logged in"
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "error");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    // API key 登录
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "apikey");
    assert_eq!(probe_with(&h).await.status, AgentStatus::Ready);
}

#[tokio::test]
async fn hanging_login_status_respects_the_deadline() {
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "hang");
    let mut ctx = h.probe_ctx();
    ctx.deadline = Duration::from_secs(3);
    let started = Instant::now();
    let info = CodexAdapter::new().probe(&ctx).await;
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    // 认证没能确定 = unknown，不阻塞
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
}

#[tokio::test]
async fn slow_model_list_is_cut_off_by_the_deadline() {
    for var in ["FAKE_CODEX_MODEL_LIST", "FAKE_CODEX_INIT"] {
        let h = CodexHarness::new().var(var, "hang");
        let mut ctx = h.probe_ctx();
        ctx.deadline = Duration::from_secs(4);
        let started = Instant::now();
        let info = CodexAdapter::new().probe(&ctx).await;
        assert!(started.elapsed() < Duration::from_secs(9), "{var}: {:?}", started.elapsed());
        assert_eq!(info.status, AgentStatus::Ready, "{var}: {:?}", info.issue);
        assert!(info.models.is_empty() && info.allow_custom_model);
    }
}

#[tokio::test]
async fn old_versions_are_unsupported_with_an_upgrade_hint() {
    let h = CodexHarness::new().var("FAKE_CODEX_VERSION_TEXT", "codex-cli 0.150.2");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::UnsupportedVersion);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "version_too_old");
    assert_eq!(issue.hint.as_deref(), Some("npm i -g @openai/codex@latest"));
    assert_eq!(info.version.as_deref(), Some("0.150.2"));
    // 恰好是最低版本
    let h = CodexHarness::new().var("FAKE_CODEX_VERSION_TEXT", "codex-cli 0.157.0");
    assert_eq!(probe_with(&h).await.status, AgentStatus::Ready);
}

#[tokio::test]
async fn unparseable_version_is_not_fatal() {
    let h = CodexHarness::new().var("FAKE_CODEX_VERSION_TEXT", "codex dev build");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.version.as_deref(), Some("codex dev build"));
}

#[tokio::test]
async fn missing_binary_is_not_installed_with_an_install_hint() {
    let dir = tempfile::tempdir().unwrap();
    let env = HashMap::from([
        ("PATH".to_string(), dir.path().to_string_lossy().into_owned()),
        ("HOME".to_string(), dir.path().to_string_lossy().into_owned()),
    ]);
    let ctx = ProbeContext {
        env: Arc::new(env),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(10),
        cheap: false,
        previous: None,
        scratch_dir: dir.path().to_path_buf(),
    };
    let info = CodexAdapter::new().probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::NotInstalled);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "binary_not_found");
    assert_eq!(issue.hint.as_deref(), Some("npm i -g @openai/codex"));
    assert!(info.models.is_empty());
}

#[tokio::test]
async fn binary_that_does_not_run_is_an_error() {
    let h = CodexHarness::new().var("FAKE_CODEX_VERSION_EXIT", "3");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Error);
    assert_eq!(info.issue.unwrap().code, "binary_failed");
}

#[tokio::test]
async fn disabled_is_reported_without_running_anything() {
    let mut h = CodexHarness::new();
    h.config.enabled = false;
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Disabled);
    assert!(!info.enabled);
    assert_eq!(info.issue.unwrap().hint.as_deref(), Some("set agents.codex.enabled = true"));
    assert!(h.log_entries().is_empty());
}

#[tokio::test]
async fn account_read_is_the_authority_on_whether_a_login_is_needed() {
    // 自定义提供方：`requiresOpenaiAuth = false`，login status 说未登录也不挡（真实录制的响应）
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "custom");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    // 需要登录、没有账户 → 挡
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "out");
    let info = probe_with(&h).await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    assert_eq!(info.issue.unwrap().code, "login_required");
    // 有账户（ChatGPT / API key）→ 不挡，即使 login status 的文字说不准
    for account in ["chatgpt", "apikey"] {
        let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "error").var("FAKE_CODEX_ACCOUNT", account);
        assert_eq!(probe_with(&h).await.status, AgentStatus::Ready, "{account}");
    }
    // account/read 说没有账户、需要登录，而 login status 却说登录了（比如钥匙串里的旧凭证失效）：以 account/read 为准
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "out");
    assert_eq!(probe_with(&h).await.status, AgentStatus::NotAuthenticated);
    // 不再按 config.toml 里 model_provider 的名字猜：写了自定义提供方，但 account/read 说需要登录 → 挡
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "out");
    std::fs::create_dir_all(h.dir.path().join(".codex")).unwrap();
    std::fs::write(h.dir.path().join(".codex/config.toml"), "model_provider = \"ollama\"\n").unwrap();
    assert_eq!(probe_with(&h).await.status, AgentStatus::NotAuthenticated);
    // 反过来：config.toml 里什么都没写，account/read 说不需要（profile、`-c model_provider=…` 选的提供方）→ 不挡
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "custom");
    assert_eq!(probe_with(&h).await.status, AgentStatus::Ready);
}

#[tokio::test]
async fn without_account_read_only_a_confirmed_logout_blocks() {
    // account/read 失败（或 app-server 起不来）：退回 login status
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "error");
    assert_eq!(probe_with(&h).await.status, AgentStatus::NotAuthenticated);
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "in").var("FAKE_CODEX_ACCOUNT", "error");
    assert_eq!(probe_with(&h).await.status, AgentStatus::Ready);
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "error").var("FAKE_CODEX_ACCOUNT", "error");
    assert_eq!(probe_with(&h).await.status, AgentStatus::Ready);
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_INIT", "fail");
    assert_eq!(probe_with(&h).await.status, AgentStatus::NotAuthenticated);
    // account/read 超时
    let h = CodexHarness::new().var("FAKE_CODEX_AUTH", "out").var("FAKE_CODEX_ACCOUNT", "hang");
    let mut ctx = h.probe_ctx();
    ctx.deadline = Duration::from_secs(4);
    let started = Instant::now();
    let info = CodexAdapter::new().probe(&ctx).await;
    assert!(started.elapsed() < Duration::from_secs(9), "{:?}", started.elapsed());
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    assert_eq!(info.models.len(), 7, "the model list does not depend on account/read");
}

#[tokio::test]
async fn auth_status_reads_stderr_and_the_exit_code() {
    let h = CodexHarness::new();
    let ctx = h.probe_ctx();
    let res = anycon_adapters::common::resolve_binary(anycon_adapters::common::ResolveOptions {
        env: &h.env,
        config_command: ctx.config.command.as_deref(),
        default_name: "codex",
        probe_args: &["--version"],
        per_candidate_timeout: Duration::from_secs(20),
        cwd: h.dir.path(),
        probe_all: false,
    })
    .await;
    let auth = |mode: &'static str| {
        let mut env = h.env.clone();
        env.insert("FAKE_CODEX_AUTH".into(), mode.into());
        let res = res.clone();
        let cwd = h.dir.path().to_path_buf();
        async move { probe::auth_status(&res, &env, &cwd, Duration::from_secs(20)).await }
    };
    assert_eq!(auth("in").await, AuthState::LoggedIn { detail: Some("Logged in using ChatGPT".into()) });
    assert!(matches!(auth("apikey").await, AuthState::LoggedIn { detail: Some(d) } if d.contains("API key")));
    assert_eq!(auth("out").await, AuthState::LoggedOut);
    assert!(matches!(auth("error").await, AuthState::Unknown(_)));
}

#[tokio::test]
async fn diagnose_has_the_documented_fields() {
    let h = CodexHarness::new();
    let (text, fields) = anycon_adapters::diagnose("codex", &h.probe_ctx()).await.expect("codex is diagnosable");
    let get = |l: &str| fields.iter().find(|f| f.label == l).map(|f| f.value.clone());
    assert_eq!(get("adapter").as_deref(), Some("codex"));
    assert_eq!(get("status").as_deref(), Some("ready"));
    assert_eq!(get("version").as_deref(), Some("0.157.1"));
    assert_eq!(get("models").as_deref(), Some("7"));
    assert!(get("auth").unwrap().starts_with("logged in"));
    assert_eq!(get("resolved from").as_deref(), Some("config"));
    assert!(get("candidates").unwrap().contains("ok: codex-cli 0.157.1"));
    assert!(get("account").unwrap().contains("logged in (chatgpt)"), "{:?}", get("account"));
    assert!(get("auth verdict").unwrap().starts_with("account/read:"));
    assert!(get("models source").unwrap().contains("7 models"));
    assert!(fields.iter().any(|f| f.label.starts_with("which -a")));
    assert!(text.starts_with("Codex (codex)"));
    assert!(text.contains("status: ready"));
    // 未安装时给出 hint
    let dir = tempfile::tempdir().unwrap();
    let ctx = ProbeContext {
        env: Arc::new(HashMap::from([("PATH".to_string(), dir.path().to_string_lossy().into_owned())])),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(10),
        cheap: false,
        previous: None,
        scratch_dir: dir.path().to_path_buf(),
    };
    let (text, _) = anycon_adapters::diagnose("codex", &ctx).await.unwrap();
    assert!(text.contains("binary_not_found") && text.contains("npm i -g @openai/codex"), "{text}");
}

#[tokio::test]
async fn skips_a_broken_shim_earlier_on_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad");
    let good = dir.path().join("good");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::create_dir_all(&good).unwrap();
    let write = |d: &std::path::Path, body: String| {
        let p = d.join("codex");
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write(&bad, "#!/bin/sh\necho 'node: command not found' >&2\nexit 127\n".to_string());
    write(&good, format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", fake_codex().display()));
    let py = which::which("python3")
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let env = HashMap::from([
        ("PATH".to_string(), format!("{}:{}:{py}:/usr/bin:/bin", bad.display(), good.display())),
        ("HOME".to_string(), dir.path().to_string_lossy().into_owned()),
        ("ANYCON_VERSION_TIMEOUT_MS".to_string(), "20000".to_string()),
    ]);
    let ctx = ProbeContext {
        env: Arc::new(env),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(40),
        cheap: false,
        previous: None,
        scratch_dir: dir.path().to_path_buf(),
    };
    let info = CodexAdapter::new().probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.path.as_deref(), Some(good.join("codex").to_str().unwrap()));
}
