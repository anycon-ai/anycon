//! 存储抽象。hub 的其余部分只依赖 [`Store`] trait，v0.1 的实现是 [`SqliteStore`]（Postgres 以后再加）。
//!
//! 表结构见 `migrations/sqlite/` 和 docs/spec/hub.md § 8：全是元数据，没有任何会话内容。
//! 所有 token / 邀请码 / cookie 值都只以哈希形式出现在这里（见 [`crate::util::hash_token`]）。

use anycon_protocol::{DeviceKind, NotifyCategory};
use async_trait::async_trait;
use serde::Serialize;

mod oauth;
mod sqlite;

pub use oauth::{
    Identity, IdentityAccountOutcome, LinkIdentityOutcome, NewIdentityAccount, NewOAuthState, OAuthPurpose, OAuthState,
};
pub use sqlite::SqliteStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage backend error: {0}")]
    Backend(String),
    #[error("migration error: {0}")]
    Migrate(String),
    #[error("invalid stored value: {0}")]
    Corrupt(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountToken {
    pub id: String,
    pub label: String,
    pub created_at_ms: u64,
    pub last_used_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WebSession {
    pub id: String,
    pub account_id: String,
    pub user_agent: String,
    pub ip_prefix: String,
    pub created_at_ms: u64,
    pub last_seen_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Invite {
    pub id: String,
    pub created_by: String,
    pub note: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub used_by: Option<String>,
    pub used_at_ms: Option<u64>,
}

/// 兑换邀请的结果。
#[derive(Debug)]
pub enum RedeemOutcome {
    Redeemed(Account),
    NotFound,
    Expired,
    Used,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub account_id: String,
    pub kind: DeviceKind,
    pub name: String,
    pub platform: String,
    pub origin: Option<String>,
    pub created_at_ms: u64,
    pub last_seen_ms: Option<u64>,
    pub revoked_at_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct NewDevice {
    pub id: String,
    pub account_id: String,
    pub kind: DeviceKind,
    pub name: String,
    pub platform: String,
    pub origin: Option<String>,
    pub token_hash: String,
}

#[derive(Debug, Clone, Default)]
pub struct NewAudit {
    pub event: &'static str,
    pub account_id: Option<String>,
    pub device_id: Option<String>,
    pub ip_prefix: Option<String>,
    pub user_agent: Option<String>,
    /// 只放元数据（方式、对端设备 id……），绝不放任何内容或秘密。
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts_ms: u64,
    pub event: String,
    pub account_id: Option<String>,
    pub device_id: Option<String>,
    pub ip_prefix: Option<String>,
    pub user_agent: Option<String>,
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StoreStats {
    pub accounts: u64,
    pub devices: u64,
    pub db_size_bytes: u64,
}

/// 一条预览令牌（只有哈希，ADR-39）。`Debug` 不输出哈希。
#[derive(Clone)]
pub struct PreviewToken {
    pub token_hash: String,
    pub label: String,
    /// label 绑定的节点（daemon）。
    pub node_id: String,
    pub account_id: String,
    /// 签发它的设备。
    pub device_id: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for PreviewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewToken")
            .field("node_id", &self.node_id)
            .field("device_id", &self.device_id)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

/// 签发一条预览令牌的参数（`created_at_ms` 由 [`Store::issue_preview_token`] 的 `now_ms` 给）。
#[derive(Clone)]
pub struct NewPreviewToken {
    pub token_hash: String,
    pub label: String,
    pub node_id: String,
    pub account_id: String,
    pub device_id: String,
    pub expires_at_ms: u64,
}

/// 签发预览令牌时的上限（docs/spec/hub.md § 13.3）。
#[derive(Debug, Clone, Copy)]
pub struct PreviewIssueLimits {
    /// 一个节点最多绑定几个 label。
    pub labels_per_node: u32,
    /// 一台设备未过期的令牌最多几个，超过时删掉最旧的。
    pub tokens_per_device: u32,
}

/// [`Store::issue_preview_token`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueOutcome {
    Issued,
    /// label 已经绑定到别的节点。
    LabelTaken,
    /// label 是新的，但这个节点已经绑满了。
    TooManyLabels,
}

/// 邮箱验证码的用途：`Login` 用来登录，`Bind` 用来给已登录账号绑定 / 换绑邮箱。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmailPurpose {
    Login,
    Bind,
}

impl EmailPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            EmailPurpose::Login => "login",
            EmailPurpose::Bind => "bind",
        }
    }
}

/// 库里的一条验证码（只有哈希）。`Debug` 不输出邮箱和哈希。
///
/// 同一组（邮箱，用途，账号）里可以同时有几条有效的码（[`Store::put_email_code`]）：登录码一组 = 一个邮箱，
/// 绑定码一组 = （发起绑定的账号，邮箱）。
#[derive(Clone)]
pub struct EmailCode {
    /// 规范化后的邮箱。
    pub email: String,
    pub purpose: EmailPurpose,
    /// `Bind` 时是发起绑定的账号；`Login` 时为 `None`。
    pub account_id: Option<String>,
    pub code_hash: String,
    pub expires_at_ms: u64,
    /// 这一条码已经被尝试了几次。
    pub attempts: u32,
    pub created_at_ms: u64,
}

impl std::fmt::Debug for EmailCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailCode")
            .field("purpose", &self.purpose)
            .field("attempts", &self.attempts)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

/// [`Store::attempt_email_codes`] 交回来的一条码：刚被计过一次尝试的那一行。`id` 用来认领（[`Store::take_email_code`]）。
/// `Debug` 不输出哈希。
#[derive(Clone)]
pub struct CodeCandidate {
    pub id: i64,
    pub code_hash: String,
    /// 算上这一次提交，这条码已经被尝试了几次。
    pub attempts: u32,
}

impl std::fmt::Debug for CodeCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeCandidate").field("id", &self.id).field("attempts", &self.attempts).finish_non_exhaustive()
    }
}

