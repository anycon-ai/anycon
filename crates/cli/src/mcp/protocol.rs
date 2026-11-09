//! MCP（Model Context Protocol）服务端的协议层：stdio 上换行分隔的 JSON-RPC 2.0，一行一条消息。
//!
//! 只实现 tools：`initialize`、`notifications/initialized`、`notifications/cancelled`、`ping`、`tools/list`、`tools/call`。
//! 没有 resources / prompts / sampling，也不引入 MCP SDK（ADR-49）。
//!
//! - stdout 只写协议消息；日志走 stderr。输入 EOF 即收尾退出；
//! - 通知不回复；未知方法回 `-32601`；工具执行失败用结果里的 `isError: true`，协议错误才用 JSON-RPC error；
//! - `tools/call` 各自在独立任务里跑（一次等待可以长达几分钟，不能挡住 `ping` 和别的调用），响应按完成顺序写出；
//!   客户端发 `notifications/cancelled` 时中止对应的调用且不回复；
//! - 不支持 JSON-RPC 批量（2025-06-18 起规范里已经去掉），收到数组回 `-32600`。
//!
//! 协议层不认识任何具体的工具：工具由 [`ToolHost`] 提供，所以这里的测试不需要 daemon。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

/// 支持的协议版本，新的在前。客户端请求的版本在这里就回它，否则回第一项。
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// 单行（一条消息）的上限。超长的行整行丢弃并回 `-32700`，连接继续用。
const MAX_LINE: usize = 8 * 1024 * 1024;
/// 输入 EOF 之后等还在跑的调用多久，超过就中止。
const EOF_GRACE: Duration = Duration::from_secs(10);

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// 一个工具的声明（`tools/list` 的一项）。
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: String,
    /// JSON Schema（draft 2020-12 的子集）。
    pub input_schema: Value,
    /// 不改任何状态。
    pub read_only: bool,
}

impl ToolDef {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": false,
                "openWorldHint": false,
            },
        })
    }
}

/// 一次工具调用的结果：若干段文字，加上是不是执行失败。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub blocks: Vec<String>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput { blocks: vec![text.into()], is_error: false }
    }

    /// 紧凑 JSON 一段。
    pub fn json(v: &Value) -> Self {
        Self::text(v.to_string())
    }

    /// 工具执行失败（调用方能读到原因、可以自己纠正）。
    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput { blocks: vec![text.into()], is_error: true }
    }

    fn to_json(&self) -> Value {
        json!({
            "content": self.blocks.iter().map(|t| json!({ "type": "text", "text": t })).collect::<Vec<_>>(),
            "isError": self.is_error,
        })
    }
}

/// 协议层要的工具提供方。
pub trait ToolHost: Send + Sync + 'static {
    fn tools(&self) -> Vec<ToolDef>;

    /// 给客户端的使用说明（`initialize` 结果里的 `instructions`）。
    fn instructions(&self) -> Option<String> {
        None
    }

    /// 执行一个工具。工具不存在返回 `None`（协议层回 `-32602`）；参数不对、执行失败都用 [`ToolOutput::error`]。
    fn call(&self, name: &str, arguments: Value) -> impl Future<Output = Option<ToolOutput>> + Send;
}

pub fn server_name() -> &'static str {
    "anycon"
}

fn response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

/// `initialize` 的版本协商。
pub fn negotiate_version(requested: &str) -> &'static str {
    SUPPORTED_VERSIONS.iter().copied().find(|v| *v == requested).unwrap_or(SUPPORTED_VERSIONS[0])
}

fn initialize_result<H: ToolHost>(host: &H, params: &Value) -> Result<Value, (i64, String)> {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "initialize needs a string protocolVersion".to_string()))?;
    let mut result = json!({
        "protocolVersion": negotiate_version(requested),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": server_name(),
            "title": "anycon",
            "version": env!("CARGO_PKG_VERSION"),
        },
    });
    if let Some(i) = host.instructions() {
        result["instructions"] = Value::String(i);
    }
    Ok(result)
}

/// 请求 id 在「进行中的调用」表里的键。
fn id_key(id: &Value) -> String {
    id.to_string()
}

/// 退出时中止任务（调用被取消、或者协议层收尾）。
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type InFlight = Arc<Mutex<HashMap<String, AbortHandle>>>;

