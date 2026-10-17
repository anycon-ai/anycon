//! 通知分发：daemon 发给**离线设备**的 `notify` 信封触发 Web Push 和邮件
//! （[ADR-37](../../../docs/decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)，docs/spec/hub.md § 6）。
//!
//! 触发点在 `ws::conn::forward`：已配对的 `(from, to)` 之间的 `notify` 信封，目标没有在线连接时调用 [`on_undelivered`]。
//! 目标在线的话信封照常经 WebSocket 送到（页面在后台由页面自己弹系统通知），hub 不再推一次。
//!
//! 这里是 hub 里**唯一**会读 `notify` 信封 body 的地方，而且只读明文外层的 [`NotifyOuter`]（`category`、不透明的 `session_id`）；
//! `detail`（Noise 密文）一个字节都不碰。判定和发送都在后台任务里，不阻塞转发。
//!
//! 日志纪律：只记设备 id、类别、推送服务主机名 / 状态码、收件人哈希前 8 位；不记 body、邮箱、endpoint 路径、邮件正文。

use std::sync::Arc;

use anycon_protocol::{DeviceKind, Envelope, NotifyCategory};
use axum::body::Bytes;
use serde::Deserialize;

use crate::app::AppState;
use crate::mail::{is_email_category, notification_mail, OutgoingMail};
use crate::push::{PushOptions, PushOutcome, Urgency};
use crate::store::{category_to_str, NotificationPref};
use crate::util::email_tag;

/// 一封 `notify` 信封超过这个大小就不理它：它的外层只有一个类别和一个会话 id，正常远小于此。
const MAX_NOTIFY_FRAME: usize = 64 * 1024;
/// 会话 id 的长度上限。
const MAX_SESSION_ID: usize = 128;

/// 全部类别，顺序同 [`NotifyCategory`] 的声明。
pub const ALL_CATEGORIES: [NotifyCategory; 6] = [
    NotifyCategory::PermissionRequested,
    NotifyCategory::TurnFinished,
    NotifyCategory::Error,
    NotifyCategory::SessionIdle,
    NotifyCategory::EnrollRequested,
    NotifyCategory::Security,
];

/// 偏好的默认值（docs/spec/hub.md § 6.3）：`permission_requested` / `error` / `enroll_requested` / `security`
/// 推送和邮件都开，`turn_finished` 只开推送，`session_idle` 全关。
pub fn default_pref(c: NotifyCategory) -> NotificationPref {
    let (push, email) = match c {
        NotifyCategory::PermissionRequested
        | NotifyCategory::Error
        | NotifyCategory::EnrollRequested
        | NotifyCategory::Security => (true, true),
        NotifyCategory::TurnFinished => (true, false),
        NotifyCategory::SessionIdle => (false, false),
    };
    NotificationPref { category: c, push, email }
}

/// 库里的偏好叠在默认值上，得到六个类别的完整结果。
pub fn effective_prefs(stored: &[NotificationPref]) -> Vec<NotificationPref> {
    ALL_CATEGORIES
        .iter()
        .map(|&c| stored.iter().find(|p| p.category == c).copied().unwrap_or_else(|| default_pref(c)))
        .collect()
}

/// 推送的 `TTL` / `Urgency`（docs/spec/hub.md § 6.1）：1 小时，`enroll_requested` 5 分钟（和接入会话一样长）；
/// `permission_requested`、`enroll_requested` 是 `high`。
pub fn push_options(c: NotifyCategory) -> PushOptions {
    PushOptions {
        ttl: if c == NotifyCategory::EnrollRequested { 300 } else { 3600 },
        urgency: if matches!(c, NotifyCategory::PermissionRequested | NotifyCategory::EnrollRequested) {
            Urgency::High
        } else {
            Urgency::Normal
        },
    }
}

/// 推送载荷（RFC 8291 加密之前的明文）：`{ category, node, session }`，没有任何正文。
pub fn push_payload(category: NotifyCategory, node: &str, session: Option<&str>) -> Vec<u8> {
    serde_json::json!({ "category": category_to_str(category), "node": node, "session": session })
        .to_string()
        .into_bytes()
}

/// `notify` 信封 body 的明文外层。**故意不声明 `detail`**：它是 Noise 密文，hub 不读（serde 会把它当未知字段跳过）。
#[derive(Deserialize)]
struct NotifyOuter {
    category: NotifyCategory,
    #[serde(default)]
    session_id: Option<String>,
}

