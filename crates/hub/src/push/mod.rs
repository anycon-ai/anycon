//! Web Push（[ADR-37](../../../docs/decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)，docs/spec/hub.md § 6.1）。
//!
//! - [`vapid`]：服务端密钥对和 JWT（RFC 8292）；
//! - [`webpush`]：载荷加密（RFC 8291，`aes128gcm`）；
//! - 本模块：endpoint 白名单（[`EndpointPolicy`]）和实际发送（[`PushService`]）。
//!
//! hub 是替用户去 POST 一个**浏览器给的 URL** 的一方，所以 endpoint 必须是 https 且主机落在已知推送服务的白名单里，
//! 否则就是 SSRF 跳板；HTTP 客户端不跟随重定向、不读系统代理。日志只记 endpoint 的主机名和状态码——endpoint 的路径本身是个凭据，
//! 所以绝不打印 URL，也不打印 HTTP 客户端的错误信息（它们可能带 URL）。
//!
//! HTTP 客户端是 hyper + hyper-rustls（`ring` + 内置 webpki 根证书），不用 reqwest：reqwest 0.13 一旦打开 rustls 特性，
//! 同一个构建里所有 `reqwest::Client::new()`（cli / daemon 的测试在用）都会因为没装默认 crypto provider 而 panic。

pub mod vapid;
pub mod webpush;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::Full;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::Semaphore;
use url::Url;

use crate::store::PushSubscription;

pub use vapid::{VapidError, VapidKeys};

/// 单个推送请求的默认超时（[`crate::config::Timing::push_timeout`]）。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 全 hub 同时在途的推送请求上限。
pub const MAX_IN_FLIGHT: usize = 16;
/// VAPID JWT 的有效期（RFC 8292 要求不超过 24 小时）。
pub const JWT_LIFETIME: Duration = Duration::from_secs(12 * 3600);
/// endpoint 的长度上限（比常见的 FCM endpoint 长得多）。
pub const MAX_ENDPOINT_LEN: usize = 2048;

/// 推送请求的 `Urgency` 头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    High,
}

impl Urgency {
    fn as_str(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOptions {
    /// `TTL` 头（秒）：推送服务替离线设备保留多久。
    pub ttl: u32,
    pub urgency: Urgency,
}

/// 一次推送的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// 推送服务收下了（2xx）。
    Delivered,
    /// 404 / 410：订阅已经失效，调用方删掉它。
    Gone,
    /// 其它失败（网络错误、超时、别的状态码）。`status` 是 HTTP 状态码，网络错误时没有。
    Failed { status: Option<u16> },
}

/// endpoint 为什么不被接受。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("the push endpoint is not a valid URL")]
    Invalid,
    #[error("the push endpoint must be an https URL on a known push service")]
    NotAllowed,
}

/// 内置的推送服务白名单（docs/spec/hub.md § 6.1）：FCM、Mozilla autopush、Apple、Windows 通知服务。
const EXACT_HOSTS: &[&str] = &["fcm.googleapis.com"];
const SUFFIX_HOSTS: &[&str] = &[".push.services.mozilla.com", ".push.apple.com", ".notify.windows.com"];

/// endpoint 白名单。生产里只有内置名单；测试可以额外放行本机的假推送服务（[`crate::Config::push_allow_hosts`]）。
#[derive(Debug, Clone, Default)]
pub struct EndpointPolicy {
    extra_hosts: Vec<String>,
}

impl EndpointPolicy {
    pub fn new(extra_hosts: Option<Vec<String>>) -> Self {
        Self { extra_hosts: extra_hosts.unwrap_or_default().into_iter().map(|h| h.to_ascii_lowercase()).collect() }
    }

    /// 校验并解析 endpoint。额外放行的主机（只给测试用）允许 http 和任意端口；内置名单要求 https、默认端口、
    /// 没有用户名密码，主机是名单里的精确主机或名单里域名的子域。
    pub fn check(&self, endpoint: &str) -> Result<Url, EndpointError> {
        if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_LEN {
            return Err(EndpointError::Invalid);
        }
        let url = Url::parse(endpoint).map_err(|_| EndpointError::Invalid)?;
        let host = url.host_str().ok_or(EndpointError::Invalid)?.to_ascii_lowercase();
        if self.extra_hosts.contains(&host) && matches!(url.scheme(), "http" | "https") {
            return Ok(url);
        }
        let builtin = EXACT_HOSTS.contains(&host.as_str())
            || SUFFIX_HOSTS.iter().any(|s| host.len() > s.len() && host.ends_with(s) && !host.starts_with('.'));
        let plain = url.scheme() == "https" && url.username().is_empty() && url.password().is_none();
        let port_ok = url.port().is_none() || url.port() == Some(443);
        if builtin && plain && port_ok {
            Ok(url)
        } else {
            Err(EndpointError::NotAllowed)
        }
    }
}