/// 读到的一行。
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Eof,
    Text(Vec<u8>),
    /// 超过 [`MAX_LINE`]：已经读到换行为止并丢弃。
    TooLong,
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, max: usize) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    // 最多读 max + 1 字节：超长的行不用读完整行就能发现
    let n = (&mut *reader).take(max as u64 + 1).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.len() > max && buf.last() != Some(&b'\n') {
        // 把这一行剩下的部分读掉丢弃
        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    reader.consume(i + 1);
                    break;
                }
                None => {
                    let n = chunk.len();
                    reader.consume(n);
                }
            }
        }
        return Ok(Line::TooLong);
    }
    Ok(Line::Text(buf))
}

/// 在 `reader` / `writer` 上服务，直到输入 EOF 且进行中的调用收尾。
pub async fn serve<H, R, W>(host: Arc<H>, mut reader: R, writer: W) -> std::io::Result<()>
where
    H: ToolHost,
    R: AsyncRead + AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut w = writer;
        while let Some(line) = out_rx.recv().await {
            let ok = w.write_all(line.as_bytes()).await.is_ok() && w.write_all(b"\n").await.is_ok();
            if !ok || w.flush().await.is_err() {
                return;
            }
        }
    });
    let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
    let mut calls: JoinSet<()> = JoinSet::new();

    loop {
        // 顺手收掉已经结束的调用
        while calls.try_join_next().is_some() {}
        let line = match read_line(&mut reader, MAX_LINE).await? {
            Line::Eof => break,
            Line::TooLong => {
                let _ = out.send(error_response(&Value::Null, PARSE_ERROR, "message too long"));
                continue;
            }
            Line::Text(l) => l,
        };
        let text = line.trim_ascii();
        if text.is_empty() {
            continue;
        }
        handle_line(&host, text, &out, &in_flight, &mut calls);
    }

    // 输入关了：给进行中的调用一点时间写出响应，然后收尾
    let drain = async { while calls.join_next().await.is_some() {} };
    if tokio::time::timeout(EOF_GRACE, drain).await.is_err() {
        calls.abort_all();
    }
    drop(out);
    let _ = writer_task.await;
    Ok(())
}

fn handle_line<H: ToolHost>(
    host: &Arc<H>,
    text: &[u8],
    out: &mpsc::UnboundedSender<String>,
    in_flight: &InFlight,
    calls: &mut JoinSet<()>,
) {
    let msg: Value = match serde_json::from_slice(text) {
        Ok(v) => v,
        Err(_) => {
            let _ = out.send(error_response(&Value::Null, PARSE_ERROR, "parse error"));
            return;
        }
    };
    let Some(obj) = msg.as_object() else {
        let why = if msg.is_array() { "JSON-RPC batches are not supported" } else { "a message must be a JSON object" };
        let _ = out.send(error_response(&Value::Null, INVALID_REQUEST, why));
        return;
    };
    let id = obj.get("id").cloned();
    let method = obj.get("method").and_then(Value::as_str);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        // 回复谁：有合法 id 就回给它，否则 null
        let id = id.filter(|i| i.is_string() || i.is_number()).unwrap_or(Value::Null);
        let _ = out.send(error_response(&id, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
        return;
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        // 客户端的响应：我们不发请求，忽略
        (None, _) => {}
        (Some(method), None) => on_notification(method, &params, in_flight),
        (Some(method), Some(id)) => {
            if !(id.is_string() || id.is_number()) {
                let _ = out.send(error_response(
                    &Value::Null,
                    INVALID_REQUEST,
                    "a request id must be a string or a number",
                ));
                return;
            }
            on_request(host, method, params, id, out, in_flight, calls);
        }
    }
}

fn on_notification(method: &str, params: &Value, in_flight: &InFlight) {
    if method == "notifications/cancelled" {
        if let Some(id) = params.get("requestId") {
            if let Some(h) = in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&id_key(id)) {
                h.abort();
            }
        }
    }
    // `notifications/initialized` 和别的通知：不需要做什么，也不回复
}