/// [`Store::create_account_with_email`] 的结果。
#[derive(Debug)]
pub enum CreateAccountOutcome {
    Created(Account),
    /// 邮箱已经属于别的账号（唯一约束），什么都没写。
    EmailTaken,
}

/// 给账号设邮箱的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEmailOutcome {
    Done,
    /// 邮箱已经属于别的账号（唯一约束）。
    Taken,
}

/// 一条 Web Push 订阅（按设备存，一台设备一条）。`Debug` 不输出 endpoint 和密钥。
#[derive(Clone)]
pub struct PushSubscription {
    pub id: String,
    pub device_id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub created_at_ms: u64,
}

impl std::fmt::Debug for PushSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushSubscription")
            .field("id", &self.id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// 账号订阅列表里的一行：订阅 + 设备名。`Debug` 不输出 endpoint。
#[derive(Clone)]
pub struct PushSubscriptionRow {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    /// 只给 `Url` 解析出主机名用，不要整个打日志。
    pub endpoint: String,
    pub created_at_ms: u64,
}

impl std::fmt::Debug for PushSubscriptionRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushSubscriptionRow")
            .field("id", &self.id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// 通知偏好里的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationPref {
    pub category: NotifyCategory,
    pub push: bool,
    pub email: bool,
}

/// 新建设备时的配额上限（`multi`，来自 [`crate::config::QuotaLimits`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLimits {
    /// 账号名下未吊销的设备数上限（所有种类，含 daemon）。
    pub devices: u32,
    /// 其中 daemon 的上限。
    pub daemons: u32,
}

/// 新建设备时撞上的是哪一项上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceLimit {
    Devices,
    Daemons,
}

/// [`Store::create_devices_within_limits`] 的结果。
#[derive(Debug)]
pub enum CreateDevicesOutcome {
    /// 全部建好了（顺序同传入）。
    Created(Vec<Device>),
    /// 会超出上限：一台都没建。
    Exceeded(DeviceLimit),
}

/// 管理页搜索：`needle` 是已经去空白、转小写的查询串，`raw` 是去空白、没转小写的原串。
/// 命中条件：显示名或邮箱包含 `needle`（按 Unicode 不区分大小写），或者账号 id 正好等于 `raw`。
pub(crate) fn account_matches(account: &Account, needle: &str, raw: &str) -> bool {
    account.id == raw
        || account.display_name.to_lowercase().contains(needle)
        || account.email.as_deref().is_some_and(|e| e.to_lowercase().contains(needle))
}

