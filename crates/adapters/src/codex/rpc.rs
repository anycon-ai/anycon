//! app-server 的 JSON-RPC 层：换行分隔、**省略 `jsonrpc` 头**。
//!
//! - [`classify`]：把 stdout 的一行分成响应 / 服务端请求 / 通知（纯函数）
//! - [`RpcClient`]：我们发出的请求与响应的对应（按 id 等待，带超时），以及对服务端请求的答复
//!
//! 不解释任何 method 的含义；探测会话（`probe`）和正式会话（`handle`）共用。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::common::JsonlChild;

/// JSON-RPC 错误对象（`{ code, message }`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

/// `-32601 method not found`。
pub const METHOD_NOT_FOUND: i64 = -32601;
/// `-32603 internal error`。
pub const INTERNAL_ERROR: i64 = -32603;

/// stdout 的一行。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// 对我们某个请求的答复（我们的 id 都是整数）。
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    /// 服务端发给我们的请求，必须答复（否则 turn 会卡住）。`id` 可能是整数或字符串，原样回传。
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
    classify_value(&v)
}

pub fn classify_value(v: &Value) -> Option<Incoming> {
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
                Some(Incoming::Response { id, result: Err(RpcError { code, message }) })
            } else {
                let result = obj.get("result").cloned().unwrap_or(Value::Null);
                Some(Incoming::Response { id, result: Ok(result) })
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
    #[error("timed out waiting for the codex app-server")]
    Timeout,
    #[error("the codex app-server is not running")]
    Closed,
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

/// 请求 / 响应关联。读任务收到 [`Incoming::Response`] 就调 [`RpcClient::resolve`]；进程退出时调 [`RpcClient::close`]。
pub struct RpcClient {
    child: Arc<JsonlChild>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
    closed: AtomicBool,
}

impl RpcClient {
    pub fn new(child: Arc<JsonlChild>) -> Self {
        Self { child, next_id: AtomicU64::new(1), pending: Mutex::new(HashMap::new()), closed: AtomicBool::new(false) }
    }

    /// 发一个请求并等响应。
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcFailure> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RpcFailure::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("rpc pending lock").insert(id, tx);
        // `close()` 可能刚好在上面两步之间跑完：再查一次，免得永远等不到
        if self.closed.load(Ordering::SeqCst) {
            self.pending.lock().expect("rpc pending lock").remove(&id);
            return Err(RpcFailure::Closed);
        }
        if !self.child.send_json(&json!({ "id": id, "method": method, "params": params })) {
            self.pending.lock().expect("rpc pending lock").remove(&id);
            return Err(RpcFailure::Closed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(RpcFailure::Rpc(e)),
            Ok(Err(_)) => Err(RpcFailure::Closed),
            Err(_) => {
                self.pending.lock().expect("rpc pending lock").remove(&id);
                Err(RpcFailure::Timeout)
            }
        }
    }

    /// 发一个通知（没有 id，不等答复）。
    pub fn notify(&self, method: &str) -> bool {
        self.child.send_json(&json!({ "method": method }))
    }

    /// 收到响应。没人等的（超时后才到的）丢弃。
    pub fn resolve(&self, id: u64, result: Result<Value, RpcError>) {
        let waiter = self.pending.lock().expect("rpc pending lock").remove(&id);
        match waiter {
            Some(w) => {
                let _ = w.send(result);
            }
            None => tracing::debug!(id, "codex: response with no waiter"),
        }
    }

    /// 进程退出：所有等待者立刻得到 `Closed`。
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().expect("rpc pending lock").clear();
    }

    /// 答复服务端请求（成功）。`id` 原样回传。
    pub fn reply(&self, id: &Value, result: Value) -> bool {
        self.child.send_json(&json!({ "id": id, "result": result }))
    }

    /// 答复服务端请求（JSON-RPC 错误）。
    pub fn reply_error(&self, id: &Value, error: &RpcError) -> bool {
        self.child.send_json(&json!({ "id": id, "error": error.to_json() }))
    }
}

// ---------- 超长行的「预览」 ----------