/// endpoint 的主机名（给列表展示和日志用；解析不出就是空串）。
pub fn endpoint_host(endpoint: &str) -> String {
    Url::parse(endpoint).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

/// base64url（容忍多余的 `=` 和标准字母表）解码。
pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let normalized: String = s
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    URL_SAFE_NO_PAD.decode(normalized).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum PushSetupError {
    #[error("cannot build the HTTP client for Web Push: {0}")]
    Client(String),
}

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// 推送发送服务：VAPID 密钥 + HTTP 客户端 + 并发上限。
pub struct PushService {
    vapid: VapidKeys,
    subject: String,
    policy: EndpointPolicy,
    client: HttpsClient,
    slots: Arc<Semaphore>,
    timeout: Duration,
}

impl std::fmt::Debug for PushService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushService")
            .field("vapid", &self.vapid)
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

/// TLS：rustls + `ring` + 内置的 webpki 根证书，不依赖系统证书（distroless 镜像里没有），也不碰 OpenSSL。
pub(crate) fn tls_config() -> Result<rustls::ClientConfig, PushSetupError> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| PushSetupError::Client(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

impl PushService {
    pub fn new(
        vapid: VapidKeys,
        subject: String,
        extra_hosts: Option<Vec<String>>,
        timeout: Duration,
    ) -> Result<Self, PushSetupError> {
        let mut http = HttpConnector::new();
        // 交给 hyper-rustls 判断 scheme（https 走 TLS；只有测试放行的主机才会是 http）。
        http.enforce_http(false);
        http.set_connect_timeout(Some(timeout));
        http.set_nodelay(true);
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config()?)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        // 没有重定向跟随、没有代理：推送服务不会重定向；真的重定向了也不能跟着走（那就绕过了白名单）。
        let client = Client::builder(TokioExecutor::new())
            // 推送是用户触发的、不频繁；连接闲置太久对端多半已经关了，别复用陈旧连接把一条通知丢掉。
            .pool_idle_timeout(Duration::from_secs(20))
            .build(connector);
        Ok(Self {
            vapid,
            subject,
            policy: EndpointPolicy::new(extra_hosts),
            client,
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            timeout,
        })
    }

    /// `/api/config.vapid_public_key`。
    pub fn public_key(&self) -> &str {
        self.vapid.public_key()
    }

    pub fn policy(&self) -> &EndpointPolicy {
        &self.policy
    }

    /// 加密 `payload` 并 POST 到订阅的 endpoint。不返回错误：一切结果都归到 [`PushOutcome`]，日志只记主机名和状态码。
    pub async fn send(&self, sub: &PushSubscription, payload: &[u8], opts: PushOptions) -> PushOutcome {
        // 库里的 endpoint 理论上是校验过的；再校验一次，名单收紧后旧订阅也不会被发出去。
        let Ok(url) = self.policy.check(&sub.endpoint) else {
            tracing::warn!(host = %endpoint_host(&sub.endpoint), "web push: stored endpoint is no longer allowed, skipping");
            return PushOutcome::Failed { status: None };
        };
        let host = url.host_str().unwrap_or("").to_string();
        let (Some(p256dh), Some(auth)) = (decode_b64(&sub.p256dh), decode_b64(&sub.auth)) else {
            tracing::warn!(%host, "web push: stored subscription keys are not valid base64url");
            return PushOutcome::Failed { status: None };
        };
        let body = match webpush::encrypt_fresh(payload, &p256dh, &auth) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(%host, error = %e, "web push: cannot encrypt the payload for this subscription");
                return PushOutcome::Failed { status: None };
            }
        };
        let expires =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) + JWT_LIFETIME.as_secs();
        let jwt = self.vapid.jwt(&url.origin().ascii_serialization(), expires, &self.subject);

        let request = hyper::Request::post(url.as_str())
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", opts.ttl.to_string())
            .header("Urgency", opts.urgency.as_str())
            .header("Authorization", format!("vapid t={jwt}, k={}", self.vapid.public_key()))
            .header("User-Agent", concat!("anycon-hub/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body)));
        let Ok(request) = request else {
            tracing::warn!(%host, "web push: cannot build the request");
            return PushOutcome::Failed { status: None };
        };

        let Ok(_permit) = self.slots.clone().acquire_owned().await else {
            return PushOutcome::Failed { status: None };
        };
        // 超时算整个「连接 + 发送 + 等响应头」；不读响应体。
        match tokio::time::timeout(self.timeout, self.client.request(request)).await {
            Ok(Ok(response)) => {
                let status = response.status();
                let code = status.as_u16();
                if status.is_success() {
                    tracing::debug!(%host, status = code, "web push delivered to the push service");
                    PushOutcome::Delivered
                } else if code == 404 || code == 410 {
                    tracing::info!(%host, status = code, "web push: the subscription is gone, removing it");
                    PushOutcome::Gone
                } else {
                    tracing::warn!(%host, status = code, "web push rejected by the push service");
                    PushOutcome::Failed { status: Some(code) }
                }
            }
            Ok(Err(e)) => {
                // 不打印 `e`：客户端的错误信息可能带 URL，而 endpoint 的路径是凭据。
                let kind = if e.is_connect() { "connect" } else { "request" };
                tracing::warn!(%host, kind, "web push request failed");
                PushOutcome::Failed { status: None }
            }
            Err(_) => {
                tracing::warn!(%host, kind = "timeout", "web push request failed");
                PushOutcome::Failed { status: None }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> EndpointPolicy {
        EndpointPolicy::new(None)
    }

    #[test]
    fn the_built_in_push_services_are_allowed() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc:def",
            "https://fcm.googleapis.com:443/fcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
            "https://updates-autopush.stage.mozaws.net.push.services.mozilla.com/x",
            "https://web.push.apple.com/QWERTY",
            "https://wns2-par02p.notify.windows.com/w/?token=abc",
            "https://HTTPS-UPPER.PUSH.APPLE.COM/x",
        ] {
            assert!(builtin().check(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for bad in [
            "http://fcm.googleapis.com/fcm/send/abc",               // not https
            "https://fcm.googleapis.com.evil.example/fcm/send/abc", // suffix trick
            "https://evilfcm.googleapis.com/x",                     // not exactly fcm.googleapis.com
            "https://fcm.googleapis.com@evil.example/x",            // userinfo
            "https://user:pw@fcm.googleapis.com/x",                 // credentials
            "https://fcm.googleapis.com:8443/x",                    // odd port
            "https://push.apple.com/x",                             // the bare suffix domain itself
            "https://.push.apple.com/x",
            "https://notify.windows.com/x",
            "https://evil.example/x",
            "https://127.0.0.1/x",
            "https://localhost/x",
            "https://10.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest/meta-data",
            "https://example.com/?u=https://fcm.googleapis.com/",
            "ftp://fcm.googleapis.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "//fcm.googleapis.com/x",
            "fcm.googleapis.com/x",
            "",
            "not a url",
        ] {
            assert!(builtin().check(bad).is_err(), "{bad}");
        }
        let too_long = format!("https://fcm.googleapis.com/{}", "a".repeat(MAX_ENDPOINT_LEN));
        assert_eq!(builtin().check(&too_long), Err(EndpointError::Invalid));
        assert_eq!(builtin().check("https://evil.example/x"), Err(EndpointError::NotAllowed));
        assert_eq!(builtin().check("nope"), Err(EndpointError::Invalid));
    }

    #[test]
    fn test_hosts_are_additive_and_exact() {
        let p = EndpointPolicy::new(Some(vec!["127.0.0.1".into(), "Fake-Push.TEST".into()]));
        assert!(p.check("http://127.0.0.1:41234/push/abc").is_ok());
        assert!(p.check("https://127.0.0.1/push/abc").is_ok());
        assert!(p.check("http://fake-push.test:9/x").is_ok(), "host names are case-insensitive");
        assert!(p.check("http://127.0.0.2:1/x").is_err(), "only the listed hosts");
        assert!(p.check("http://localhost:1/x").is_err());
        assert!(p.check("ftp://127.0.0.1/x").is_err());
        assert!(p.check("https://fcm.googleapis.com/x").is_ok(), "the built-in list still applies");
        assert!(p.check("http://fcm.googleapis.com/x").is_err(), "and still requires https");
    }

    #[test]
    fn hosts_and_base64_helpers() {
        assert_eq!(endpoint_host("https://fcm.googleapis.com/fcm/send/secret-path"), "fcm.googleapis.com");
        assert_eq!(endpoint_host("not a url"), "");
        assert_eq!(decode_b64("AQID").unwrap(), vec![1, 2, 3]);
        assert_eq!(decode_b64("AQID=").unwrap(), vec![1, 2, 3], "padding is tolerated");
        assert_eq!(decode_b64("-_-_").unwrap(), decode_b64("+/+/").unwrap(), "standard alphabet is tolerated");
        assert!(decode_b64("!!!").is_none());
    }

    #[test]
    fn the_service_builds_with_the_bundled_roots() {
        // 只验证 TLS 配置和客户端能建起来（真正的 https 往返没法在不联网的测试里做）。
        let svc =
            PushService::new(VapidKeys::generate(), "mailto:a@example.com".into(), None, REQUEST_TIMEOUT).unwrap();
        assert!(svc.public_key().len() >= 86);
        assert!(format!("{svc:?}").contains("PushService"));
    }
}
