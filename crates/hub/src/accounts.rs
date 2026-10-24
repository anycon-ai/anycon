//! 账号规则：谁是有效管理员、自助注册（建账号）的共享流程、各处共用的登录方式清单。
//!
//! 这些规则只有一份实现，HTTP 各路由（邮箱验证码、OAuth、邀请、管理页）都调这里，不在别处各写一遍：
//!
//! - **有效管理员**：库里的 `accounts.is_admin` 标记，或者 `multi` 模式下账号邮箱在 `ANYCON_ADMIN_EMAILS` 里
//!   （[`is_effective_admin`]）。配置派生的管理员**不写回**库里的标记——改配置立刻生效，删掉邮箱就失去权限。
//! - **注册**（[`sign_up`]）：`multi` 里第一次邮箱验证码登录建账号走它：显示名 + 可选的已验证邮箱，
//!   唯一邮箱竞争（并发注册同一个邮箱）输了的一方拿到已有账号，写 `account_created` 审计。OAuth 首次登录要同时绑身份，
//!   建账号和绑身份在库里是一个事务（[`crate::oauth::login`]），但显示名（[`signup_display_name`]）和审计
//!   （[`audit_account_created`]）用的是同一份。
//! - **登录方式**（[`config_login_methods`]、[`account_login_methods`]）：`/api/config` 和 `/api/me` 用，
//!   解绑邮箱 / 解除 OAuth 绑定 / 删登录 token 时也靠它判断「是不是最后一种登录方式」。顺序固定：`github`、`google`、`email`、`token`。
//! - **移除登录方式**（[`begin_login_method_removal`]）：解绑邮箱、解除 OAuth 绑定、删登录 token 都是「检查还剩别的方式 → 删」，
//!   不是一条 SQL。三者共用一把按账号的锁，并且在锁里**重新读账号**再算登录方式，所以并发的两次移除不会互相指望对方还在，
//!   请求开始时取的账号快照（邮箱可能已经被另一个请求解绑）也不会被拿来判断。

use std::hash::{Hash, Hasher};

use tokio::sync::MutexGuard;

use crate::app::AppState;
use crate::config::{Config, Mode, OAuthProvider};
use crate::error::{ApiError, ApiResult};
use crate::http::session::Client;
use crate::store::{Account, CreateAccountOutcome, NewAudit, StoreResult};
use crate::util::{clip, ip_prefix, new_id, now_ms};

/// 显示名的最大长度（字符）。
const DISPLAY_NAME_MAX: usize = 64;
/// [`LoginMethodLocks`] 的锁条数。
const LOCK_STRIPES: usize = 64;

// ---------------------------------------------------------------------------
// 管理员
// ---------------------------------------------------------------------------

/// 管理员身份来自配置：`multi` 模式，且账号邮箱在 `ANYCON_ADMIN_EMAILS` 里。
pub fn admin_by_config(config: &Config, account: &Account) -> bool {
    config.mode == Mode::Multi && account.email.as_deref().is_some_and(|e| config.is_admin_email(e))
}

/// **有效管理员**：库里的标记，或者 [`admin_by_config`]。授权判断（`Authed`）和管理页展示都用这一个函数。
pub fn is_effective_admin(config: &Config, account: &Account) -> bool {
    account.is_admin || admin_by_config(config, account)
}

/// 是不是 `single` 的 bootstrap 管理员账号（它的 token 在配置里，不在库里，删了就没法再登录管理这个 hub）。`multi` 没有。
/// 这是唯一的判断：删账号、取消管理员、登录方式清单都用它。
pub fn is_bootstrap_admin(state: &AppState, account_id: &str) -> bool {
    state.admin.as_ref().is_some_and(|admin| admin.account_id == account_id)
}

// ---------------------------------------------------------------------------
// 登录方式
// ---------------------------------------------------------------------------

