//! CLI 错误与退出码（docs/spec/cli.md § 4）：0 成功；1 一般错误；2 参数错误；3 节点离线；4 未配对 / 未初始化；5 权限被拒；130 被 Ctrl+C 中断。

use anycon_protocol::{ErrorCode, RpcError};

pub const EXIT_ERROR: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_OFFLINE: u8 = 3;
pub const EXIT_NOT_PAIRED: u8 = 4;
pub const EXIT_DENIED: u8 = 5;
pub const EXIT_INTERRUPTED: u8 = 130;

#[derive(Debug)]
pub struct CliError {
    pub exit: u8,
    /// 稳定的错误码（和协议错误码一致，或 cli 自己的：`offline`、`not_initialized`…）
    pub code: String,
    pub message: String,
    /// daemon 错误的 `data.reason`（`busy` + `quota` 之类）：同一个错误码下的细分原因。不是 RPC 错误时为 `None`。
    pub reason: Option<String>,
}

impl CliError {
    pub fn new(exit: u8, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { exit, code: code.into(), message: message.into(), reason: None }
    }
    pub fn general(message: impl Into<String>) -> Self {
        Self::new(EXIT_ERROR, "error", message)
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, "invalid", message)
    }
    pub fn offline(message: impl Into<String>) -> Self {
        Self::new(EXIT_OFFLINE, "offline", message)
    }
    pub fn not_initialized(message: impl Into<String>) -> Self {
        Self::new(EXIT_NOT_PAIRED, "not_initialized", message)
    }
    pub fn not_paired(message: impl Into<String>) -> Self {
        Self::new(EXIT_NOT_PAIRED, "not_paired", message)
    }
    pub fn protocol_mismatch(message: impl Into<String>) -> Self {
        Self::new(EXIT_ERROR, "protocol_mismatch", message)
    }
    /// 被拒绝（退出码 5）：账号内接入的码 / 口令不对、请求被拒绝、方式没开启、退避中、已锁定。
    pub fn denied(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(EXIT_DENIED, code, message)
    }
    pub fn interrupted() -> Self {
        Self::new(EXIT_INTERRUPTED, "interrupted", "interrupted")
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CliError {}

fn code_str(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::Invalid => "invalid",
        ErrorCode::InvalidSettings => "invalid_settings",
        ErrorCode::AgentUnavailable => "agent_unavailable",
        ErrorCode::NodeBusy => "node_busy",
        ErrorCode::Busy => "busy",
        ErrorCode::AlreadyResolved => "already_resolved",
        ErrorCode::Exists => "exists",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Forbidden => "forbidden",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::ProtocolMismatch => "protocol_mismatch",
        ErrorCode::Internal => "internal",
    }
}

impl From<RpcError> for CliError {
    fn from(e: RpcError) -> Self {
        let code = e.stable_code();
        let mut msg = e.message.clone();
        if let Some(d) = &e.data {
            if let (Some(f), Some(r)) = (&d.field, &d.reason) {
                msg = format!("{f}: {r}");
            } else if let Some(r) = &d.reason {
                msg = format!("{msg} ({r})");
            }
        }
        // spec/cli.md § 4：参数不对（含不合法的设置）= 2，被拒绝 = 5，其余 = 1
        let exit = match code {
            Some(ErrorCode::Forbidden) => EXIT_DENIED,
            Some(ErrorCode::Invalid) | Some(ErrorCode::InvalidSettings) => EXIT_USAGE,
            _ => EXIT_ERROR,
        };
        let reason = e.data.as_ref().and_then(|d| d.reason.clone());
        CliError { exit, code: code.map(code_str).unwrap_or("error").to_string(), message: msg, reason }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::general(e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::general(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_errors_map_to_codes_and_exit_status() {
        let e: CliError = RpcError::new(ErrorCode::Forbidden, "no").into();
        assert_eq!((e.exit, e.code.as_str()), (EXIT_DENIED, "forbidden"));
        let e: CliError = RpcError::invalid_settings("model", "unknown model x").into();
        assert_eq!((e.exit, e.code.as_str()), (EXIT_USAGE, "invalid_settings"), "bad settings are a usage error");
        assert_eq!(e.message, "model: unknown model x");
        assert_eq!(e.to_string(), "invalid_settings: model: unknown model x");
        let e: CliError = RpcError::new(ErrorCode::NotFound, "gone").into();
        assert_eq!((e.exit, e.code.as_str()), (EXIT_ERROR, "not_found"));
    }
}
