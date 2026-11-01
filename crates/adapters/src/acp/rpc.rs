//! ACP 的 JSON-RPC 2.0 层（stdio，换行分隔，**带 `jsonrpc: "2.0"` 头**）。
//!
//! - [`classify`]：把 stdout 的一行分成响应 / agent 发来的请求 / 通知（纯函数）
//! - [`Rpc`]：我们发出的请求与响应的对应（按 id 等待），以及对 agent 请求的答复
//!
//! 不解释任何 method 的含义；探测会话（`probe`）和正式连接（`conn`）共用。我们自己的请求 id 都是整数；agent 发来的请求
//! id 可以是整数或字符串，答复时原样带回。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::common::JsonlChild;

/// 认证错误：请求要先 `authenticate`（或者用户要先在机器上登录）。
pub const AUTH_REQUIRED: i64 = -32000;
/// 引用的资源（比如会话）不存在。
pub const RESOURCE_NOT_FOUND: i64 = -32002;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// 请求被取消（`session/prompt` 被取消时有的 agent 回这个错误而不是 `cancelled` 的 stop reason）。
pub const REQUEST_CANCELLED: i64 = -32800;

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn to_json(&self) -> Value {
        match &self.data {
            Some(d) => json!({ "code": self.code, "message": self.message, "data": d }),
            None => json!({ "code": self.code, "message": self.message }),
        }
    }
}

/// stdout 的一行。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// 对我们某个请求的答复。
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    /// agent 发给我们的请求，必须答复（否则它会一直等）。`id` 原样回传。
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// 分类一行。不是 JSON 对象 / 形状不认识 = `None`（调用方忽略）。
pub fn classify(line: &str) -> Option<Incoming> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").filter(|i| !i.is_null());
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(m), Some(id)) => Some(Incoming::Request { id: id.clone(), method: m.to_string(), params }),
        (Some(m), None) => Some(Incoming::Notification { method: m.to_string(), params }),
        (None, Some(id)) => {
            let id = id.as_u64()?;
            if let Some(err) = obj.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                let data = err.get("data").filter(|d| !d.is_null()).cloned();
                Some(Incoming::Response { id, result: Err(RpcError { code, message, data }) })
            } else {
                Some(Incoming::Response { id, result: Ok(obj.get("result").cloned().unwrap_or(Value::Null)) })
            }
        }
        (None, None) => None,
    }
}

/// 一次请求失败的原因。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcFailure {
    #[error("{}", .0.message)]
    Rpc(RpcError),
    #[error("timed out waiting for the agent")]
    Timeout,
    #[error("the agent process is not running")]
    Closed,
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

/// 已经写出去、还在等答复的请求。
pub struct Pending {
    id: u64,
    rx: oneshot::Receiver<Result<Value, RpcError>>,
}

impl Pending {
    /// 一直等到答复（`session/prompt` 一个 turn 可以跑很久，没有超时）。进程退出 = `Closed`。
    pub async fn wait(self) -> Result<Value, RpcFailure> {
        match self.rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(RpcFailure::Rpc(e)),
            Err(_) => Err(RpcFailure::Closed),
        }
    }
}

/// 请求 / 响应关联。读任务收到 [`Incoming::Response`] 就调 [`Rpc::resolve`]；进程退出时调 [`Rpc::close`]。
pub struct Rpc {
    child: Arc<JsonlChild>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
    closed: AtomicBool,
}

impl Rpc {
    pub fn new(child: Arc<JsonlChild>) -> Self {
        Self { child, next_id: AtomicU64::new(1), pending: Mutex::new(HashMap::new()), closed: AtomicBool::new(false) }
    }

    fn forget(&self, id: u64) {
        self.pending.lock().expect("rpc pending lock").remove(&id);
    }

    /// 写出一个请求，立刻返回（不等答复）。写不出去（进程已经没了）= `Closed`。
    pub fn start(&self, method: &str, params: Value) -> Result<Pending, RpcFailure> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RpcFailure::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("rpc pending lock").insert(id, tx);
        // `close()` 可能刚好在上面两步之间跑完：再查一次，免得永远等不到
        if self.closed.load(Ordering::SeqCst) {
            self.forget(id);
            return Err(RpcFailure::Closed);
        }
        if !self.child.send_json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })) {
            self.forget(id);
            return Err(RpcFailure::Closed);
        }
        Ok(Pending { id, rx })
    }

    /// 发一个请求并等答复，最多 `timeout`。
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcFailure> {
        let pending = self.start(method, params)?;
        let id = pending.id;
        match tokio::time::timeout(timeout, pending.wait()).await {
            Ok(r) => r,
            Err(_) => {
                self.forget(id);
                Err(RpcFailure::Timeout)
            }
        }
    }

    /// 发一个通知（没有 id，不等答复）。
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.child.send_json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// 收到响应。没人等的（超时后才到的）丢弃。
    pub fn resolve(&self, id: u64, result: Result<Value, RpcError>) {
        let waiter = self.pending.lock().expect("rpc pending lock").remove(&id);
        match waiter {
            Some(w) => {
                let _ = w.send(result);
            }
            None => tracing::debug!(id, "acp: response with no waiter"),
        }
    }

    /// 进程退出：所有等待者立刻得到 `Closed`，之后的请求直接失败。
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().expect("rpc pending lock").clear();
    }

    /// 答复 agent 的请求（成功）。`id` 原样回传。
    pub fn reply(&self, id: &Value, result: Value) -> bool {
        self.child.send_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// 答复 agent 的请求（JSON-RPC 错误）。
    pub fn reply_error(&self, id: &Value, error: &RpcError) -> bool {
        self.child.send_json(&json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_three_shapes() {
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s"}}"#),
            Some(Incoming::Response { id: 3, result: Ok(json!({"sessionId": "s"})) })
        );
        assert_eq!(
            classify(
                r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"Authentication required","data":{"x":1}}}"#
            ),
            Some(Incoming::Response {
                id: 4,
                result: Err(RpcError {
                    code: -32000,
                    message: "Authentication required".into(),
                    data: Some(json!({"x": 1}))
                })
            })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"s"}}"#),
            Some(Incoming::Request {
                id: json!(0),
                method: "session/request_permission".into(),
                params: json!({"sessionId": "s"})
            })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#),
            Some(Incoming::Notification { method: "session/update".into(), params: json!({"sessionId": "s"}) })
        );
    }

    #[test]
    fn string_request_ids_are_kept_and_junk_is_ignored() {
        match classify(r#"{"jsonrpc":"2.0","method":"x","id":"abc","params":{}}"#) {
            Some(Incoming::Request { id, .. }) => assert_eq!(id, json!("abc")),
            other => panic!("{other:?}"),
        }
        for junk in ["not json", "", "[1,2]", r#"{"hello":"world"}"#, r#"{"id":"x","result":{}}"#] {
            assert_eq!(classify(junk), None, "{junk:?}");
        }
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","method":"initialized"}"#),
            Some(Incoming::Notification { method: "initialized".into(), params: Value::Null })
        );
        // `id: null` 的答复不是我们发的
        assert_eq!(classify(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse"}}"#), None);
    }
}
