//! 客户端传输的错误：带「类别」（离线、未配对、参数、一般）和稳定的错误码，调用方（CLI 的退出码、分身的错误回应）按类别映射。
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
}

impl ClientError {
    pub fn new(class: ErrorClass, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { class, code: code.into(), message: message.into(), rpc: None, reason: None }
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
        Self { class: ErrorClass::General, code, message: e.message.clone(), rpc: Some(e), reason: None }
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