pub fn category_to_str(c: NotifyCategory) -> &'static str {
    match c {
        NotifyCategory::PermissionRequested => "permission_requested",
        NotifyCategory::TurnFinished => "turn_finished",
        NotifyCategory::Error => "error",
        NotifyCategory::SessionIdle => "session_idle",
        NotifyCategory::EnrollRequested => "enroll_requested",
        NotifyCategory::Security => "security",
    }
}

pub fn category_from_str(s: &str) -> Option<NotifyCategory> {
    Some(match s {
        "permission_requested" => NotifyCategory::PermissionRequested,
        "turn_finished" => NotifyCategory::TurnFinished,
        "error" => NotifyCategory::Error,
        "session_idle" => NotifyCategory::SessionIdle,
        "enroll_requested" => NotifyCategory::EnrollRequested,
        "security" => NotifyCategory::Security,
        _ => return None,
    })
}

pub fn kind_to_str(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Daemon => "daemon",
        DeviceKind::Web => "web",
        DeviceKind::Cli => "cli",
        DeviceKind::Sdk => "sdk",
    }
}

pub fn kind_from_str(s: &str) -> StoreResult<DeviceKind> {
    Ok(match s {
        "daemon" => DeviceKind::Daemon,
        "web" => DeviceKind::Web,
        "cli" => DeviceKind::Cli,
        "sdk" => DeviceKind::Sdk,
        other => return Err(StoreError::Corrupt(format!("unknown device kind {other:?}"))),
    })
}

