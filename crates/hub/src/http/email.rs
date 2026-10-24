//! 邮箱验证码登录和账号里的邮箱绑定（[ADR-37](../../../../docs/decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)，
//! docs/spec/hub.md § 2.2 / § 2.3 / § 7.2 / § 8）。配了 SMTP 才有；没配时这些路由回 `404 not_found`。
//!
//! - `POST /auth/email/send { email }`：**总是 200**。`single` 里邮箱绑定了账号才发码；`multi` 是开放注册，对任何合法邮箱
//!   都发码。发码（查库、写库、发信）全在后台任务里做，响应时间不随邮箱是否注册变化（防枚举）。限速也与邮箱是否注册无关：
//!   先按 IP（5 次 / 小时）、再按邮箱（60 秒一次、每天最多 10 个码）。
//! - `POST /auth/email/verify { email, code }`：成功 `204` + 登录 cookie；码不对、过期、不存在一律 `401 invalid_code`，
//!   失败的响应至少在请求开始之后 250 ms 才发出（成功不等）。`multi` 里码对了、而邮箱还没有账号时，用这个邮箱建账号
//!   （[`crate::accounts::sign_up`]）再登录；`single` 里这种情况和码不对一样处理。
//! - `POST /api/me/email { email }` · `POST /api/me/email/verify { email, code }` · `DELETE /api/me/email`：绑定 / 换绑 / 解绑。
//!
//! 码：6 位数字，库里只存哈希，比较用常数时间，验证成功立刻删（一次性）。同一组（邮箱，用途，账号）最多同时 3 条有效的码，
//! 新码不让旧码作废；每条码尝试 5 次作废（一次提交对每条有效的码各计一次，计数和取码是库里的一条语句）；
//! 每个邮箱每天累计验证失败 30 次后，当天不再接受它的码（不管请求来自哪个 IP）。
//! 日志只记邮箱的哈希前 8 位，不记邮箱、不记码。

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::accounts::{self, SignUp};
use crate::app::AppState;
use crate::config::Mode;
use crate::error::{ApiError, ApiResult};
use crate::mail::{code_mail, lang_from_accept_language, normalize_email, CodeKind, Lang, OutgoingMail};
use crate::store::{EmailCode, EmailPurpose, NewAudit, SetEmailOutcome};
use crate::util::{clip, ct_eq, email_tag, hash_token, ip_prefix, now_ms};

use super::api::{me_view, MeView};
use super::json::ApiJson;
use super::session::{start_session, Authed, Client};

/// 邮箱功能没开（没配 SMTP）：和路由不存在一样。
fn require_email(state: &AppState) -> ApiResult<()> {
    if state.email_enabled() {
        Ok(())
    } else {
        Err(ApiError::not_found())
    }
}

fn invalid_email() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_email", "not a valid email address")
}

fn invalid_code(status: StatusCode) -> ApiError {
    ApiError::new(status, "invalid_code", "the code is wrong, expired or already used")
}

/// 6 位数字码，均匀随机（`OsRng`）。
fn new_code() -> String {
    format!("{:06}", OsRng.gen_range(0..1_000_000u32))
}

/// 码的存库形式。带上用途和邮箱做域分隔：同一个码在别的邮箱 / 用途下哈希不同。
fn code_hash(email: &str, purpose: EmailPurpose, code: &str) -> String {
    hash_token(&format!("anycon-email-code\0{}\0{email}\0{code}", purpose.as_str()))
}

/// 用户输入的码规整成 6 位数字（容忍 `123 456`、`123-456` 这种分组写法）；不是 6 位数字返回 `None`。
fn normalize_code(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

fn accept_language(headers: &HeaderMap) -> Lang {
    lang_from_accept_language(headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()))
}

#[derive(Serialize)]
pub struct SendOk {
    ok: bool,
    /// 多少秒之后可以再要一个码。
    resend_after_secs: u64,
}

fn send_ok(state: &AppState) -> Json<SendOk> {
    Json(SendOk { ok: true, resend_after_secs: state.config.limits.email_resend_interval.as_secs() })
}

