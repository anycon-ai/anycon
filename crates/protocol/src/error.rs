//! 稳定错误码。客户端据 `code` 做 i18n，`message` 是英文调试信息。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 参数不合法（通用）。
    #[error("invalid params")]
    Invalid,
    /// 会话设置不被 agent 接受，`data.field` 指出字段。
    #[error("invalid settings")]
    InvalidSettings,
    /// agent 未安装 / 未登录 / 被禁用，`data.reason` 是 AgentStatus。
    #[error("agent unavailable")]
    AgentUnavailable,
    /// 并发上限且未排队。
    #[error("node busy")]
    NodeBusy,
    /// turn 进行中不能做这个操作。
    #[error("busy")]
    Busy,
    /// 权限请求已被答复。
    #[error("already resolved")]
    AlreadyResolved,
    #[error("exists")]
    Exists,
    #[error("not found")]
    NotFound,
    /// 被配置禁止（如危险模式）或越过 workspace 边界。
    #[error("forbidden")]
    Forbidden,
    /// adapter 不支持该能力。
    #[error("unsupported")]
    Unsupported,
    /// 协议版本不兼容。
    #[error("protocol mismatch")]
    ProtocolMismatch,
    #[error("internal error")]
    Internal,
}

impl ErrorCode {
    /// JSON-RPC 的数字 code。-32000..-32099 是服务端自定义区间。
    pub fn rpc_code(self) -> i64 {
        match self {
            ErrorCode::Invalid => -32602,
            ErrorCode::InvalidSettings => -32001,
            ErrorCode::AgentUnavailable => -32002,
            ErrorCode::NodeBusy => -32003,
            ErrorCode::Busy => -32004,
            ErrorCode::AlreadyResolved => -32005,
            ErrorCode::Exists => -32006,
            ErrorCode::NotFound => -32007,
            ErrorCode::Forbidden => -32008,
            ErrorCode::Unsupported => -32009,
            ErrorCode::ProtocolMismatch => -32010,
            ErrorCode::Internal => -32603,
        }
    }
}