/// `/api/config.login_methods`：hub 提供的登录方式。`github`、`google`（配了凭据）、`email`（配了 SMTP）、
/// `token`（只有 `single`）。
pub fn config_login_methods(state: &AppState) -> Vec<&'static str> {
    let mut methods: Vec<&'static str> = state.config.oauth_providers().map(OAuthProvider::as_str).collect();
    if state.email_enabled() {
        methods.push("email");
    }
    if state.config.mode == Mode::Single {
        methods.push("token");
    }
    methods
}

/// `/api/me.login_methods`：这个账号现在能用来登录的方式，顺序同 [`config_login_methods`]。
///
/// - `github` / `google`：账号绑了这个提供方的身份，而且 hub 配了它（[`linked_oauth_methods`]）；
/// - `email`：绑了邮箱，而且 hub 配了 SMTP；
/// - `token`：只有 `single`——bootstrap 管理员，或者自己建过登录 token。
///
/// 可能是空的（邀请进来、还没建 token、没绑邮箱）。
pub async fn account_login_methods(state: &AppState, account: &Account) -> StoreResult<Vec<&'static str>> {
    let mut methods = linked_oauth_methods(state, &account.id).await?;
    if account.email.is_some() && state.email_enabled() {
        methods.push("email");
    }
    if state.config.mode == Mode::Single
        && (is_bootstrap_admin(state, &account.id) || !state.store.list_account_tokens(&account.id).await?.is_empty())
    {
        methods.push("token");
    }
    Ok(methods)
}

/// 账号已绑定、且 hub 配了的 OAuth 提供方（`github`、`google`，按这个顺序）。
/// 提供方不在配置里了的身份留在库里，但不算登录方式（登不进来）。
async fn linked_oauth_methods(state: &AppState, account_id: &str) -> StoreResult<Vec<&'static str>> {
    let identities = state.store.list_identities(account_id).await?;
    Ok(state
        .config
        .oauth_providers()
        .map(OAuthProvider::as_str)
        .filter(|p| identities.iter().any(|i| i.provider == *p))
        .collect())
}

// ---------------------------------------------------------------------------
// 移除登录方式
// ---------------------------------------------------------------------------

/// 「移除一种登录方式」的串行化：每个账号同一时刻只有一个这样的操作在「检查 → 删除」之间。
///
/// 按账号 id 哈希到固定条数的锁上（不随账号数增长，不用清理）；两个不同账号撞到同一条锁只是偶尔多等一会儿。
#[derive(Debug)]
pub struct LoginMethodLocks {
    stripes: Box<[tokio::sync::Mutex<()>]>,
}

impl Default for LoginMethodLocks {
    fn default() -> Self {
        Self { stripes: (0..LOCK_STRIPES).map(|_| tokio::sync::Mutex::new(())).collect() }
    }
}

impl LoginMethodLocks {
    /// 拿这个账号的锁，持有到返回值 drop。
    pub async fn lock(&self, account_id: &str) -> MutexGuard<'_, ()> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        account_id.hash(&mut hasher);
        self.stripes[(hasher.finish() % LOCK_STRIPES as u64) as usize].lock().await
    }
}

/// 一次「移除登录方式」的进行中状态：持有账号的锁，并带着在锁里重新读出来的账号和它此刻的登录方式。
/// 判断完、删完之后 drop 它。
pub struct LoginMethodRemoval<'a> {
    _guard: MutexGuard<'a, ()>,
    /// 锁里从库里重新读的账号（不是请求开始时 `Authed` 里的快照：邮箱可能已经被并发的请求解绑）。
    pub account: Account,
    /// 此刻能用来登录的方式（[`account_login_methods`]）。
    pub methods: Vec<&'static str>,
}

impl LoginMethodRemoval<'_> {
    /// 去掉 `method` 之后这个账号是不是一种能登录的方式都不剩。`method` 本来就不算登录方式（比如提供方已经不在配置里
    /// 的身份）时，去掉它不改变什么，返回 `false`。
    pub fn would_lock_out(&self, method: &str) -> bool {
        self.methods.contains(&method) && self.methods.iter().all(|m| *m == method)
    }
}