/// 一条超长的行（只有它的前几 KiB）看起来是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OversizeKind {
    /// 通知（有 `method`、没有顶层 `id`）：可以丢弃。
    Notification { method: String },
    /// 服务端请求（有 `method` 和顶层 `id`）：必须答复，丢了 turn 会卡死 → 协议错误。
    Request { method: String },
    /// 对我们某个请求的响应（顶层 `id` + `result`）：丢了调用方就一直等 → 协议错误。
    Response,
    /// 前缀里看不出来 → 保守地当协议错误。
    Unknown,
}

/// 分类一条超长的行。只看顶层的键：Codex 的序列化顺序是通知 `{"method","params",…}`、请求 `{"method","id","params"}`、
/// 响应 `{"id","result"}`，`params` / `result` 这种大头都在后面，所以前缀里一定能看到 `method` / `id`。
pub fn classify_oversize(prefix: &str) -> OversizeKind {
    let keys = object_keys(prefix, &[]);
    let has = |k: &str| keys.iter().any(|(key, _)| key == k);
    let method = keys.iter().find(|(k, _)| k == "method").and_then(|(_, v)| v.clone());
    match method {
        Some(method) if has("id") => OversizeKind::Request { method },
        Some(method) => OversizeKind::Notification { method },
        None if has("id") && (has("result") || has("error")) => OversizeKind::Response,
        None => OversizeKind::Unknown,
    }
}

/// `item/started` / `item/completed` 的 `params.item` 里的 `type` 和 `id`（`item` 的键顺序是 `type`、`id`、…，大字段在后面）。
pub fn oversize_item(prefix: &str) -> Option<(String, String)> {
    let keys = object_keys(prefix, &["params", "item"]);
    let get = |k: &str| keys.iter().find(|(key, _)| key == k).and_then(|(_, v)| v.clone());
    Some((get("type")?, get("id")?))
}

/// 前缀里按 `keys` 依次出现的第一个 `"<key>": "<字符串>"` 的值：先找 `keys[0]`，再从那之后找 `keys[1]`…，最后一个键后面必须是字符串
/// （不管嵌套在哪一层、冒号两边有没有空白）。用来从超长补丁的开头抠出第一个文件的路径和种类（`["path"]`、`["kind", "type"]`）。
pub fn string_at(prefix: &str, keys: &[&str]) -> Option<String> {
    let mut from = 0;
    let mut value = None;
    for (n, key) in keys.iter().enumerate() {
        let needle = format!("\"{key}\"");
        // 找到后面确实跟着 `:` 的那一次出现（值里的同名字符串不算）
        let mut search = from;
        loop {
            let at = search + prefix.get(search..)?.find(&needle)?;
            let mut sc = Scanner { s: prefix.as_bytes(), i: at + needle.len() };
            match sc.eat(b':') {
                Ok(true) => {
                    from = sc.i;
                    if n + 1 == keys.len() {
                        sc.ws();
                        value = sc.string().ok();
                    }
                    break;
                }
                _ => search = at + needle.len(),
            }
        }
    }
    value
}

/// 容错的 JSON 对象扫描器：输入可能在任何位置被截断。
struct Scanner<'a> {
    s: &'a [u8],
    i: usize,
}

/// `Err(())` = 输入在这里被截断了。
type Scan<T> = Result<T, ()>;

