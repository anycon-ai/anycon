//! OAuth 相关的存储类型：账号的外部身份（`identities`）和授权流程里的一次性 state（`oauth_states`）。
//!
//! 全是元数据：身份只有 `(提供方, 提供方的稳定用户 id)` 属于哪个账号，不存用户名、邮箱和 access token；
//! state 以哈希存放，PKCE 的 verifier 在库里是明文（换 token 时要带给提供方），随 state 一起被一次性取走。

use super::{Account, StoreError, StoreResult};

/// 账号在一个提供方的身份。`Debug` 不输出 `subject`。
#[derive(Clone)]
pub struct Identity {
    /// `github` 或 `google`（[`crate::config::OAuthProvider::as_str`]）。
    pub provider: String,
    /// 提供方的稳定用户 id（GitHub 的数字 id、Google 的 `sub`）。
    pub subject: String,
    pub account_id: String,
    pub created_at_ms: u64,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("provider", &self.provider)
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

/// [`super::Store::link_identity`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkIdentityOutcome {
    /// 新绑定了。
    Linked,
    /// 这个身份本来就在这个账号上，什么都没写。
    AlreadyLinked,
    /// 这个身份在**别的**账号上，什么都没写。
    SubjectTaken,
    /// 这个账号在这个提供方已经有一个**不同**的身份，什么都没写。
    ProviderTaken,
}

/// 一次授权流程的用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthPurpose {
    /// 登录（`multi` 里没有账号就注册）。
    Login,
    /// 已登录的账号绑定一个身份。
    Link,
    /// `single`：带邀请码注册。
    Invite,
}

impl OAuthPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            OAuthPurpose::Login => "login",
            OAuthPurpose::Link => "link",
            OAuthPurpose::Invite => "invite",
        }
    }

    pub fn parse(s: &str) -> StoreResult<Self> {
        Ok(match s {
            "login" => OAuthPurpose::Login,
            "link" => OAuthPurpose::Link,
            "invite" => OAuthPurpose::Invite,
            other => return Err(StoreError::Corrupt(format!("unknown oauth purpose {other:?}"))),
        })
    }
}

/// 登记一次授权流程的参数（[`super::Store::put_oauth_state`]）。`Debug` 不输出哈希、邀请码哈希和 verifier。
#[derive(Clone)]
pub struct NewOAuthState {
    /// state 的哈希（[`crate::util::hash_token`]）。
    pub state_hash: String,
    pub provider: String,
    pub purpose: OAuthPurpose,
    /// `Link` 时是发起关联的账号。
    pub account_id: Option<String>,
    /// `Invite` 时是邀请码的哈希。
    pub invite_hash: Option<String>,
    /// 登录成功后回到哪里（已校验的站内路径）。
    pub next_path: Option<String>,
    pub pkce_verifier: String,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for NewOAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewOAuthState")
            .field("provider", &self.provider)
            .field("purpose", &self.purpose)
            .finish_non_exhaustive()
    }
}

/// [`super::Store::take_oauth_state`] 取出来的一次授权流程。`Debug` 不输出邀请码哈希和 verifier。
#[derive(Clone)]
pub struct OAuthState {
    pub provider: String,
    pub purpose: OAuthPurpose,
    pub account_id: Option<String>,
    pub invite_hash: Option<String>,
    pub next_path: Option<String>,
    pub pkce_verifier: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for OAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthState")
            .field("provider", &self.provider)
            .field("purpose", &self.purpose)
            .finish_non_exhaustive()
    }
}

/// 为一个新身份建账号的参数（[`super::Store::create_account_with_identity`]）。
#[derive(Clone, Copy)]
pub struct NewIdentityAccount<'a> {
    pub display_name: &'a str,
    /// 提供方声明**已验证**的邮箱（规范化后）。没有就建一个没邮箱的账号。
    pub email: Option<&'a str>,
    pub provider: &'a str,
    pub subject: &'a str,
    /// `Some`：`single` 的邀请注册，原子地兑换这个邀请码（哈希）；`None`：开放注册（`multi`）。
    pub invite_hash: Option<&'a str>,
}

impl std::fmt::Debug for NewIdentityAccount<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewIdentityAccount").field("provider", &self.provider).finish_non_exhaustive()
    }
}

/// [`super::Store::create_account_with_identity`] 的结果。除 `Created` 外什么都没写。
#[derive(Debug)]
pub enum IdentityAccountOutcome {
    /// 账号和身份建好了（带邀请码时邀请也已标记为已用）。
    Created(Account),
    /// 邮箱已经属于别的账号（并发注册 / 刚被绑定）。
    EmailTaken,
    /// 这个身份已经属于一个账号（并发的另一次登录刚建好）。
    IdentityTaken,
    InviteNotFound,
    InviteExpired,
    InviteUsed,
}