fn on_request<H: ToolHost>(
    host: &Arc<H>,
    method: &str,
    params: Value,
    id: Value,
    out: &mpsc::UnboundedSender<String>,
    in_flight: &InFlight,
    calls: &mut JoinSet<()>,
) {
    let reply = |v: Result<Value, (i64, String)>| match v {
        Ok(result) => response(&id, result),
        Err((code, message)) => error_response(&id, code, &message),
    };
    match method {
        "initialize" => {
            let _ = out.send(reply(initialize_result(host.as_ref(), &params)));
        }
        "ping" => {
            let _ = out.send(reply(Ok(json!({}))));
        }
        "tools/list" => {
            let tools: Vec<Value> = host.tools().iter().map(ToolDef::to_json).collect();
            let _ = out.send(reply(Ok(json!({ "tools": tools }))));
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str).map(str::to_string) else {
                let _ = out.send(reply(Err((INVALID_PARAMS, "tools/call needs a tool name".into()))));
                return;
            };
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(a) if a.is_object() => a.clone(),
                Some(_) => {
                    let _ = out.send(reply(Err((INVALID_PARAMS, "arguments must be an object".into()))));
                    return;
                }
            };
            let key = id_key(&id);
            let (host, out, table) = (host.clone(), out.clone(), in_flight.clone());
            let key2 = key.clone();
            let handle = calls.spawn(async move {
                // 工具跑在内层任务里：它 panic 了，调用方也能收到一个错误响应，而不是永远等下去
                let mut inner = AbortOnDrop(tokio::spawn({
                    let name = name.clone();
                    async move { host.call(&name, arguments).await }
                }));
                let joined = (&mut inner.0).await;
                table.lock().unwrap_or_else(|e| e.into_inner()).remove(&key2);
                let line = match joined {
                    Ok(Some(o)) => response(&id, o.to_json()),
                    Ok(None) => error_response(&id, INVALID_PARAMS, &format!("unknown tool: {name}")),
                    Err(_) => error_response(&id, INTERNAL_ERROR, "the tool failed unexpectedly"),
                };
                let _ = out.send(line);
            });
            in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(key, handle);
        }
        other => {
            let _ = out.send(error_response(&id, METHOD_NOT_FOUND, &format!("method not found: {other}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个工具：`echo`（回显参数）、`slow`（睡很久）、`boom`（panic）。
    struct Fake;

    impl ToolHost for Fake {
        fn tools(&self) -> Vec<ToolDef> {
            vec![
                ToolDef {
                    name: "echo",
                    title: "Echo",
                    description: "Echo the arguments.".into(),
                    input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": true }),
                    read_only: true,
                },
                ToolDef {
                    name: "slow",
                    title: "Slow",
                    description: "Sleeps.".into(),
                    input_schema: json!({ "type": "object", "properties": {} }),
                    read_only: true,
                },
            ]
        }

        fn instructions(&self) -> Option<String> {
            Some("be nice".into())
        }

        async fn call(&self, name: &str, arguments: Value) -> Option<ToolOutput> {
            match name {
                "echo" => Some(ToolOutput::json(&arguments)),
                "fail" => Some(ToolOutput::error("it did not work")),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Some(ToolOutput::text("late"))
                }
                "boom" => panic!("boom"),
                _ => None,
            }
        }
    }

    /// 把 `input` 当作客户端写的全部内容（之后 EOF），返回服务端写出的每一行（已解析）。
    async fn exchange(input: &str) -> Vec<Value> {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server);
        let task = tokio::spawn(serve(Arc::new(Fake), tokio::io::BufReader::new(sr), sw));
        let (mut cr, mut cw) = tokio::io::split(client);
        cw.write_all(input.as_bytes()).await.unwrap();
        cw.shutdown().await.unwrap();
        let mut out = String::new();
        cr.read_to_string(&mut out).await.unwrap();
        task.await.unwrap().unwrap();
        out.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}"))).collect()
    }

    fn req(id: u64, method: &str, params: Value) -> String {
        format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
    }

    #[tokio::test]
    async fn initialize_negotiates_the_version_and_declares_tools() {
        for v in SUPPORTED_VERSIONS {
            let r = exchange(&req(1, "initialize", json!({ "protocolVersion": v, "capabilities": {} }))).await;
            assert_eq!(r.len(), 1);
            assert_eq!(r[0]["id"], 1);
            assert_eq!(r[0]["result"]["protocolVersion"], *v, "a supported version is echoed back");
            assert!(r[0]["result"]["capabilities"]["tools"].is_object());
            assert!(r[0]["result"]["capabilities"].get("resources").is_none());
            assert!(r[0]["result"]["capabilities"].get("prompts").is_none());
            assert_eq!(r[0]["result"]["serverInfo"]["name"], "anycon");
            assert_eq!(r[0]["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
            assert_eq!(r[0]["result"]["instructions"], "be nice");
        }
        // 不认识的版本（更新的、更旧的、乱写的）：回我们支持的最新版
        for v in ["2099-01-01", "2023-01-01", "banana", ""] {
            let r = exchange(&req(1, "initialize", json!({ "protocolVersion": v }))).await;
            assert_eq!(r[0]["result"]["protocolVersion"], SUPPORTED_VERSIONS[0], "{v:?}");
        }
        // 没有 protocolVersion 是参数错误
        for p in [json!({}), json!(null), json!({ "protocolVersion": 5 })] {
            let r = exchange(&req(1, "initialize", p)).await;
            assert_eq!(r[0]["error"]["code"], INVALID_PARAMS);
        }
        assert_eq!(negotiate_version("2025-03-26"), "2025-03-26");
        assert!(SUPPORTED_VERSIONS.contains(&"2024-11-05") && SUPPORTED_VERSIONS.contains(&"2025-03-26"));
    }

    #[tokio::test]
    async fn notifications_get_no_reply_and_ping_works_before_initialize() {
        let input = format!(
            "{}{}{}{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string() + "\n",
            json!({ "jsonrpc": "2.0", "method": "notifications/whatever", "params": { "x": 1 } }).to_string() + "\n",
            // 未知通知也不回复（即使是「看起来像请求」的方法名）
            json!({ "jsonrpc": "2.0", "method": "tools/call", "params": { "name": "echo" } }).to_string() + "\n",
            req(7, "ping", json!(null)),
        );
        let r = exchange(&input).await;
        assert_eq!(r.len(), 1, "only the ping is answered: {r:?}");
        assert_eq!((r[0]["id"].clone(), r[0]["result"].clone()), (json!(7), json!({})));
    }

    #[tokio::test]
    async fn unknown_methods_are_32601_and_string_ids_are_echoed() {
        let input = format!(
            "{}{}",
            json!({ "jsonrpc": "2.0", "id": "abc", "method": "resources/list" }).to_string() + "\n",
            json!({ "jsonrpc": "2.0", "id": 2, "method": "prompts/list" }).to_string() + "\n",
        );
        let r = exchange(&input).await;
        assert_eq!(r.len(), 2);
        assert_eq!((r[0]["id"].clone(), r[0]["error"]["code"].clone()), (json!("abc"), json!(METHOD_NOT_FOUND)));
        assert_eq!(r[1]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_list_and_call() {
        let r = exchange(&format!(
            "{}{}{}{}",
            req(1, "tools/list", json!({})),
            req(2, "tools/call", json!({ "name": "echo", "arguments": { "a": 1 } })),
            req(3, "tools/call", json!({ "name": "fail" })),
            req(4, "tools/call", json!({ "name": "nope", "arguments": {} })),
        ))
        .await;
        let by_id = |id: u64| r.iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("no reply to {id}: {r:?}"));
        let tools = by_id(1)["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert!(tools[0]["inputSchema"].is_object());
        let echo = &by_id(2)["result"];
        assert_eq!(echo["isError"], false);
        assert_eq!(echo["content"][0]["type"], "text");
        assert_eq!(echo["content"][0]["text"], "{\"a\":1}");
        // 工具失败 = 结果里 isError，不是协议错误
        let fail = &by_id(3);
        assert!(fail.get("error").is_none());
        assert_eq!(fail["result"]["isError"], true);
        assert_eq!(fail["result"]["content"][0]["text"], "it did not work");
        // 没有这个工具 = 协议错误
        assert_eq!(by_id(4)["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn malformed_input_does_not_kill_the_server() {
        let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{}\"}}\n", "x".repeat(100));
        let input = format!(
            "not json at all\n\n   \n[1,2]\n42\n\"str\"\nnull\n{{\"jsonrpc\":\"1.0\",\"id\":3,\"method\":\"ping\"}}\n{{\"id\":4,\"method\":\"ping\"}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":{{}},\"method\":\"ping\"}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":7}}\n{big}\
             {}{}{}{{\"truncated\":",
            req(10, "tools/call", json!({})),
            req(11, "tools/call", json!({ "name": "echo", "arguments": [1] })),
            req(12, "ping", json!({})),
        );
        let r = exchange(&input).await;
        let codes: Vec<i64> = r.iter().filter_map(|m| m["error"]["code"].as_i64()).collect();
        assert!(
            codes.iter().all(|c| [PARSE_ERROR, INVALID_REQUEST, INVALID_PARAMS, METHOD_NOT_FOUND].contains(c)),
            "{r:?}"
        );
        // 最后的好请求照样被答复
        assert!(r.iter().any(|m| m["id"] == 12 && m["result"] == json!({})), "{r:?}");
        // tools/call 缺名字、arguments 不是对象 = -32602
        assert!(r.iter().any(|m| m["id"] == 10 && m["error"]["code"] == INVALID_PARAMS), "{r:?}");
        assert!(r.iter().any(|m| m["id"] == 11 && m["error"]["code"] == INVALID_PARAMS), "{r:?}");
        // `"method": 7` 当作没有方法的消息（响应），不回复；jsonrpc 版本不对 / null id 都有错误回复
        assert!(r.iter().any(|m| m["id"] == 3 && m["error"]["code"] == INVALID_REQUEST), "{r:?}");
        assert!(r.iter().any(|m| m["id"].is_null() && m["error"]["code"] == INVALID_REQUEST), "{r:?}");
        assert!(r.iter().any(|m| m["error"]["code"] == PARSE_ERROR), "not json / truncated JSON: {r:?}");
    }

    #[tokio::test]
    async fn an_over_long_line_is_dropped_and_the_next_one_still_works() {
        // 直接测 read_line 的边界：上限 16 字节
        let mut input: &[u8] = b"0123456789abcdef0123456789\nshort\nexact-16-bytes!!\n";
        assert_eq!(read_line(&mut input, 16).await.unwrap(), Line::TooLong);
        assert_eq!(read_line(&mut input, 16).await.unwrap(), Line::Text(b"short\n".to_vec()));
        assert_eq!(read_line(&mut input, 16).await.unwrap(), Line::Text(b"exact-16-bytes!!\n".to_vec()));
        assert_eq!(read_line(&mut input, 16).await.unwrap(), Line::Eof);
        // 没有换行就 EOF 的最后一行照常返回
        let mut input: &[u8] = b"tail";
        assert_eq!(read_line(&mut input, 16).await.unwrap(), Line::Text(b"tail".to_vec()));
    }

    #[tokio::test]
    async fn a_panicking_tool_still_gets_an_error_response() {
        let r = exchange(&format!("{}{}", req(1, "tools/call", json!({ "name": "boom" })), req(2, "ping", json!({}))))
            .await;
        assert!(r.iter().any(|m| m["id"] == 1 && m["error"]["code"] == INTERNAL_ERROR), "{r:?}");
        assert!(r.iter().any(|m| m["id"] == 2 && m["result"] == json!({})), "the server keeps going: {r:?}");
    }

    #[tokio::test]
    async fn a_slow_call_does_not_block_other_requests_and_can_be_cancelled() {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server);
        let task = tokio::spawn(serve(Arc::new(Fake), tokio::io::BufReader::new(sr), sw));
        let (cr, mut cw) = tokio::io::split(client);
        let mut lines = tokio::io::BufReader::new(cr).lines();
        cw.write_all(req(1, "tools/call", json!({ "name": "slow" })).as_bytes()).await.unwrap();
        // 慢调用还在跑，ping 照常答复
        cw.write_all(req(2, "ping", json!({})).as_bytes()).await.unwrap();
        let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], 2);
        // 取消慢调用：不回复；随后输入关闭，服务端很快收尾（不用等 EOF_GRACE）
        let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } });
        cw.write_all(format!("{cancel}\n").as_bytes()).await.unwrap();
        cw.shutdown().await.unwrap();
        let rest =
            tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await.expect("server stops").unwrap();
        assert!(rest.is_none(), "a cancelled call is not answered: {rest:?}");
        tokio::time::timeout(Duration::from_secs(5), task).await.expect("serve returns").unwrap().unwrap();
    }
}