impl Scanner<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Scan<u8> {
        self.ws();
        self.s.get(self.i).copied().ok_or(())
    }

    fn eat(&mut self, c: u8) -> Scan<bool> {
        if self.peek()? == c {
            self.i += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 读一个字符串（`i` 指向开头的引号），返回解码后的内容。
    fn string(&mut self) -> Scan<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return Err(());
        }
        let start = self.i + 1;
        let mut j = start;
        while j < self.s.len() {
            match self.s[j] {
                b'\\' => j += 2,
                b'"' => {
                    let raw = String::from_utf8_lossy(&self.s[start..j]).into_owned();
                    self.i = j + 1;
                    // 借 serde 处理转义；不合法就原样
                    return Ok(serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or(raw));
                }
                _ => j += 1,
            }
        }
        Err(())
    }

    /// 跳过一个值。
    fn skip_value(&mut self) -> Scan<()> {
        match self.peek()? {
            b'"' => self.string().map(|_| ()),
            b'{' | b'[' => {
                let mut depth = 0usize;
                while self.i < self.s.len() {
                    match self.s[self.i] {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                Err(())
            }
            _ => {
                // 数字 / true / false / null：读到分隔符；在末尾被截断就算截断
                while self.i < self.s.len() && !matches!(self.s[self.i], b',' | b'}' | b']') {
                    self.i += 1;
                }
                if self.i >= self.s.len() {
                    Err(())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// 扫描 `{…}`，按 `path` 下降到目标对象，收集目标对象各个键（和字符串值）。
    fn keys(&mut self, path: &[&str], out: &mut Vec<(String, Option<String>)>) -> Scan<()> {
        if !self.eat(b'{')? {
            return Err(());
        }
        loop {
            if self.eat(b'}')? {
                return Ok(());
            }
            if self.eat(b',')? {
                continue;
            }
            self.ws();
            let key = self.string()?;
            if !self.eat(b':')? {
                return Err(());
            }
            match path.split_first() {
                None => {
                    let value = if self.peek()? == b'"' {
                        let v = self.string();
                        out.push((key, v.as_ref().ok().cloned()));
                        v.map(|_| ())
                    } else {
                        out.push((key, None));
                        self.skip_value()
                    };
                    value?;
                }
                Some((head, rest)) => {
                    if key == *head && self.peek()? == b'{' {
                        self.keys(rest, out)?;
                        if !out.is_empty() {
                            return Ok(());
                        }
                    } else {
                        self.skip_value()?;
                    }
                }
            }
        }
    }
}

/// 前缀里 `path` 所指对象的键（和字符串值）。前缀被截断时返回截断之前读到的。
fn object_keys(prefix: &str, path: &[&str]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut sc = Scanner { s: prefix.as_bytes(), i: 0 };
    let _ = sc.keys(path, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix_of(v: &Value, keep: usize) -> String {
        let s = v.to_string();
        s[..keep.min(s.len())].to_string()
    }

    #[test]
    fn oversize_lines_are_classified_from_their_first_bytes() {
        let big = "x".repeat(10_000);
        // 通知：method 在前，params 是大头；params 里的 `id` 不算顶层
        let note = json!({"method": "turn/diff/updated", "params": {"threadId": "t", "id": "nested", "diff": big}});
        assert_eq!(
            classify_oversize(&prefix_of(&note, 300)),
            OversizeKind::Notification { method: "turn/diff/updated".into() }
        );
        // 请求：id 紧跟在 method 后面
        let req = json!({"method": "item/fileChange/requestApproval", "id": 7, "params": {"x": big}});
        assert_eq!(
            classify_oversize(&prefix_of(&req, 300)),
            OversizeKind::Request { method: "item/fileChange/requestApproval".into() }
        );
        // 响应
        let resp = json!({"id": 3, "result": {"data": big}});
        assert_eq!(classify_oversize(&prefix_of(&resp, 300)), OversizeKind::Response);
        // 看不出来
        assert_eq!(classify_oversize("{\"params\":{\"a\":\"zzzz"), OversizeKind::Unknown);
        assert_eq!(classify_oversize("not json at all"), OversizeKind::Unknown);
        assert_eq!(classify_oversize(""), OversizeKind::Unknown);
        // 键顺序反过来（params 在前）但 method 还在前缀里
        let odd = format!("{{\"params\":{{\"k\":\"v\"}},\"method\":\"a/b\",\"emittedAtMs\":1,\"pad\":\"{big}\"}}");
        assert_eq!(classify_oversize(&odd[..200]), OversizeKind::Notification { method: "a/b".into() });
    }

    #[test]
    fn truncation_in_the_middle_of_anything_is_tolerated() {
        let line = json!({"method": "m\"q", "id": 1.5e3, "params": {"a": [1, {"b": "c\\d"}], "s": "str"}}).to_string();
        for cut in 0..line.len() {
            // 任何位置截断都不能 panic
            let _ = classify_oversize(&line[..cut]);
            let _ = oversize_item(&line[..cut]);
        }
        // 转义过的 method 被还原
        assert_eq!(classify_oversize(&line), OversizeKind::Request { method: "m\"q".into() });
    }

    #[test]
    fn item_type_and_id_are_found_in_the_nested_item() {
        // 和 Codex 一样的键顺序（serde_json 的 `json!` 会按字母序重排，所以手写）
        let diff = "y".repeat(9000);
        let item = format!(
            "{{\"method\":\"item/started\",\"params\":{{\"item\":{{\"type\":\"fileChange\",\"id\":\"call_9\",\"changes\":[{{\"path\":\"/p/big.txt\",\"kind\":{{\"type\":\"add\"}},\"diff\":\"{diff}\"}}],\"status\":\"inProgress\"}},\"threadId\":\"t\"}}}}"
        );
        let prefix = item[..400].to_string();
        assert_eq!(oversize_item(&prefix), Some(("fileChange".into(), "call_9".into())));
        assert_eq!(string_at(&prefix, &["path"]).as_deref(), Some("/p/big.txt"));
        assert_eq!(string_at(&prefix, &["kind", "type"]).as_deref(), Some("add"));
        // 冒号两边有空白也行（别的序列化器 / 重新格式化过的流量）
        assert_eq!(string_at("{ \"changes\": [ { \"path\" : \"/x y/z\" } ] }", &["path"]).as_deref(), Some("/x y/z"));
        // 前缀太短，还没读到 id
        assert_eq!(oversize_item(&item[..70]), None);
        // 不是 item 通知
        assert_eq!(
            oversize_item(&prefix_of(&json!({"method": "turn/diff/updated", "params": {"diff": "d"}}), 400)),
            None
        );
        assert_eq!(string_at("{\"a\":1}", &["path"]), None);
        // 值里的同名字符串不算键
        assert_eq!(string_at("{\"a\":\"path\",\"path\":\"/real\"}", &["path"]).as_deref(), Some("/real"));
        // 路径里有转义
        assert_eq!(string_at("{\"path\":\"/a/\\\"b\\\"/c\"}", &["path"]).as_deref(), Some("/a/\"b\"/c"));
    }

    #[test]
    fn classifies_the_three_shapes() {
        assert_eq!(
            classify(r#"{"id":3,"result":{"turn":{"id":"t"}}}"#),
            Some(Incoming::Response { id: 3, result: Ok(json!({"turn": {"id": "t"}})) })
        );
        assert_eq!(
            classify(r#"{"error":{"code":-32600,"message":"no active turn to steer"},"id":4}"#),
            Some(Incoming::Response { id: 4, result: Err(RpcError::new(-32600, "no active turn to steer")) })
        );
        assert_eq!(
            classify(r#"{"method":"item/commandExecution/requestApproval","id":0,"params":{"itemId":"c"}}"#),
            Some(Incoming::Request {
                id: json!(0),
                method: "item/commandExecution/requestApproval".into(),
                params: json!({"itemId": "c"})
            })
        );
        assert_eq!(
            classify(r#"{"method":"turn/started","params":{"threadId":"t"}}"#),
            Some(Incoming::Notification { method: "turn/started".into(), params: json!({"threadId": "t"}) })
        );
    }

    #[test]
    fn string_request_ids_are_kept_and_junk_is_ignored() {
        match classify(r#"{"method":"x","id":"abc","params":{}}"#) {
            Some(Incoming::Request { id, .. }) => assert_eq!(id, json!("abc")),
            other => panic!("{other:?}"),
        }
        assert_eq!(classify("not json"), None);
        assert_eq!(classify(""), None);
        assert_eq!(classify("[1,2]"), None);
        assert_eq!(classify(r#"{"hello":"world"}"#), None);
        // 响应的 id 不是整数 = 不是我们发的
        assert_eq!(classify(r#"{"id":"x","result":{}}"#), None);
        // 通知没带 params 也行
        assert_eq!(
            classify(r#"{"method":"initialized"}"#),
            Some(Incoming::Notification { method: "initialized".into(), params: Value::Null })
        );
    }
}
