//! 客户端传输的错误：带「类别」（离线、未配对、参数、一般）和稳定的错误码，调用方（CLI 的退出码、Twin 的错误回应）按类别映射。
//!
//! 文字是写给终端用户看的（英文，提示里会点名 `anycon connect` 之类的命令）：CLI 直接打印它们。

use anycon_protocol::RpcError;

/// 错误的大类。CLI 把它们映射成退出码（一般 = 1、参数 = 2、离线 = 3、未配对 = 4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    General,
    Usage,
    Offline,
    NotPaired,
}

/// 客户端传输的错误。节点回了错误响应时 `rpc` 带着原始的 [`RpcError`]。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ClientError {
    pub class: ErrorClass,
    /// 稳定的错误码：`offline`、`not_paired`、`pair_failed`、`key_mismatch`、`protocol_mismatch`、`timeout`……
    pub code: String,
    pub message: String,
    pub rpc: Option<RpcError>,
    /// 比 `code` 更细的机器可读原因（配对被 hub 拒绝时是 hub 给的原因：`not_found`、`timeout`、`forbidden`……）。
    pub reason: Option<String>,
    /// hub 限速时它建议等多久（`Retry-After`）；只有 [`ClientError::rate_limited`] 会带，hub 没给就是 `None`。
    /// 取整到秒，0 = 没有。不用 `Option`，为的是不让 `ClientError`（到处在 `Result` 里）变大。
    retry_after_secs: u32,
}

/// hub 限速时 [`ClientError::code`] 的值。
pub const RATE_LIMITED_CODE: &str = "rate_limited";

/// hub 限速时 [`ClientError::reason`] 的值（也是 `twin.link` 的 `data.reason`）。
pub const HUB_RATE_LIMITED_REASON: &str = "hub_rate_limited";

impl ClientError {
    /// hub 限速时它建议等多久（`Retry-After`）。
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        (self.retry_after_secs > 0).then(|| std::time::Duration::from_secs(u64::from(self.retry_after_secs)))
    }
    pub fn new(class: ErrorClass, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { class, code: code.into(), message: message.into(), rpc: None, reason: None, retry_after_secs: 0 }
    }
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
    pub fn general(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::General, "error", message)
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Usage, "invalid", message)
    }
    pub fn offline(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Offline, "offline", message)
    }
    /// hub 对 WebSocket 升级限速了（HTTP 429）。归在「离线」类（hub 暂时去不了，稍后可以再试）。
    pub fn rate_limited(hub_url: &str, retry_after: Option<std::time::Duration>) -> Self {
        let wait = match retry_after {
            Some(d) => format!("about {} s", d.as_secs().max(1)),
            None => "a little while".to_string(),
        };
        let mut e = Self::new(
            ErrorClass::Offline,
            RATE_LIMITED_CODE,
            format!("the hub at {hub_url} is limiting new connections from this address; try again in {wait}"),
        )
        .with_reason(HUB_RATE_LIMITED_REASON);
        e.retry_after_secs = retry_after.map_or(0, |d| d.as_secs().clamp(1, u64::from(u32::MAX)) as u32);
        e
    }
    pub fn not_paired(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::NotPaired, "not_paired", message)
    }
    pub fn protocol_mismatch(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::General, "protocol_mismatch", message)
    }
    /// 节点对请求回了错误响应：错误码用协议的稳定错误码（snake_case），没有就是 `error`。
    pub fn from_rpc(e: RpcError) -> Self {
        let code = e
            .stable_code()
            .and_then(|c| serde_json::to_value(c).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "error".into());
        Self {
            class: ErrorClass::General,
            code,
            message: e.message.clone(),
            rpc: Some(e),
            reason: None,
            retry_after_secs: 0,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::general(e.to_string())
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::general(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;