/// 开始移除账号的一种登录方式（解绑邮箱、解除 OAuth 绑定、删登录 token）：拿账号的锁，重新读账号，算它此刻的登录方式。
/// 账号已经不存在（刚被删）→ `401`。
pub async fn begin_login_method_removal<'a>(
    state: &'a AppState,
    account_id: &str,
) -> ApiResult<LoginMethodRemoval<'a>> {
    let guard = state.login_method_locks.lock(account_id).await;
    let account = state.store.get_account(account_id).await?.ok_or_else(ApiError::unauthorized)?;
    let methods = account_login_methods(state, &account).await?;
    Ok(LoginMethodRemoval { _guard: guard, account, methods })
}

// ---------------------------------------------------------------------------
// 注册
// ---------------------------------------------------------------------------

/// 一次自助注册的参数。
#[derive(Debug, Clone, Copy)]
pub struct SignUp<'a> {
    /// 审计 `detail.method`：`email`、`github`、`google` 或 `invite`。
    pub method: &'static str,
    /// 显示名候选，按优先级（比如 GitHub 的 `[name, login]`）；空白的跳过。都没有就用邮箱 `@` 之前的部分，
    /// 再没有就是 `user-<6 位 hex>`（[`signup_display_name`]）。
    pub names: &'a [&'a str],
    /// 已验证的邮箱（规范化后）。没有就建一个没邮箱的账号。**未验证的邮箱不要传。**
    pub email: Option<&'a str>,
}

/// [`sign_up`] 的结果。
#[derive(Debug)]
pub enum SignUpOutcome {
    /// 新建了账号（`account_created` 审计已写）。
    Created(Account),
    /// 邮箱已经属于一个账号（并发注册输了，或刚被绑定）：没建新账号，调用方登录这个账号。
    Existing(Account),
}

impl SignUpOutcome {
    pub fn account(&self) -> &Account {
        match self {
            SignUpOutcome::Created(a) | SignUpOutcome::Existing(a) => a,
        }
    }

    pub fn into_account(self) -> Account {
        match self {
            SignUpOutcome::Created(a) | SignUpOutcome::Existing(a) => a,
        }
    }
}