#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// `/readyz`：数据库可用。
    async fn ping(&self) -> StoreResult<()>;
    async fn stats(&self) -> StoreResult<StoreStats>;

    // ---- 账号 ----
    async fn create_account(&self, display_name: &str, is_admin: bool, now_ms: u64) -> StoreResult<Account>;
    /// 建一个普通账号（非管理员），可以直接带一个**已验证**的邮箱（规范化后的）：自助注册（`multi`）用它。
    /// 邮箱已被占用 → [`CreateAccountOutcome::EmailTaken`]，什么都不写——并发的两次注册靠唯一索引分出胜负。
    async fn create_account_with_email(
        &self,
        display_name: &str,
        email: Option<&str>,
        now_ms: u64,
    ) -> StoreResult<CreateAccountOutcome>;
    async fn get_account(&self, id: &str) -> StoreResult<Option<Account>>;
    /// single 模式的管理员账号（最早创建的 `is_admin`）。
    async fn first_admin(&self) -> StoreResult<Option<Account>>;
    async fn update_display_name(&self, id: &str, display_name: &str) -> StoreResult<()>;
    /// 最早创建的在前（`created_at_ms, id`）。`after` 是上一页最后一项的 `(created_at_ms, id)`，从它之后继续。
    async fn list_accounts(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Account>>;

    // ---- 邮箱（ADR-37） ----
    /// 按规范化后的邮箱找账号。
    async fn find_account_by_email(&self, email: &str) -> StoreResult<Option<Account>>;
    /// 设置（`Some`）或清除（`None`）账号的邮箱。邮箱已被别的账号占用返回 [`SetEmailOutcome::Taken`]。
    async fn set_account_email(&self, account_id: &str, email: Option<&str>) -> StoreResult<SetEmailOutcome>;
    /// 登记一条验证码。同一组（邮箱，用途，账号）里最多留 `keep` 条：先清掉这一组里已经过期的，再把最旧的顶掉。
    /// 新码**不会**让还没过期的旧码作废——别人请求新码顶不掉你手里那个。
    async fn put_email_code(&self, code: EmailCode, keep: u32) -> StoreResult<()>;
    /// 这个（邮箱，用途）下所有账号的码，最旧的在前（诊断和测试用，验证走 [`Store::attempt_email_codes`]）。
    async fn list_email_codes(&self, email: &str, purpose: EmailPurpose) -> StoreResult<Vec<EmailCode>>;
    /// 一次验证提交：**一条语句**里给这一组（邮箱，用途，账号）所有没过期、还没用完 `max_attempts` 次的码各计一次尝试，
    /// 并把刚被计过数的这几行交回来（没有可猜的码就是空）。计数和取码是同一个原子操作，所以并发的猜测一共也只有
    /// `max_attempts` 次比较的机会；调用方拿交回来的哈希比较，不会再碰到「比较的是旧行、次数记在新行上」。
    /// 顺手清掉这一组里过期的；次数用完的由调用方比较失败之后按 `id` 删（`attempts` 随行交回）。
    async fn attempt_email_codes(
        &self,
        email: &str,
        purpose: EmailPurpose,
        account_id: Option<&str>,
        now_ms: u64,
        max_attempts: u32,
    ) -> StoreResult<Vec<CodeCandidate>>;
    /// 按 id 删除一条验证码，返回它是否还在。比对成功时用它「认领」：并发的两个验证请求只有一个能拿到 `true`。
    async fn take_email_code(&self, id: i64) -> StoreResult<bool>;
    async fn purge_expired_email_codes(&self, now_ms: u64) -> StoreResult<u64>;

    // ---- Web Push 订阅 ----
    /// 一台设备只留一条：已有的被顶掉（换新 id）。
    async fn upsert_push_subscription(
        &self,
        device_id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        now_ms: u64,
    ) -> StoreResult<PushSubscription>;
    /// 账号名下（未吊销设备）的全部订阅，带设备名。
    async fn list_push_subscriptions(&self, account_id: &str) -> StoreResult<Vec<PushSubscriptionRow>>;
    async fn get_push_subscription_of_device(&self, device_id: &str) -> StoreResult<Option<PushSubscription>>;
    /// 删账号名下的一条订阅。
    async fn delete_push_subscription(&self, account_id: &str, id: &str) -> StoreResult<bool>;
    /// 按订阅 id 删（推送服务回 404 / 410 时）。用 id 而不是设备 id：这期间设备可能已经换了新订阅，不能误删。
    async fn delete_push_subscription_by_id(&self, id: &str) -> StoreResult<bool>;

    // ---- 通知偏好 ----
    /// 库里存着的行（没有行的类别用默认值，由调用方补）。
    async fn get_notification_prefs(&self, account_id: &str) -> StoreResult<Vec<NotificationPref>>;
    async fn set_notification_prefs(&self, account_id: &str, prefs: &[NotificationPref]) -> StoreResult<()>;

    // ---- 登录 token ----
    async fn create_account_token(
        &self,
        account_id: &str,
        label: &str,
        token_hash: &str,
        now_ms: u64,
    ) -> StoreResult<AccountToken>;
    async fn list_account_tokens(&self, account_id: &str) -> StoreResult<Vec<AccountToken>>;
    async fn delete_account_token(&self, account_id: &str, id: &str) -> StoreResult<bool>;
    /// 按哈希找账号，并刷新 `last_used_ms`。
    async fn find_account_by_token_hash(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<Account>>;

    // ---- 浏览器会话 ----
    async fn create_web_session(
        &self,
        account_id: &str,
        token_hash: &str,
        user_agent: &str,
        ip_prefix: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> StoreResult<WebSession>;
    /// 只返回未过期的会话及其账号。
    async fn find_web_session(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<(WebSession, Account)>>;
    async fn touch_web_session(&self, id: &str, now_ms: u64, expires_at_ms: u64) -> StoreResult<()>;
    async fn list_web_sessions(&self, account_id: &str, now_ms: u64) -> StoreResult<Vec<WebSession>>;
    async fn delete_web_session(&self, account_id: &str, id: &str) -> StoreResult<bool>;
    async fn delete_web_session_by_hash(&self, token_hash: &str) -> StoreResult<bool>;
    async fn delete_web_sessions_of_account(&self, account_id: &str) -> StoreResult<u64>;
    async fn purge_expired_web_sessions(&self, now_ms: u64) -> StoreResult<u64>;

    // ---- OAuth 身份与授权 state（GitHub、Google） ----
    /// 按（提供方，提供方的用户 id）找身份。
    async fn find_identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>>;
    /// 账号的全部身份，先绑定的在前。
    async fn list_identities(&self, account_id: &str) -> StoreResult<Vec<Identity>>;
    /// 把身份绑到账号上。一个账号在一个提供方最多一个身份、一个外部身份只属于一个账号，由库里的唯一约束保证，
    /// 判断和写入是一个原子操作（见 [`LinkIdentityOutcome`]）。
    async fn link_identity(
        &self,
        account_id: &str,
        provider: &str,
        subject: &str,
        now_ms: u64,
    ) -> StoreResult<LinkIdentityOutcome>;
    /// 解绑账号在这个提供方的身份，返回原来有没有。
    async fn unlink_identity(&self, account_id: &str, provider: &str) -> StoreResult<bool>;
    /// 原子地「建账号 + 绑身份（+ 带邀请码时兑换邀请）」：任何一步失败都什么都不留（不会有绑不上身份的空账号，
    /// 也不会白白用掉一个邀请）。
    async fn create_account_with_identity(
        &self,
        new: NewIdentityAccount<'_>,
        now_ms: u64,
    ) -> StoreResult<IdentityAccountOutcome>;
    /// 登记一次授权流程（state 的哈希、用途、PKCE verifier……）。
    async fn put_oauth_state(&self, state: NewOAuthState, now_ms: u64) -> StoreResult<()>;
    /// 原子地取走一次授权流程（单次使用）：行被删掉，已过期的也一样，但过期的返回 `None`。
    async fn take_oauth_state(&self, state_hash: &str, now_ms: u64) -> StoreResult<Option<OAuthState>>;
    /// 删掉所有过期的授权 state。
    async fn purge_expired_oauth_states(&self, now_ms: u64) -> StoreResult<u64>;

    // ---- 邀请 ----
    async fn create_invite(
        &self,
        created_by: &str,
        code_hash: &str,
        note: Option<&str>,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> StoreResult<Invite>;
    /// 最新的在前（`created_at_ms DESC, id DESC`）。`after` 是上一页最后一项的 `(created_at_ms, id)`，从它之后继续。
    async fn list_invites(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Invite>>;
    async fn delete_invite(&self, id: &str) -> StoreResult<bool>;
    /// 原子地「校验邀请 + 建账号 + 标记已用」。
    async fn redeem_invite(&self, code_hash: &str, display_name: &str, now_ms: u64) -> StoreResult<RedeemOutcome>;

    // ---- 设备 ----
    async fn create_device(&self, device: NewDevice, now_ms: u64) -> StoreResult<Device>;
    async fn get_device(&self, id: &str) -> StoreResult<Option<Device>>;
    async fn find_device_by_token_hash(&self, token_hash: &str) -> StoreResult<Option<Device>>;
    /// 账号下未吊销的设备。
    async fn list_devices(&self, account_id: &str) -> StoreResult<Vec<Device>>;
    async fn rename_device(&self, id: &str, name: &str) -> StoreResult<()>;
    async fn touch_device_seen(&self, id: &str, now_ms: u64) -> StoreResult<()>;
    /// 置 `revoked_at_ms` 并删除其全部配对、push 订阅，以及它签发的预览令牌和（它是节点时）它的全部预览 label 与令牌；
    /// 返回被删配对的另一端。设备不存在或已吊销返回空。
    async fn revoke_device(&self, id: &str, now_ms: u64) -> StoreResult<Vec<String>>;

    // ---- 配对 ----
    async fn insert_pairing(&self, a: &str, b: &str, now_ms: u64) -> StoreResult<()>;
    async fn peers_of(&self, device_id: &str) -> StoreResult<Vec<String>>;
    async fn is_paired(&self, a: &str, b: &str) -> StoreResult<bool>;
    /// 账号下所有配对（归一化顺序的两端）。
    async fn list_pairings_of_account(&self, account_id: &str) -> StoreResult<Vec<(String, String)>>;

    // ---- 预览（ADR-39，docs/spec/hub.md § 13） ----
    /// 原子地「绑定 label（第一次见到时）+ 签发令牌 + 清理这台设备的旧令牌」。
    /// label 已绑定到别的节点 → [`IssueOutcome::LabelTaken`]；label 是新的但节点已绑满 → [`IssueOutcome::TooManyLabels`]；
    /// 这两种情况下什么都不写。成功后这台设备未过期的令牌超过 `limits.tokens_per_device` 的，从最旧的开始删。
    async fn issue_preview_token(
        &self,
        token: NewPreviewToken,
        now_ms: u64,
        limits: PreviewIssueLimits,
    ) -> StoreResult<IssueOutcome>;
    /// 按哈希找令牌，**不看是否过期**（调用方比较 `expires_at_ms`）。
    async fn find_preview_token(&self, token_hash: &str) -> StoreResult<Option<PreviewToken>>;
    /// 令牌还在库里且没过期：已建立的预览连接用它周期性复核（令牌被删就关连接）。
    async fn preview_token_valid(&self, token_hash: &str, now_ms: u64) -> StoreResult<bool>;
    /// 删掉所有过期的令牌（label 的绑定保留）。返回删了几条。
    async fn purge_expired_preview_tokens(&self, now_ms: u64) -> StoreResult<u64>;
    /// 节点已绑定的 label，按绑定时间。
    async fn list_preview_labels(&self, node_id: &str) -> StoreResult<Vec<String>>;
    /// 设备签发的、库里还有的令牌（含过期未清的），按签发时间。
    async fn list_preview_tokens_of_device(&self, device_id: &str) -> StoreResult<Vec<PreviewToken>>;

    // ---- 审计 ----
    async fn insert_audit(&self, entry: NewAudit, now_ms: u64) -> StoreResult<()>;
    async fn list_audit(&self, limit: u32, before_id: Option<i64>) -> StoreResult<Vec<AuditEntry>>;
    async fn purge_audit_before(&self, ts_ms: u64) -> StoreResult<u64>;

    // ---- kv ----
    async fn kv_get(&self, key: &str) -> StoreResult<Option<String>>;
    async fn kv_set(&self, key: &str, value: &str) -> StoreResult<()>;

    // ---- multi：配额、账号删除、管理员标记、账号搜索 ----
    /// 账号名下未吊销的设备数，和其中 daemon 的数：`(devices, daemons)`。
    async fn count_devices(&self, account_id: &str) -> StoreResult<(u32, u32)>;
    /// 原子地「按配额检查 + 建设备」：`devices` 全建好，或者（会超出 `limits`）一台都不建。
    ///
    /// 检查和插入在同一个写事务里，数的是库里**此刻**未吊销的设备（包括同一事务里前面刚插入的），所以并发的两次
    /// 配对 / 接入收尾对同一个账号不会一起冲过上限。`devices` 可以是几个账号的，每台按自己的账号数。
    async fn create_devices_within_limits(
        &self,
        devices: Vec<NewDevice>,
        limits: DeviceLimits,
        now_ms: u64,
    ) -> StoreResult<CreateDevicesOutcome>;
    /// 设置 / 清除库里的管理员标记（不碰 `ANYCON_ADMIN_EMAILS` 派生的管理员）。账号不存在返回 `false`。
    async fn set_account_admin(&self, account_id: &str, is_admin: bool) -> StoreResult<bool>;
    /// 删账号：一个事务里删这个账号的 `hub_audit` 行、它兑换过的邀请（外键是 `SET NULL`，留着的话
    /// `used_by IS NULL` 会让这张邀请重新变得可用）、以它的邮箱发的登录码，然后删账号本身（其余表由外键级联删），
    /// 最后写入 `tombstone`（唯一留下的审计行）。账号不存在返回 `false`，什么都不写。
    async fn delete_account(&self, account_id: &str, tombstone: NewAudit, now_ms: u64) -> StoreResult<bool>;
    /// 管理页搜索：显示名或邮箱包含 `query`（不区分大小写），或账号 id 正好等于 `query`。分页同 [`Store::list_accounts`]。
    ///
    /// 默认实现按 keyset 顺序分批扫 [`Store::list_accounts`] 在内存里过滤：SQLite 的 `lower()` / `LIKE` 只认 ASCII，
    /// 非 ASCII 的显示名（`É`、`Ü`）用它们会漏。账号数量级下，管理员偶尔搜一次的开销可以接受。
    async fn search_accounts(
        &self,
        query: &str,
        limit: u32,
        after: Option<(u64, String)>,
    ) -> StoreResult<Vec<Account>> {
        const CHUNK: u32 = 500;
        let raw = query.trim();
        let needle = raw.to_lowercase();
        let mut found = Vec::new();
        let mut cursor = after;
        loop {
            let rows = self.list_accounts(CHUNK, cursor.clone()).await?;
            let full = rows.len() as u32 == CHUNK;
            let last = rows.last().map(|a| (a.created_at_ms, a.id.clone()));
            for account in rows {
                if account_matches(&account, &needle, raw) {
                    found.push(account);
                    if found.len() as u32 >= limit {
                        return Ok(found);
                    }
                }
            }
            match last {
                Some(last) if full => cursor = Some(last),
                _ => return Ok(found),
            }
        }
    }
}