/// 会话 id 只放行安全字符（它会进推送载荷和邮件里的链接）；不合规就当没有，通知照发。
fn clean_session_id(s: Option<String>) -> Option<String> {
    s.filter(|s| {
        !s.is_empty()
            && s.len() <= MAX_SESSION_ID
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

/// 点开通知后去的页面（邮件里的链接）。
fn link_for(public_url: &str, category: NotifyCategory, node: &str, session: Option<&str>) -> String {
    match (category, session) {
        (NotifyCategory::EnrollRequested | NotifyCategory::Security, _) => {
            format!("{public_url}/n/{node}/settings?tab=enroll")
        }
        (_, Some(s)) => format!("{public_url}/n/{node}/s/{s}"),
        (_, None) => format!("{public_url}/n/{node}"),
    }
}

/// 已配对的 `from` → `to` 发了一封 `notify`，而 `to` 此刻没有在线连接：在后台判定要不要推送 / 发邮件。
/// 不阻塞调用方（信封转发路径）：这里只做廉价的大小和速率检查，然后把活交给后台任务。
pub fn on_undelivered(state: &Arc<AppState>, from: &str, to: &str, frame: &Bytes) {
    if frame.len() > MAX_NOTIFY_FRAME {
        tracing::debug!(from, to, len = frame.len(), "notify: oversized envelope, not considered for push or email");
        return;
    }
    if state.account_limits.notify_device.check(&to.to_string()).is_err() {
        tracing::debug!(to, "notify: too many notifications for this device, not considered for push or email");
        return;
    }
    let st = state.clone();
    let (from, to, frame) = (from.to_string(), to.to_string(), frame.clone());
    state.bg.spawn(async move { dispatch(st, from, to, frame).await });
}

async fn dispatch(state: Arc<AppState>, from: String, to: String, frame: Bytes) {
    // 解不出来就不推（信封本身已经照常转发 / 回执过了）。
    let Some(outer) = Envelope::decode(&frame).ok().and_then(|e| rmp_serde::from_slice::<NotifyOuter>(&e.body).ok())
    else {
        tracing::debug!(%from, %to, "notify: body is not a readable notify body, not pushing");
        return;
    };
    let category = outer.category;
    let session = clean_session_id(outer.session_id);

    let (sender, recipient) = match (state.store.get_device(&from).await, state.store.get_device(&to).await) {
        (Ok(Some(s)), Ok(Some(r))) => (s, r),
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(error = %e, "notify: device lookup failed");
            return;
        }
        _ => return,
    };
    // 通知只来自账号里的 daemon，发给同一账号里的活设备。
    if sender.kind != DeviceKind::Daemon
        || sender.revoked_at_ms.is_some()
        || recipient.revoked_at_ms.is_some()
        || sender.account_id != recipient.account_id
    {
        tracing::debug!(%from, %to, "notify: not from a daemon of the same account, ignored");
        return;
    }
    let account_id = recipient.account_id.clone();
    let prefs = match state.store.get_notification_prefs(&account_id).await {
        Ok(stored) => effective_prefs(&stored),
        Err(e) => {
            tracing::warn!(error = %e, "notify: cannot load the notification preferences");
            return;
        }
    };
    let pref = prefs.iter().find(|p| p.category == category).copied().unwrap_or_else(|| default_pref(category));

    if pref.push {
        push_to_device(&state, &recipient.id, &sender.id, category, session.as_deref()).await;
    }
    if pref.email && is_email_category(category) {
        mail_account(&state, &account_id, &sender.name, &sender.id, category, session.as_deref()).await;
    }
}

/// 给收件设备的订阅发一条推送。
async fn push_to_device(
    state: &AppState,
    device_id: &str,
    node: &str,
    category: NotifyCategory,
    session: Option<&str>,
) {
    let sub = match state.store.get_push_subscription_of_device(device_id).await {
        Ok(Some(sub)) => sub,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "notify: cannot load the push subscription");
            return;
        }
    };
    let outcome = state.push.send(&sub, &push_payload(category, node, session), push_options(category)).await;
    if outcome == PushOutcome::Gone {
        // 按订阅 id 删：这期间设备可能已经换了新订阅，不能误删。
        if let Err(e) = state.store.delete_push_subscription_by_id(&sub.id).await {
            tracing::warn!(error = %e, "notify: cannot delete a gone push subscription");
        }
    }
}

