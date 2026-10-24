//! 拿到提供方给的身份（[`Profile`]）之后，怎么对应到账号：匹配、关联、注册。
//!
//! 登录（purpose `login` / `invite`），按顺序：
//! 1. `(provider, subject)` 已经是某个账号的身份 → 登录那个账号；
//! 2. 没有这个身份，但提供方**声明已验证**的邮箱等于某个账号的邮箱 → 把身份关联到那个账号并登录
//!    （这个账号在该提供方已有另一个身份 → `already_linked`）；
//! 3. 都没有：`multi` 建新账号（邮箱 = 已验证的邮箱，没有就没邮箱）；`single` 带邀请码就兑换邀请建账号，
//!    否则 `registration_closed`。
//!
//! **未验证 / 没有的邮箱从不用来匹配账号，也从不写进账号**（经典的账号接管：别人在提供方那边挂一个你的邮箱，
//! 不验证就能冒领你的账号）。想把这样的身份加到已有账号，要先登录那个账号，再在设置里关联。
//!
//! 关联（purpose `link`）：已登录的账号绑定一个身份，不碰邮箱。
//!
//! 并发：同一个身份的两次登录同时没找到账号时，建账号和绑身份是库里的一个事务（[`Store::create_account_with_identity`]），
//! 输的一方看到「身份 / 邮箱已被占用」就从头再判断一次（这回会走到第 1 或第 2 步），不会留下空账号。

use serde_json::json;

use super::provider::Profile;
use super::OAuthErrorCode;
use crate::accounts;
use crate::app::AppState;
use crate::config::{Mode, OAuthProvider};
use crate::error::{ApiError, ApiResult};
use crate::http::session::Client;
use crate::store::{Account, IdentityAccountOutcome, LinkIdentityOutcome, NewAudit, NewIdentityAccount};
use crate::util::{clip, ip_prefix, now_ms};

/// 并发竞争之后最多从头判断几次。
const ATTEMPTS: usize = 3;

/// 登录的结果。
#[derive(Debug)]
pub enum SignIn {
    /// 登录这个账号。
    Account(Account),
    /// 不能登录，回登录页带这个错误码。
    Rejected(OAuthErrorCode),
}

/// 一条审计：`identity_linked` 等账号层面的事件（IP 前缀 + UA，`detail` 只放提供方名）。
pub async fn audit(
    state: &AppState,
    client: &Client,
    event: &'static str,
    account_id: Option<&str>,
    detail: serde_json::Value,
) {
    state
        .audit(NewAudit {
            event,
            account_id: account_id.map(str::to_string),
            device_id: None,
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            user_agent: Some(clip(&client.user_agent, 300)),
            detail: Some(detail),
        })
        .await;
}

/// 登录 / 邀请注册（见模块文档）。`invite_hash`：purpose 为 `invite` 时邀请码的哈希。
pub async fn sign_in(
    state: &AppState,
    client: &Client,
    provider: OAuthProvider,
    profile: &Profile,
    invite_hash: Option<&str>,
) -> ApiResult<SignIn> {
    let p = provider.as_str();
    for _ in 0..ATTEMPTS {
        // 1. 身份已经存在。账号在这期间被删（外键级联会一并删掉身份）就当没有，往下走。
        if let Some(identity) = state.store.find_identity(p, &profile.subject).await? {
            if let Some(account) = state.store.get_account(&identity.account_id).await? {
                return Ok(SignIn::Account(account));
            }
        }

        // 2. 提供方声明已验证的邮箱 = 某个账号的邮箱。
        if let Some(account) = match profile.email.as_deref() {
            Some(email) => state.store.find_account_by_email(email).await?,
            None => None,
        } {
            match state.store.link_identity(&account.id, p, &profile.subject, now_ms()).await? {
                LinkIdentityOutcome::Linked => {
                    audit(state, client, "identity_linked", Some(&account.id), json!({ "provider": p })).await;
                    return Ok(SignIn::Account(account));
                }
                // 并发的另一次登录刚把它绑好了：就是这个账号
                LinkIdentityOutcome::AlreadyLinked => return Ok(SignIn::Account(account)),
                LinkIdentityOutcome::ProviderTaken => return Ok(SignIn::Rejected(OAuthErrorCode::AlreadyLinked)),
                // 身份刚被别的账号拿走了：从头再判断
                LinkIdentityOutcome::SubjectTaken => continue,
            }
        }

        // 3. 没有任何账号对应这个身份。
        let invite = match (state.config.mode, invite_hash) {
            (Mode::Multi, _) => None,
            (Mode::Single, Some(hash)) => Some(hash),
            (Mode::Single, None) => return Ok(SignIn::Rejected(OAuthErrorCode::RegistrationClosed)),
        };
        let names: Vec<&str> = profile.names.iter().map(String::as_str).collect();
        let display_name = accounts::signup_display_name(&names, profile.email.as_deref());
        let new = NewIdentityAccount {
            display_name: &display_name,
            email: profile.email.as_deref(),
            provider: p,
            subject: &profile.subject,
            invite_hash: invite,
        };
        match state.store.create_account_with_identity(new, now_ms()).await? {
            IdentityAccountOutcome::Created(account) => {
                accounts::audit_account_created(state, client, &account.id, p).await;
                if invite.is_some() {
                    audit(state, client, "invite_used", Some(&account.id), json!({ "method": p })).await;
                }
                return Ok(SignIn::Account(account));
            }
            // 邮箱 / 身份刚被并发的另一次登录占了：从头再判断（这回会走到第 1 或第 2 步）
            IdentityAccountOutcome::EmailTaken | IdentityAccountOutcome::IdentityTaken => continue,
            IdentityAccountOutcome::InviteNotFound => return Ok(SignIn::Rejected(OAuthErrorCode::InviteInvalid)),
            IdentityAccountOutcome::InviteExpired => return Ok(SignIn::Rejected(OAuthErrorCode::InviteExpired)),
            IdentityAccountOutcome::InviteUsed => return Ok(SignIn::Rejected(OAuthErrorCode::InviteUsed)),
        }
    }
    Err(ApiError::internal("cannot settle which account an OAuth identity belongs to"))
}

/// 已登录的账号绑定一个身份。成功（含已经绑在这个账号上）是 `Ok(None)`，否则是错误码。不碰账号的邮箱。
pub async fn link(
    state: &AppState,
    client: &Client,
    account_id: &str,
    provider: OAuthProvider,
    profile: &Profile,
) -> ApiResult<Option<OAuthErrorCode>> {
    let p = provider.as_str();
    Ok(match state.store.link_identity(account_id, p, &profile.subject, now_ms()).await? {
        LinkIdentityOutcome::Linked => {
            audit(state, client, "identity_linked", Some(account_id), json!({ "provider": p })).await;
            None
        }
        LinkIdentityOutcome::AlreadyLinked => None,
        LinkIdentityOutcome::SubjectTaken => Some(OAuthErrorCode::IdentityTaken),
        LinkIdentityOutcome::ProviderTaken => Some(OAuthErrorCode::AlreadyLinked),
    })
}
