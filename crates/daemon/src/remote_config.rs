//! 远程查看 / 修改节点配置：`node.get_config` / `node.set_config`（[ADR-43](../../../docs/decisions.md)，docs/spec/daemon.md § 3）。
//!
//! `config.toml` 是机器主人的最后一道边界，而配对过的客户端可能被盗用，所以远程能碰的只有一张白名单（[`SPECS`]）：
//! 只收热加载、不扩大攻击面的键；安全相关的两个开关（`permissions.allow_dangerous_modes`、`preview.enabled`）只能从 `true`
//! 改成 `false`，要放宽得在机器上 `anycon config set`。白名单之外的键（`agents.*`、`workspace.*`、`hub.*`、`enroll.*`……）
//! 既不会返回，也不接受。
//!
//! 写入、热加载、审计复用本机 CLI 的同一套逻辑：[`crate::config::set_toml_values`]（保留注释和格式、整份配置校验通过才原子替换）
//! → [`crate::config::ConfigStore::reload`] → 审计 `config.changed`。所有改动先在 [`plan`] 里整体校验，一项不合法就一项都不写。

use std::sync::Arc;
use std::time::Duration;

use anycon_protocol::{
    ConfigChange, ConfigEditable, ConfigItem, ConfigValueKind, ErrorCode, NodeConfigResult, NodeSetConfigParams,
    RpcError,
};
use serde_json::{json, Value};

use crate::clients::Caller;
use crate::config::{validate_branch_prefix, Config, ConfigError, ConfigStore, PREVIEW_TTL_MAX, PREVIEW_TTL_MIN};
use crate::daemon::Ctx;
use crate::util::parse_duration;

const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

/// 一个键的取值规则。
#[derive(Debug, Clone, Copy)]
enum Rule {
    Bool,
    /// 闭区间。
    Int {
        min: u64,
        max: u64,
    },
    /// 闭区间；`0` 在 `sessions.idle_timeout` / `permissions.request_timeout` 上表示「不限」，所以下限是 0。
    Duration {
        min: Duration,
        max: Duration,
    },
    Enum(&'static [&'static str]),
    /// 字符数上限，外加这个键自己的格式规则。
    Text {
        max_chars: u64,
        check: fn(&str) -> Result<(), String>,
    },
}

/// 白名单的一项。
struct Spec {
    key: &'static str,
    rule: Rule,
    editable: ConfigEditable,
}

/// 可以远程查看 / 修改的键，也是 `node.get_config` 返回的顺序（按配置段分组）。
///
/// 只放热加载的键（`config::is_hot_key`），所以改完不需要重启 daemon。刻意**不**在这里的：`agents.*`（`command` / `env` 等于远程执行
/// 任意命令）、`workspace.*`、`permissions.allow_session_env` / `session_env_allow`、`preview.allow_ports`、`hub.*`、`enroll.*`
/// （已有专门的 RPC）、`sessions.max_attachment_bytes`、`sessions.history_retention`。
const SPECS: &[Spec] = &[
    Spec { key: "sessions.max_concurrent", rule: Rule::Int { min: 1, max: 32 }, editable: ConfigEditable::Any },
    Spec {
        key: "sessions.idle_timeout",
        rule: Rule::Duration { min: Duration::ZERO, max: WEEK },
        editable: ConfigEditable::Any,
    },
    Spec { key: "permissions.allow_dangerous_modes", rule: Rule::Bool, editable: ConfigEditable::TightenOnly },
    Spec {
        key: "permissions.request_timeout",
        rule: Rule::Duration { min: Duration::ZERO, max: WEEK },
        editable: ConfigEditable::Any,
    },
    Spec { key: "worktree.default", rule: Rule::Enum(&["worktree", "local"]), editable: ConfigEditable::Any },
    Spec {
        key: "worktree.branch_prefix",
        rule: Rule::Text { max_chars: 40, check: validate_branch_prefix },
        editable: ConfigEditable::Any,
    },
    Spec { key: "worktree.auto_archive_on_pr_close", rule: Rule::Bool, editable: ConfigEditable::Any },
    Spec { key: "preview.enabled", rule: Rule::Bool, editable: ConfigEditable::TightenOnly },
    Spec {
        key: "preview.ticket_ttl",
        rule: Rule::Duration { min: PREVIEW_TTL_MIN, max: PREVIEW_TTL_MAX },
        editable: ConfigEditable::Any,
    },
    Spec {
        key: "notify.turn_finished",
        rule: Rule::Enum(&["always", "when_unwatched", "never"]),
        editable: ConfigEditable::Any,
    },
    Spec {
        key: "notify.session_idle_after",
        rule: Rule::Duration { min: Duration::ZERO, max: WEEK },
        editable: ConfigEditable::Any,
    },
    Spec {
        key: "log.level",
        rule: Rule::Enum(&["error", "warn", "info", "debug", "trace"]),
        editable: ConfigEditable::Any,
    },
];