/// 生成码、存哈希、发邮件。后台任务里调用；失败只记日志（收件人哈希前 8 位 + SMTP 状态）。
async fn issue_code(state: &AppState, email: &str, purpose: EmailPurpose, account_id: Option<String>, lang: Lang) {
    let Some(mailer) = state.mailer.clone() else { return };
    let code = new_code();
    let now = now_ms();
    let ttl = state.config.timing.email_code_ttl;
    let row = EmailCode {
        email: email.to_string(),
        purpose,
        account_id,
        code_hash: code_hash(email, purpose, &code),
        expires_at_ms: now + ttl.as_millis() as u64,
        attempts: 0,
        created_at_ms: now,
    };
    if let Err(e) = state.store.put_email_code(row, state.config.limits.email_codes_active).await {
        tracing::warn!(error = %e, "cannot store an email code");
        return;
    }
    let kind = if purpose == EmailPurpose::Login { CodeKind::Login } else { CodeKind::Bind };
    let (subject, body) = code_mail(lang, kind, &code, (ttl.as_secs() / 60).max(1));
    let tag = email_tag(email);
    match mailer.send(OutgoingMail { to: email.to_string(), subject, body }).await {
        Ok(()) => tracing::info!(to = %tag, purpose = purpose.as_str(), "email code sent"),
        Err(e) => tracing::warn!(
            to = %tag,
            purpose = purpose.as_str(),
            smtp_status = e.status().unwrap_or("-"),
            error = %e.redacted(email),
            "email code could not be sent"
        ),
    }
}

/// 每日计数（发码、验证失败）的键：用途分开计，同一个邮箱的登录和绑定互不影响。
fn daily_key(purpose: EmailPurpose, email: &str) -> String {
    format!("{}:{email}", purpose.as_str())
}

/// 校验用户提交的码。这一组（邮箱，用途，账号）里没有码、都过期、次数都用完、不是该账号发起的绑定，都当「码不对」。
///
/// 1. 这个邮箱当天验证失败已经满了（`email_failures_per_day`）→ 不再看码，一律不对（不管请求来自哪个 IP）；
/// 2. [`Store::attempt_email_codes`]：**一条语句**给这一组所有有效的码各计一次尝试，并把被计过数的那几行交回来——
///    计数先于比较，并发的猜测一共也只有 `email_code_max_attempts` 次比较的机会，比较的就是被计过数的那几行
///    （不会出现「比的是旧行、次数记在新行上」）；
/// 3. 和每一条交回来的码都比一遍（不在命中时提前退出），命中就按 id 原子认领（删掉）它，并发提交同一个码只有一个能成功；
/// 4. 没命中：记一次当天的验证失败，并把这次提交刚好用完次数的码删掉。没有可猜的码（没发过、过期了）不算失败，
///    不创建每日计数的键——否则随便一个邮箱字符串都能往内存里塞计数器。
async fn check_code(
    state: &AppState,
    email: &str,
    purpose: EmailPurpose,
    submitted: &str,
    expected_account: Option<&str>,
) -> ApiResult<bool> {
    let key = daily_key(purpose, email);
    if state.account_limits.email_daily_failures.is_full(&key) {
        return Ok(false);
    }
    let max = state.config.limits.email_code_max_attempts;
    let candidates = state.store.attempt_email_codes(email, purpose, expected_account, now_ms(), max).await?;
    if candidates.is_empty() {
        return Ok(false);
    }
    // 格式不对的输入也走一遍哈希和比较，成功 / 失败路径的耗时一样。
    let submitted_hash = code_hash(email, purpose, &normalize_code(submitted).unwrap_or_default());
    let mut hit = None;
    for c in &candidates {
        if ct_eq(&submitted_hash, &c.code_hash) {
            hit.get_or_insert(c.id);
        }
    }
    if let Some(id) = hit {
        if state.store.take_email_code(id).await? {
            return Ok(true);
        }
    }
    state.account_limits.email_daily_failures.record(&key);
    for c in candidates.iter().filter(|c| c.attempts >= max) {
        if let Err(e) = state.store.take_email_code(c.id).await {
            tracing::warn!(error = %e, "cannot delete a used-up email code");
        }
    }
    Ok(false)
}

/// 验证失败的响应至少要等到请求开始之后 `email_verify_failure_delay`：成功和失败、「有待验证的码」和「没有」
/// 在库里做的事不一样，耗时差几十微秒，多采样几次就能分出邮箱绑没绑；固定的下限把这点差别埋掉。成功不等。
async fn pad_failure(state: &AppState, started: Instant) {
    let deadline = tokio::time::Instant::from_std(started) + state.config.timing.email_verify_failure_delay;
    tokio::time::sleep_until(deadline).await;
}

fn audit_email_login(client: &Client, event: &'static str, account: Option<String>) -> NewAudit {
    NewAudit {
        event,
        account_id: account,
        device_id: None,
        ip_prefix: Some(ip_prefix(client.ctx.ip)),
        user_agent: Some(clip(&client.user_agent, 300)),
        detail: Some(serde_json::json!({ "method": "email" })),
    }
}

// ---------------------------------------------------------------------------
// 登录
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SendReq {
    email: String,
}