/// 整理显示名：去控制字符、去首尾空白、最多 64 个字符。整理完是空的返回 `None`。
fn clean_display_name(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let clipped = clip(cleaned.trim(), DISPLAY_NAME_MAX);
    let name = clipped.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 注册时的显示名：第一个整理后非空的候选；没有就取邮箱 `@` 之前的部分；再没有就是 `user-<6 位 hex>`。
pub fn signup_display_name(names: &[&str], email: Option<&str>) -> String {
    names
        .iter()
        .find_map(|n| clean_display_name(n))
        .or_else(|| email.and_then(|e| e.split('@').next()).and_then(clean_display_name))
        .unwrap_or_else(|| format!("user-{}", &new_id()[..6]))
}

/// 写 `account_created` 审计（`detail.method`）。邀请兑换在库里建账号，不经过 [`sign_up`]，所以单独暴露。
pub async fn audit_account_created(state: &AppState, client: &Client, account_id: &str, method: &'static str) {
    state
        .audit(NewAudit {
            event: "account_created",
            account_id: Some(account_id.to_string()),
            device_id: None,
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            user_agent: Some(clip(&client.user_agent, 300)),
            detail: Some(serde_json::json!({ "method": method })),
        })
        .await;
}

/// 自助注册：建账号（显示名 + 可选的已验证邮箱），写 `account_created` 审计。
///
/// 邮箱的唯一索引是并发注册同一个邮箱时的裁判：输了的一方拿到 [`SignUpOutcome::Existing`]，不报错、不重复建，
/// 调用方登录那个账号就行。调用方负责在这之前已经确认过：这个邮箱 / 身份确实是被验证过的（邮箱验证码、
/// OAuth 提供方说它已验证），以及 `multi` 模式（`single` 是邀请制，不该走这里）。
pub async fn sign_up(state: &AppState, client: &Client, req: SignUp<'_>) -> ApiResult<SignUpOutcome> {
    let name = signup_display_name(req.names, req.email);
    // 输掉竞争后查已有账号，理论上可能正好撞上它被删 / 换邮箱，再试几次；不会循环下去。
    for _ in 0..3 {
        match state.store.create_account_with_email(&name, req.email, now_ms()).await? {
            CreateAccountOutcome::Created(account) => {
                audit_account_created(state, client, &account.id, req.method).await;
                return Ok(SignUpOutcome::Created(account));
            }
            CreateAccountOutcome::EmailTaken => {
                if let Some(owner) = match req.email {
                    Some(email) => state.store.find_account_by_email(email).await?,
                    None => None,
                } {
                    return Ok(SignUpOutcome::Existing(owner));
                }
            }
        }
    }
    Err(ApiError::internal("cannot create or find the account for a sign-up"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(email: Option<&str>, is_admin: bool) -> Account {
        Account {
            id: "a1".into(),
            display_name: "A".into(),
            email: email.map(str::to_string),
            is_admin,
            created_at_ms: 0,
        }
    }

    fn config(mode: Mode, admin_emails: &[&str]) -> Config {
        let mut c = Config::new("https://hub.test", "/nonexistent").unwrap();
        c.mode = mode;
        c.admin_emails = admin_emails.iter().map(|e| e.to_string()).collect();
        c
    }

    #[test]
    fn effective_admin_is_the_db_flag_or_a_configured_email_in_multi_only() {
        let multi = config(Mode::Multi, &["boss@example.com"]);
        let single = config(Mode::Single, &["boss@example.com"]);

        // 库里的标记在两种模式下都算
        assert!(is_effective_admin(&multi, &account(None, true)));
        assert!(is_effective_admin(&single, &account(None, true)));
        // 配置的邮箱：只有 multi
        let boss = account(Some("boss@example.com"), false);
        assert!(is_effective_admin(&multi, &boss) && admin_by_config(&multi, &boss));
        assert!(!is_effective_admin(&single, &boss) && !admin_by_config(&single, &boss));
        // 别的邮箱、没邮箱：不是
        assert!(!is_effective_admin(&multi, &account(Some("other@example.com"), false)));
        assert!(!is_effective_admin(&multi, &account(None, false)));
        // 同时有标记和配置：有效，admin_by_config 也为真（管理页据此显示「由配置授予」）
        let both = account(Some("boss@example.com"), true);
        assert!(is_effective_admin(&multi, &both) && admin_by_config(&multi, &both));
        // 没配管理员邮箱：谁都不是
        assert!(!is_effective_admin(&config(Mode::Multi, &[]), &boss));
    }

    #[test]
    fn display_names_are_cleaned_clipped_and_fall_back() {
        // 候选按顺序，空白的跳过
        assert_eq!(signup_display_name(&["  Ada Lovelace ", "ada"], Some("x@y.z")), "Ada Lovelace");
        assert_eq!(signup_display_name(&["", "   ", "ada"], None), "ada");
        // 没有候选：邮箱本地部分
        assert_eq!(signup_display_name(&[], Some("grace.hopper@example.com")), "grace.hopper");
        // 控制字符去掉（换行、NUL），不能让显示名里带控制字符
        assert_eq!(signup_display_name(&["a\nb\u{0}c"], None), "abc");
        assert_eq!(signup_display_name(&["\u{7}\n"], Some("zed@example.com")), "zed");
        // 最多 64 个字符（按字符，不是字节），剪完再去一次尾部空白
        let long = "é".repeat(100);
        assert_eq!(signup_display_name(&[&long], None).chars().count(), 64);
        let padded = format!("{} tail", "x".repeat(63));
        assert_eq!(signup_display_name(&[&padded], None), "x".repeat(63));
        // 什么都没有：user-<6 hex>
        for name in [signup_display_name(&[], None), signup_display_name(&[" "], Some("@example.com"))] {
            assert!(name.starts_with("user-") && name.len() == 11, "{name}");
            assert!(name[5..].bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
        }
    }
}