/// 白名单里的全部键（测试和文档核对用）。
pub fn whitelisted_keys() -> impl Iterator<Item = &'static str> {
    SPECS.iter().map(|s| s.key)
}

/// 这个键是不是被进程环境覆盖：`ANYCON_<段>__<键>`（和加载配置时是同一个判断），`log.level` 还包括接管日志过滤的
/// `ANYCON_LOG` / `RUST_LOG`。
pub fn process_env_overrides(key: &str) -> bool {
    crate::config::env_overrides(key) || (key == "log.level" && crate::logging::env_filter_override())
}

fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(root, |cur, seg| cur.get(seg))
}

fn kind_of(rule: &Rule) -> ConfigValueKind {
    match rule {
        Rule::Bool => ConfigValueKind::Bool,
        Rule::Int { .. } => ConfigValueKind::Int,
        Rule::Duration { .. } => ConfigValueKind::Duration,
        Rule::Enum(_) => ConfigValueKind::Enum,
        Rule::Text { .. } => ConfigValueKind::Text,
    }
}

/// `node.get_config`：白名单里的每个键，带当前生效值、能不能改、取值约束。`env_overridden` 判断一个键是不是被环境变量覆盖。
pub fn items(cfg: &Config, env_overridden: &dyn Fn(&str) -> bool) -> Vec<ConfigItem> {
    let root = serde_json::to_value(cfg).unwrap_or(Value::Null);
    SPECS
        .iter()
        .map(|spec| {
            let env_override = env_overridden(spec.key);
            let (mut options, mut min, mut max) = (Vec::new(), None, None);
            match spec.rule {
                Rule::Bool => {}
                Rule::Int { min: lo, max: hi } => (min, max) = (Some(lo), Some(hi)),
                Rule::Duration { min: lo, max: hi } => {
                    (min, max) = (Some(lo.as_millis() as u64), Some(hi.as_millis() as u64));
                }
                Rule::Enum(values) => options = values.iter().map(|v| (*v).to_string()).collect(),
                Rule::Text { max_chars, .. } => max = Some(max_chars),
            }
            ConfigItem {
                key: spec.key.to_string(),
                value: lookup(&root, spec.key).cloned().unwrap_or(Value::Null),
                kind: kind_of(&spec.rule),
                editable: if env_override { ConfigEditable::No } else { spec.editable },
                env_override,
                options,
                min,
                max,
            }
        })
        .collect()
}

/// 校验过、待写入的一项改动。
#[derive(Debug)]
pub struct Planned {
    pub key: &'static str,
    pub old: Value,
    pub new: Value,
    toml: toml_edit::Value,
}

fn field_error(code: ErrorCode, key: &str, reason: &str, message: String) -> RpcError {
    let mut e = RpcError::new(code, message);
    if let Some(d) = e.data.as_mut() {
        d.field = Some(key.to_string());
        d.reason = Some(reason.to_string());
    }
    e
}

fn forbidden(key: &str, reason: &str, message: &str) -> RpcError {
    field_error(ErrorCode::Forbidden, key, reason, format!("{key}: {message}"))
}

fn invalid(key: &str, reason: &str, message: String) -> RpcError {
    field_error(ErrorCode::Invalid, key, reason, format!("{key}: {message}"))
}