/// `POST /auth/email/send { email }`。
pub async fn login_send(
    State(state): State<Arc<AppState>>,
    client: Client,
    headers: HeaderMap,
    ApiJson(req): ApiJson<SendReq>,
) -> ApiResult<Json<SendOk>> {
    require_email(&state)?;
    if let Err(wait) = state.limits.email_send.check(client.ctx.ip) {
        tracing::info!(limit = "email_send", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let email = normalize_email(&req.email).ok_or_else(invalid_email)?;
    // 对所有邮箱一视同仁（不管有没有绑定账号），这样 429 本身不泄露任何东西。
    if let Err(wait) = state.account_limits.email_resend.check(&format!("login:{email}")) {
        tracing::info!(limit = "email_resend", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    // 每个邮箱每天最多 10 个码：同样对所有邮箱一视同仁，换 IP 也一样。
    if let Err(wait) = state.account_limits.email_daily_codes.try_record(&daily_key(EmailPurpose::Login, &email)) {
        tracing::info!(limit = "email_daily_codes", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let lang = accept_language(&headers);
    let st = state.clone();
    // 绑定没绑定都走到这里、都立刻返回；查库和发信都在后台，响应时间与结果无关。
    state.bg.spawn(async move {
        // `multi` 对任何邮箱发码（验证成功时没有账号就建一个）；`single` 只发给已经绑定了账号的邮箱。
        if st.config.mode == Mode::Multi {
            issue_code(&st, &email, EmailPurpose::Login, None, lang).await;
            return;
        }
        match st.store.find_account_by_email(&email).await {
            Ok(Some(_)) => issue_code(&st, &email, EmailPurpose::Login, None, lang).await,
            Ok(None) => tracing::debug!(to = %email_tag(&email), "login code requested for an email without an account, not sending"),
            Err(e) => tracing::warn!(error = %e, "cannot look up the account for a login code"),
        }
    });
    Ok(send_ok(&state))
}

#[derive(Deserialize)]
pub struct VerifyReq {
    email: String,
    code: String,
}

/// `POST /auth/email/verify { email, code }`：成功 `204` + cookie。
pub async fn login_verify(
    State(state): State<Arc<AppState>>,
    client: Client,
    ApiJson(req): ApiJson<VerifyReq>,
) -> ApiResult<Response> {
    let started = Instant::now();
    require_email(&state)?;
    if let Err(wait) = state.limits.email_verify.check(client.ctx.ip) {
        tracing::info!(limit = "email_verify", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    // 失败的响应一律等到至少 `email_verify_failure_delay` 才发出（成功不等）。
    let fail = |account: Option<String>| async {
        state.audit(audit_email_login(&client, "login_fail", account)).await;
        pad_failure(&state, started).await;
        Err::<Response, _>(invalid_code(StatusCode::UNAUTHORIZED))
    };
    // 邮箱格式不对也说「码不对」：不给探测的机会。
    let Some(email) = normalize_email(&req.email) else { return fail(None).await };
    if !check_code(&state, &email, EmailPurpose::Login, &req.code, None).await? {
        return fail(None).await;
    }
    // 码发出之后邮箱可能已经被解绑 / 换掉（`single`）。`multi` 里没有账号就注册一个：码证明了这个邮箱是本人的。
    let account = match state.store.find_account_by_email(&email).await? {
        Some(account) => account,
        None if state.config.mode == Mode::Multi => {
            let signup = SignUp { method: "email", names: &[], email: Some(&email) };
            accounts::sign_up(&state, &client, signup).await?.into_account()
        }
        None => return fail(None).await,
    };
    let cookie = start_session(&state, &account.id, &client).await?;
    state.audit(audit_email_login(&client, "login_ok", Some(account.id))).await;
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}

// ---------------------------------------------------------------------------
// 账号里的邮箱：绑定 / 换绑 / 解绑
// ---------------------------------------------------------------------------

/// `POST /api/me/email { email }`：给这个邮箱发一个绑定用的码。邮箱已被别的账号占用 → `409 email_taken`。
pub async fn bind_send(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    headers: HeaderMap,
    ApiJson(req): ApiJson<SendReq>,
) -> ApiResult<Json<SendOk>> {
    require_email(&state)?;
    if let Err(wait) = state.limits.bind_send.check(client.ctx.ip) {
        tracing::info!(limit = "bind_send", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let email = normalize_email(&req.email).ok_or_else(invalid_email)?;
    if let Some(owner) = state.store.find_account_by_email(&email).await? {
        if owner.id != auth.account.id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "email_taken",
                "this email is already bound to another account",
            ));
        }
    }
    // 60 秒的重发间隔按（账号，邮箱）计：别的账号对同一个邮箱要码，不会占掉你的窗口。
    if let Err(wait) = state.account_limits.email_resend.check(&format!("bind:{}:{email}", auth.account.id)) {
        tracing::info!(limit = "email_resend", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    if let Err(wait) = state.account_limits.email_daily_codes.try_record(&daily_key(EmailPurpose::Bind, &email)) {
        tracing::info!(limit = "email_daily_codes", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let lang = accept_language(&headers);
    let st = state.clone();
    let account_id = auth.account.id;
    state.bg.spawn(async move { issue_code(&st, &email, EmailPurpose::Bind, Some(account_id), lang).await });
    Ok(send_ok(&state))
}

/// `POST /api/me/email/verify { email, code }`：码对了就把邮箱绑到当前账号，返回更新后的 `/api/me`。
/// 码不对是 `400 invalid_code`（不是 401：账号是登录着的，前端遇到 401 会当成登录失效）。
pub async fn bind_verify(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    ApiJson(req): ApiJson<VerifyReq>,
) -> ApiResult<Json<MeView>> {
    let started = Instant::now();
    require_email(&state)?;
    if let Err(wait) = state.limits.bind_verify.check(client.ctx.ip) {
        tracing::info!(limit = "bind_verify", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    // 失败的响应同样至少等 `email_verify_failure_delay`（成功不等）。
    let Some(email) = normalize_email(&req.email) else {
        pad_failure(&state, started).await;
        return Err(invalid_code(StatusCode::BAD_REQUEST));
    };
    if !check_code(&state, &email, EmailPurpose::Bind, &req.code, Some(&auth.account.id)).await? {
        pad_failure(&state, started).await;
        return Err(invalid_code(StatusCode::BAD_REQUEST));
    }
    match state.store.set_account_email(&auth.account.id, Some(&email)).await? {
        SetEmailOutcome::Taken => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "email_taken",
                "this email is already bound to another account",
            ));
        }
        SetEmailOutcome::Done => {}
    }
    let change = match auth.account.email.as_deref() {
        None => "bound",
        Some(old) if old == email => "unchanged",
        Some(_) => "changed",
    };
    if change != "unchanged" {
        state
            .audit(NewAudit {
                event: "email_changed",
                account_id: Some(auth.account.id.clone()),
                ip_prefix: Some(ip_prefix(client.ctx.ip)),
                user_agent: Some(clip(&client.user_agent, 300)),
                detail: Some(serde_json::json!({ "change": change })),
                ..Default::default()
            })
            .await;
    }
    let mut account = auth.account;
    account.email = Some(email);
    Ok(Json(me_view(&state, account).await?))
}

/// `DELETE /api/me/email`：解绑。账号没有别的登录方式时 `409 last_login_method`。没有绑定过也是 `204`。
pub async fn unbind(State(state): State<Arc<AppState>>, auth: Authed, client: Client) -> ApiResult<Response> {
    require_email(&state)?;
    if auth.account.email.is_none() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let others =
        accounts::account_login_methods(&state, &auth.account).await?.into_iter().filter(|m| *m != "email").count();
    if others == 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "last_login_method",
            "the email is the only way this account can sign in; add another sign-in method first",
        ));
    }
    state.store.set_account_email(&auth.account.id, None).await?;
    state
        .audit(NewAudit {
            event: "email_changed",
            account_id: Some(auth.account.id),
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            user_agent: Some(clip(&client.user_agent, 300)),
            detail: Some(serde_json::json!({ "change": "removed" })),
            ..Default::default()
        })
        .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_six_digits_with_leading_zeros_kept() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 6, "{c}");
            assert!(c.bytes().all(|b| b.is_ascii_digit()), "{c}");
        }
    }

    #[test]
    fn submitted_codes_tolerate_grouping_but_nothing_else() {
        assert_eq!(normalize_code("123456").as_deref(), Some("123456"));
        assert_eq!(normalize_code(" 123 456 ").as_deref(), Some("123456"));
        assert_eq!(normalize_code("123-456").as_deref(), Some("123456"));
        assert_eq!(normalize_code("000042").as_deref(), Some("000042"));
        for bad in ["", "12345", "1234567", "12a456", "１２３４５６", "123456\u{0}", "abcdef"] {
            assert_eq!(normalize_code(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_stored_hash_binds_the_code_to_its_email_and_purpose() {
        let h = code_hash("a@b.c", EmailPurpose::Login, "123456");
        assert_eq!(h.len(), 64);
        assert_eq!(h, code_hash("a@b.c", EmailPurpose::Login, "123456"));
        assert_ne!(h, code_hash("a@b.c", EmailPurpose::Login, "123457"));
        assert_ne!(h, code_hash("x@b.c", EmailPurpose::Login, "123456"));
        assert_ne!(h, code_hash("a@b.c", EmailPurpose::Bind, "123456"));
        assert!(!h.contains("123456"));
    }
}