/// 给账号绑定的邮箱发一封通知邮件（条件见 docs/spec/hub.md § 6.2）。
async fn mail_account(
    state: &AppState,
    account_id: &str,
    node_name: &str,
    node: &str,
    category: NotifyCategory,
    session: Option<&str>,
) {
    let Some(mailer) = state.mailer.clone() else { return };
    let account = match state.store.get_account(account_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "notify: cannot load the account");
            return;
        }
    };
    let Some(email) = account.email else { return };
    // 用户正在看（有 web 设备在线）就不发邮件。CLI / SDK 设备不算：它们可以是常驻的脚本，不代表人在看。
    if state.registry.account_has_web_online(account_id) {
        tracing::debug!(category = category_to_str(category), "notify: a web device is online, no email");
        return;
    }
    // 同一账号同一类别 10 分钟一封。放在最后：前面任何条件不满足都不该占掉这个窗口。
    if state.account_limits.notify_email.check(&(account_id.to_string(), category)).is_err() {
        tracing::debug!(category = category_to_str(category), "notify: email already sent recently for this category");
        return;
    }
    let link = link_for(&state.config.public_url_str(), category, node, session);
    let Some((subject, body)) = notification_mail(category, node_name, &link) else { return };
    let tag = email_tag(&email);
    match mailer.send(OutgoingMail { to: email.clone(), subject, body }).await {
        Ok(()) => tracing::info!(to = %tag, category = category_to_str(category), "notification email sent"),
        Err(e) => tracing::warn!(
            to = %tag,
            category = category_to_str(category),
            smtp_status = e.status().unwrap_or("-"),
            error = %e.redacted(&email),
            "notification email failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_spec() {
        use NotifyCategory::*;
        for c in [PermissionRequested, Error, EnrollRequested, Security] {
            assert_eq!(default_pref(c), NotificationPref { category: c, push: true, email: true });
        }
        assert_eq!(default_pref(TurnFinished), NotificationPref { category: TurnFinished, push: true, email: false });
        assert_eq!(default_pref(SessionIdle), NotificationPref { category: SessionIdle, push: false, email: false });
    }

    #[test]
    fn stored_prefs_override_defaults_and_keep_the_canonical_order() {
        use NotifyCategory::*;
        let stored = [
            NotificationPref { category: Security, push: false, email: true },
            NotificationPref { category: SessionIdle, push: true, email: false },
        ];
        let eff = effective_prefs(&stored);
        assert_eq!(eff.iter().map(|p| p.category).collect::<Vec<_>>(), ALL_CATEGORIES.to_vec());
        assert_eq!(eff[5], stored[0]);
        assert_eq!(eff[3], stored[1]);
        assert_eq!(eff[0], default_pref(PermissionRequested));
        assert_eq!(effective_prefs(&[]).len(), 6);
    }

    #[test]
    fn push_headers_follow_the_spec() {
        use NotifyCategory::*;
        assert_eq!(push_options(PermissionRequested), PushOptions { ttl: 3600, urgency: Urgency::High });
        assert_eq!(push_options(EnrollRequested), PushOptions { ttl: 300, urgency: Urgency::High });
        for c in [TurnFinished, Error, SessionIdle, Security] {
            assert_eq!(push_options(c), PushOptions { ttl: 3600, urgency: Urgency::Normal }, "{c:?}");
        }
    }

    #[test]
    fn payload_has_exactly_category_node_and_session() {
        let v: serde_json::Value =
            serde_json::from_slice(&push_payload(NotifyCategory::PermissionRequested, "node-1", Some("sess-9")))
                .unwrap();
        assert_eq!(v, serde_json::json!({ "category": "permission_requested", "node": "node-1", "session": "sess-9" }));
        let v: serde_json::Value =
            serde_json::from_slice(&push_payload(NotifyCategory::Security, "node-1", None)).unwrap();
        assert_eq!(v, serde_json::json!({ "category": "security", "node": "node-1", "session": null }));
    }

    #[test]
    fn the_notify_outer_ignores_the_encrypted_detail() {
        use anycon_protocol::NotifyBody;
        let body = NotifyBody {
            category: NotifyCategory::Error,
            session_id: Some("abc".into()),
            detail: Some("AAAA-ciphertext-the-hub-must-not-look-at".into()),
        };
        let bytes = rmp_serde::to_vec_named(&body).unwrap();
        let outer: NotifyOuter = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!((outer.category, outer.session_id.as_deref()), (NotifyCategory::Error, Some("abc")));
        // 没有 session_id / detail 的也行；类别不认识就解不出
        let bytes =
            rmp_serde::to_vec_named(&NotifyBody { category: NotifyCategory::Security, session_id: None, detail: None })
                .unwrap();
        assert!(rmp_serde::from_slice::<NotifyOuter>(&bytes).is_ok());
        assert!(rmp_serde::from_slice::<NotifyOuter>(b"garbage").is_err());
    }

    #[test]
    fn session_ids_are_restricted_to_safe_characters() {
        assert_eq!(clean_session_id(Some("abc-DEF_1.2".into())).as_deref(), Some("abc-DEF_1.2"));
        for bad in ["", "a/b", "a b", "a?b=c", "../x", "a\u{0}b", "é"] {
            assert_eq!(clean_session_id(Some(bad.into())), None, "{bad:?}");
        }
        assert_eq!(clean_session_id(Some("a".repeat(MAX_SESSION_ID + 1))), None);
        assert_eq!(clean_session_id(None), None);
    }

    #[test]
    fn links_point_at_the_session_or_the_enroll_tab() {
        use NotifyCategory::*;
        let base = "https://hub.example.com";
        assert_eq!(link_for(base, PermissionRequested, "n1", Some("s1")), "https://hub.example.com/n/n1/s/s1");
        assert_eq!(link_for(base, Error, "n1", None), "https://hub.example.com/n/n1");
        assert_eq!(link_for(base, EnrollRequested, "n1", None), "https://hub.example.com/n/n1/settings?tab=enroll");
        assert_eq!(link_for(base, Security, "n1", Some("s1")), "https://hub.example.com/n/n1/settings?tab=enroll");
    }
}