/// 按规则检查一个新值；通过时返回（规范化后的 JSON 值，要写进 TOML 的值）。出错返回（`data.reason`，英文说明）。
fn check(rule: &Rule, value: &Value) -> Result<(Value, toml_edit::Value), (&'static str, String)> {
    match rule {
        Rule::Bool => {
            let b = value.as_bool().ok_or(("type", "expected true or false".to_string()))?;
            Ok((json!(b), toml_edit::Value::from(b)))
        }
        Rule::Int { min, max } => {
            let n = value.as_u64().ok_or(("type", "expected a whole number".to_string()))?;
            if !(*min..=*max).contains(&n) {
                return Err(("range", format!("must be between {min} and {max}")));
            }
            Ok((json!(n), toml_edit::Value::from(n as i64)))
        }
        Rule::Duration { min, max } => {
            let s = value.as_str().ok_or(("type", "expected a duration string such as \"20m\"".to_string()))?.trim();
            if s.len() > 32 {
                return Err(("format", "is too long".to_string()));
            }
            let d = parse_duration(s).map_err(|e| ("format", e))?;
            if d < *min || d > *max {
                return Err(("range", format!("must be between {}ms and {}ms", min.as_millis(), max.as_millis())));
            }
            Ok((json!(s), toml_edit::Value::from(s)))
        }
        Rule::Enum(values) => {
            let s = value.as_str().ok_or(("type", "expected a string".to_string()))?;
            if !values.contains(&s) {
                return Err(("format", format!("expected one of: {}", values.join(" | "))));
            }
            Ok((json!(s), toml_edit::Value::from(s)))
        }
        Rule::Text { max_chars, check: valid } => {
            let s = value.as_str().ok_or(("type", "expected a string".to_string()))?;
            if s.chars().count() as u64 > *max_chars {
                return Err(("range", format!("at most {max_chars} characters")));
            }
            valid(s).map_err(|e| ("format", e))?;
            Ok((json!(s), toml_edit::Value::from(s)))
        }
    }
}

/// 整体校验一次 `node.set_config`，不碰磁盘：任何一项出问题就返回错误（一项都不写）。按请求里的顺序检查，报第一个问题。
/// 返回真正有变化的项（和当前值相同的改动是空操作，不写文件、不进审计）。
///
/// - 键重复、`changes` 为空、值的类型或取值不对 → `invalid`（`data.field` 是键，`data.reason` 是 `duplicate` / `type` / `range` / `format`）
/// - 键不在白名单（`not_editable`）、被环境变量覆盖（`env_override`）、想放宽只能收紧的键（`tighten_only`）→ `forbidden`
pub fn plan(
    cfg: &Config,
    changes: &[ConfigChange],
    env_overridden: &dyn Fn(&str) -> bool,
) -> Result<Vec<Planned>, RpcError> {
    if changes.is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "nothing to change: give at least one change"));
    }
    let root = serde_json::to_value(cfg).unwrap_or(Value::Null);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for ch in changes {
        if !seen.insert(ch.key.as_str()) {
            return Err(invalid(&ch.key, "duplicate", "appears more than once".into()));
        }
        let Some(spec) = SPECS.iter().find(|s| s.key == ch.key) else {
            return Err(forbidden(&ch.key, "not_editable", "this key cannot be changed remotely"));
        };
        if env_overridden(spec.key) {
            return Err(forbidden(
                spec.key,
                "env_override",
                "is set by an environment variable, which overrides config.toml",
            ));
        }
        let (new, toml) = check(&spec.rule, &ch.value).map_err(|(reason, msg)| invalid(spec.key, reason, msg))?;
        let old = lookup(&root, spec.key).cloned().unwrap_or(Value::Null);
        // 只能收紧的开关：新值是 `false`，或者和现在一样（空操作），才放行
        if spec.editable == ConfigEditable::TightenOnly && new != json!(false) && new != old {
            return Err(forbidden(
                spec.key,
                "tighten_only",
                "can only be turned off remotely; turn it on with `anycon config set` on the node",
            ));
        }
        if new != old {
            out.push(Planned { key: spec.key, old, new, toml });
        }
    }
    Ok(out)
}

/// 同一台 daemon 上的 `node.set_config` 一次只处理一个：「读当前值 → 校验 → 写 → 热加载 → 审计」是一个整体，
/// 并发的两次不会拿着过时的旧值互相覆盖（文件本身另有进程内锁 + `config.toml.lock` 保证每次读-改-写是原子的）。
static SET_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 重读 `config.toml`。figment 解析是同步的，放进阻塞线程。
async fn reload(config: &Arc<ConfigStore>) -> Result<bool, RpcError> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || config.reload())
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot reload the config: {e}")))?
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot reload the config: {e}")))
}

/// `node.get_config`。
pub fn get(c: &Ctx) -> NodeConfigResult {
    NodeConfigResult { items: items(&c.config.get(), &process_env_overrides) }
}

