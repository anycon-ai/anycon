//! 内层 JSON-RPC 2.0。只定义信封；各方法的 params / result 在各域模块。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ErrorCode;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RpcId {
    Num(u64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcErrorData {
    pub code: ErrorCode,
    /// `invalid_settings` 时的字段名等补充信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// 带条件的写被拒（`todo.delete` 的 `if_updated_ms` 对不上，`reason = "changed"`）时，那一项现在的 `updated_ms`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_ms: Option<u64>,
    /// 规则超出注入的字数上限（`rule.add` / `rule.update` 的 `invalid`，`reason = "over_budget"`）时，这次改动之后一个会话拿到的
    /// 规则文字的字节数，和上限（`cap`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u64>,
    /// 判断后端当天已经使用的次数与每日上限（reason = budget）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    // 装在 `Box` 里：`RpcError` 是几乎每个函数的 `Err` 类型，太大会让 clippy 的 `result_large_err` 在一路上的函数都报警。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<RpcErrorData>>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.rpc_code(),
            message: message.into(),
            data: Some(Box::new(RpcErrorData {
                code,
                field: None,
                reason: None,
                retry_after_ms: None,
                updated_ms: None,
                bytes: None,
                cap: None,
                used: None,
                limit: None,
            })),
        }
    }
    pub fn invalid_settings(field: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorCode::InvalidSettings, "invalid settings");
        if let Some(d) = e.data.as_mut() {
            d.field = Some(field.into());
            d.reason = Some(reason.into());
        }
        e
    }
    pub fn stable_code(&self) -> Option<ErrorCode> {
        self.data.as_ref().map(|d| d.code)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: RpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// 一条内层消息。解码时按字段判别：有 `method` 且有 `id` 是请求，有 `method` 无 `id` 是通知，否则是响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(RpcRequest),
    Notification(RpcNotification),
    Response(RpcResponse),
}

impl RpcMessage {
    pub fn request(id: impl Into<RpcId>, method: impl Into<String>, params: impl Serialize) -> Self {
        RpcMessage::Request(RpcRequest {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        })
    }
    pub fn notification(method: impl Into<String>, params: impl Serialize) -> Self {
        RpcMessage::Notification(RpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        })
    }
    pub fn result(id: RpcId, result: impl Serialize) -> Self {
        RpcMessage::Response(RpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        })
    }
    pub fn error(id: RpcId, error: RpcError) -> Self {
        RpcMessage::Response(RpcResponse { jsonrpc: "2.0".into(), id, result: None, error: Some(error) })
    }
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("rpc message is always serializable")
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        // untagged 会按顺序试：Request 需要 id+method；Notification 需要 method 且无 id（serde 不会拒绝多余字段，
        // 所以 Request 排在前面保证带 id 的先匹配）；剩下的是 Response。
        serde_json::from_slice(bytes)
    }
}

impl From<u64> for RpcId {
    fn from(v: u64) -> Self {
        RpcId::Num(v)
    }
}
impl From<String> for RpcId {
    fn from(v: String) -> Self {
        RpcId::Str(v)
    }
}
impl From<&str> for RpcId {
    fn from(v: &str) -> Self {
        RpcId::Str(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RpcError` 是几乎每个函数的 `Err` 类型：超过 clippy 的 `result_large_err` 门槛（128 字节）会让一路上的函数都被警告。
    #[test]
    fn an_rpc_error_stays_small_enough_to_be_returned_everywhere() {
        assert!(std::mem::size_of::<RpcError>() < 128, "{}", std::mem::size_of::<RpcError>());
    }

    #[test]
    fn classify() {
        let req = RpcMessage::request(1u64, "node.info", serde_json::json!({}));
        let bytes = req.encode();
        assert!(matches!(RpcMessage::decode(&bytes).unwrap(), RpcMessage::Request(_)));
        let n = RpcMessage::notification("session.event", serde_json::json!({"x": 1}));
        assert!(matches!(RpcMessage::decode(&n.encode()).unwrap(), RpcMessage::Notification(_)));
        let r = RpcMessage::result(RpcId::Num(1), serde_json::json!({"ok": true}));
        assert!(matches!(RpcMessage::decode(&r.encode()).unwrap(), RpcMessage::Response(_)));
        let e = RpcMessage::error(RpcId::Num(2), RpcError::new(ErrorCode::NotFound, "no"));
        assert!(matches!(
            RpcMessage::decode(&e.encode()).unwrap(),
            RpcMessage::Response(RpcResponse { error: Some(_), .. })
        ));
    }
}