/// `node.set_config`：校验全部改动 → 写 `config.toml` → 热加载 → 审计 → 返回新的配置项列表。
pub async fn set(c: &Ctx, caller: &Caller, p: NodeSetConfigParams) -> Result<NodeConfigResult, RpcError> {
    let _one_at_a_time = SET_LOCK.lock().await;
    // 先让内存里的配置和磁盘一致：`anycon config set` 可能刚改过文件，热加载轮询还没到。旧值、「只能收紧」的判断都以此为准
    reload(&c.config).await?;
    let cfg = c.config.get();
    let planned = plan(&cfg, &p.changes, &process_env_overrides)?;
    if planned.is_empty() {
        return Ok(NodeConfigResult { items: items(&cfg, &process_env_overrides) });
    }
    let edits = planned.iter().map(|x| (x.key.to_string(), x.toml.clone())).collect();
    crate::config::set_toml_values_blocking(&c.paths, edits).await.map_err(|e| match e {
        // 整份配置的校验没过：上面逐项校验的规则比 `Config::validate` 窄，走到这里说明有遗漏，按参数不合法回
        ConfigError::Invalid(m) | ConfigError::Parse(m) => RpcError::new(ErrorCode::Invalid, m),
        ConfigError::Io(e) => RpcError::new(ErrorCode::Internal, format!("cannot write config.toml: {e}")),
    })?;
    // 和 `anycon config set`、`node.set_enroll_enabled` 同一条热加载路径：重读文件，订阅者（日志级别、预览、agent 列表……）各自生效
    reload(&c.config).await?;
    for x in &planned {
        c.audit.record(caller, "config.changed", Some(x.key), json!({ "old": x.old, "new": x.new })).await;
    }
    // 只记键名，不记值
    tracing::info!(keys = ?planned.iter().map(|x| x.key).collect::<Vec<_>>(), device = %caller.device_id, "config changed by a client");
    Ok(NodeConfigResult { items: items(&c.config.get(), &process_env_overrides) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> bool {
        false
    }

    fn ch(key: &str, value: Value) -> ConfigChange {
        ConfigChange { key: key.into(), value }
    }

    fn err_of(r: Result<Vec<Planned>, RpcError>) -> (ErrorCode, Option<String>, Option<String>) {
        let e = r.expect_err("expected an error");
        let d = e.data.expect("error data");
        (d.code, d.field, d.reason)
    }

    #[test]
    fn every_whitelisted_key_is_hot_and_exists_in_the_config() {
        let root = serde_json::to_value(Config::default()).unwrap();
        for key in whitelisted_keys() {
            assert!(
                crate::config::is_hot_key(key),
                "{key} is not hot-reloadable, so it cannot be changed without a restart"
            );
            assert!(lookup(&root, key).is_some(), "{key} is not a config key");
        }
    }

    #[test]
    fn the_whitelist_leaves_out_everything_that_widens_the_attack_surface() {
        let keys: Vec<_> = whitelisted_keys().collect();
        for k in &keys {
            for banned in ["agents", "workspace", "hub", "enroll"] {
                assert!(!k.starts_with(&format!("{banned}.")), "{k}");
            }
        }
        for banned in [
            "permissions.allow_session_env",
            "permissions.session_env_allow",
            "preview.allow_ports",
            "sessions.max_attachment_bytes",
            "sessions.history_retention",
        ] {
            assert!(!keys.contains(&banned), "{banned}");
        }
    }

    #[test]
    fn items_report_current_values_constraints_and_editability() {
        let mut cfg = Config::default();
        cfg.sessions.max_concurrent = 5;
        cfg.permissions.allow_dangerous_modes = false;
        let list = items(&cfg, &no_env);
        assert_eq!(list.len(), SPECS.len());
        let get = |k: &str| list.iter().find(|i| i.key == k).unwrap();

        let c = get("sessions.max_concurrent");
        assert_eq!(
            (c.value.clone(), c.kind, c.editable, c.env_override),
            (json!(5), ConfigValueKind::Int, ConfigEditable::Any, false)
        );
        assert_eq!((c.min, c.max), (Some(1), Some(32)));

        let t = get("preview.ticket_ttl");
        assert_eq!((t.value.clone(), t.kind), (json!("24h"), ConfigValueKind::Duration));
        assert_eq!((t.min, t.max), (Some(5 * 60_000), Some(7 * 24 * 3_600_000)));

        let n = get("notify.turn_finished");
        assert_eq!(n.kind, ConfigValueKind::Enum);
        assert_eq!(n.options, vec!["always", "when_unwatched", "never"]);

        let d = get("permissions.allow_dangerous_modes");
        assert_eq!((d.value.clone(), d.editable), (json!(false), ConfigEditable::TightenOnly));
        assert_eq!(get("preview.enabled").editable, ConfigEditable::TightenOnly);

        let p = get("worktree.branch_prefix");
        assert_eq!((p.value.clone(), p.kind, p.max), (json!("anycon"), ConfigValueKind::Text, Some(40)));
        assert_eq!(get("sessions.idle_timeout").min, Some(0));
    }

    #[test]
    fn an_environment_override_makes_the_item_read_only() {
        let cfg = Config::default();
        let overridden = |k: &str| k == "sessions.max_concurrent" || k == "preview.enabled";
        let list = items(&cfg, &overridden);
        for i in &list {
            let want = i.key == "sessions.max_concurrent" || i.key == "preview.enabled";
            assert_eq!(i.env_override, want, "{}", i.key);
            if want {
                assert_eq!(i.editable, ConfigEditable::No, "{}", i.key);
            }
        }
        let e = plan(&cfg, &[ch("sessions.max_concurrent", json!(2))], &overridden);
        assert_eq!(
            err_of(e),
            (ErrorCode::Forbidden, Some("sessions.max_concurrent".into()), Some("env_override".into()))
        );
        // 收紧也不行：环境变量盖着，写进文件不生效
        let e = plan(&cfg, &[ch("preview.enabled", json!(false))], &overridden);
        assert_eq!(err_of(e).2.as_deref(), Some("env_override"));
    }

    #[test]
    fn keys_outside_the_whitelist_are_forbidden() {
        let cfg = Config::default();
        for key in [
            "agents.claude-code.command",
            "agents.env",
            "workspace.root",
            "workspace.link_roots",
            "hub.url",
            "enroll.approval",
            "permissions.allow_session_env",
            "permissions.session_env_allow",
            "preview.allow_ports",
            "sessions.max_attachment_bytes",
            "sessions.history_retention",
            "no.such.key",
        ] {
            let e = plan(&cfg, &[ch(key, json!(true))], &no_env);
            assert_eq!(err_of(e), (ErrorCode::Forbidden, Some(key.into()), Some("not_editable".into())), "{key}");
        }
    }

    #[test]
    fn the_two_security_switches_can_only_be_turned_off() {
        let mut cfg = Config::default();
        for key in ["permissions.allow_dangerous_modes", "preview.enabled"] {
            // true -> false：放行
            cfg.permissions.allow_dangerous_modes = true;
            cfg.preview.enabled = true;
            let p = plan(&cfg, &[ch(key, json!(false))], &no_env).unwrap();
            assert_eq!((p.len(), p[0].old.clone(), p[0].new.clone()), (1, json!(true), json!(false)));
            // 已经是 true 再给 true：空操作
            assert!(plan(&cfg, &[ch(key, json!(true))], &no_env).unwrap().is_empty());
            // false -> true：放宽，拒绝
            cfg.permissions.allow_dangerous_modes = false;
            cfg.preview.enabled = false;
            let e = plan(&cfg, &[ch(key, json!(true))], &no_env);
            assert_eq!(err_of(e), (ErrorCode::Forbidden, Some(key.into()), Some("tighten_only".into())));
            // false -> false：空操作
            assert!(plan(&cfg, &[ch(key, json!(false))], &no_env).unwrap().is_empty());
        }
    }

    #[test]
    fn values_are_checked_against_each_keys_own_rule() {
        let cfg = Config::default();
        let bad = |key: &str, v: Value| err_of(plan(&cfg, &[ch(key, v)], &no_env));
        let inv = |key: &str, reason: &str| (ErrorCode::Invalid, Some(key.to_string()), Some(reason.to_string()));

        // 整数：1–32，必须是整数
        assert_eq!(bad("sessions.max_concurrent", json!(0)), inv("sessions.max_concurrent", "range"));
        assert_eq!(bad("sessions.max_concurrent", json!(33)), inv("sessions.max_concurrent", "range"));
        assert_eq!(bad("sessions.max_concurrent", json!(-1)), inv("sessions.max_concurrent", "type"));
        assert_eq!(bad("sessions.max_concurrent", json!(2.5)), inv("sessions.max_concurrent", "type"));
        assert_eq!(bad("sessions.max_concurrent", json!("3")), inv("sessions.max_concurrent", "type"));
        assert!(plan(&cfg, &[ch("sessions.max_concurrent", json!(1))], &no_env).is_ok());
        assert!(plan(&cfg, &[ch("sessions.max_concurrent", json!(32))], &no_env).is_ok());

        // 时长
        assert_eq!(bad("sessions.idle_timeout", json!("soon")), inv("sessions.idle_timeout", "format"));
        assert_eq!(bad("sessions.idle_timeout", json!("")), inv("sessions.idle_timeout", "format"));
        assert_eq!(bad("sessions.idle_timeout", json!("8d")), inv("sessions.idle_timeout", "range"));
        assert_eq!(bad("sessions.idle_timeout", json!(60)), inv("sessions.idle_timeout", "type"));
        assert_eq!(bad("preview.ticket_ttl", json!("1m")), inv("preview.ticket_ttl", "range"));
        assert_eq!(bad("preview.ticket_ttl", json!("8d")), inv("preview.ticket_ttl", "range"));
        for ok in ["5m", "7d", "24h", "90m"] {
            assert!(plan(&cfg, &[ch("preview.ticket_ttl", json!(ok))], &no_env).is_ok(), "{ok}");
        }
        // 0 = 不限：这两个键允许
        assert!(plan(&cfg, &[ch("sessions.idle_timeout", json!("0"))], &no_env).is_ok());
        assert!(plan(&cfg, &[ch("permissions.request_timeout", json!("0"))], &no_env).is_ok());

        // 枚举
        assert_eq!(bad("notify.turn_finished", json!("sometimes")), inv("notify.turn_finished", "format"));
        assert_eq!(bad("worktree.default", json!("WORKTREE")), inv("worktree.default", "format"));
        assert_eq!(bad("log.level", json!("INFO")), inv("log.level", "format"));
        assert_eq!(bad("log.level", json!(3)), inv("log.level", "type"));

        // 布尔
        assert_eq!(
            bad("worktree.auto_archive_on_pr_close", json!("true")),
            inv("worktree.auto_archive_on_pr_close", "type")
        );
        assert_eq!(
            bad("worktree.auto_archive_on_pr_close", json!(1)),
            inv("worktree.auto_archive_on_pr_close", "type")
        );

        // 文本：沿用 `validate_branch_prefix` 的规则
        assert_eq!(bad("worktree.branch_prefix", json!("main")), inv("worktree.branch_prefix", "format"));
        assert_eq!(bad("worktree.branch_prefix", json!("has space")), inv("worktree.branch_prefix", "format"));
        assert_eq!(bad("worktree.branch_prefix", json!("a".repeat(41))), inv("worktree.branch_prefix", "range"));
        assert_eq!(bad("worktree.branch_prefix", json!(null)), inv("worktree.branch_prefix", "type"));
        assert!(plan(&cfg, &[ch("worktree.branch_prefix", json!(""))], &no_env).is_ok());
        assert!(
            plan(&cfg, &[ch("worktree.branch_prefix", json!("team/x"))], &no_env).is_err(),
            "'/' is not in the allowed set"
        );
        assert!(plan(&cfg, &[ch("worktree.branch_prefix", json!("123"))], &no_env).is_ok());
    }

    #[test]
    fn a_request_is_all_or_nothing_and_reports_the_first_problem() {
        let cfg = Config::default();
        let e = plan(
            &cfg,
            &[ch("sessions.max_concurrent", json!(4)), ch("log.level", json!("loud")), ch("agents.env", json!("x"))],
            &no_env,
        );
        assert_eq!(err_of(e), (ErrorCode::Invalid, Some("log.level".into()), Some("format".into())));

        let e = plan(&cfg, &[ch("log.level", json!("debug")), ch("log.level", json!("info"))], &no_env);
        assert_eq!(err_of(e), (ErrorCode::Invalid, Some("log.level".into()), Some("duplicate".into())));

        let e = plan(&cfg, &[], &no_env);
        assert_eq!(err_of(e).0, ErrorCode::Invalid);
    }

    #[test]
    fn unchanged_values_are_dropped_from_the_plan() {
        let cfg = Config::default();
        let p = plan(
            &cfg,
            &[
                ch("sessions.max_concurrent", json!(6)),
                ch("log.level", json!("debug")),
                ch("sessions.idle_timeout", json!("20m")),
            ],
            &no_env,
        )
        .unwrap();
        assert_eq!(p.iter().map(|x| x.key).collect::<Vec<_>>(), vec!["log.level"]);
        assert_eq!((p[0].old.clone(), p[0].new.clone()), (json!("info"), json!("debug")));
    }

    #[test]
    fn durations_are_trimmed_before_they_are_stored() {
        let cfg = Config::default();
        let p = plan(&cfg, &[ch("sessions.idle_timeout", json!("  45m "))], &no_env).unwrap();
        assert_eq!(p[0].new, json!("45m"));
    }
}
