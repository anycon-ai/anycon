//! `kind = "api"` 的判断后端（docs/spec/daemon.md § 25.1）：一次性调用 Anthropic Messages 或 OpenAI Chat Completions。
//! 请求只有系统提示、用户消息、模型、输出上限和（Anthropic 可选的）`output_config.effort`，不带工具或 temperature；
//! User-Agent 只有 `anycon/<版本>`。OpenAI 官方主机用 `max_completion_tokens`，兼容网关用 `max_tokens`。
//! 密钥每次从 daemon 环境或配置读取；缺少密钥的非回环地址返回 Auth，非回环的明文 HTTP 返回 Internal，均不连接。
//! HTTPS 校验 webpki 根证书。
//!
//! 回应用流式（SSE）：长的调用在生成期间连接上一直有数据，不会因为长时间空闲被中间的网络设备断开。
//! 流边到边解析，只留下要的文字和用量：Anthropic 取第一个文字块，OpenAI 取第一项 choice 的文字。
//! 没有读到结束标记就断开的流算网络错误。整次调用受配置的 timeout 约束，不重试、不跟随重定向；
//! 取出的文字和单个事件各最多 1 MiB，整个流最多 32 MiB。
//! 网络与断连、认证、限流、HTTP 状态、超时、超限或无效回应分别映射到固定错误；提示、回应和密钥不进日志、错误或 Debug。

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use serde_json::{json, Value};

use super::{ApiBackendConfig, ApiFormat, Backend, CallError, JudgeReply, JudgeRequest, Usage};
use crate::env::DaemonEnv;
use crate::https;

/// 从流里取出的文字最多这么多字节。
const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// 一个 SSE 事件（含还没结束的一行）最多这么多字节。
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// 整个流最多这么多字节。OpenAI 格式每个 token 一个事件，事件本身的开销比文字大得多。
const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;

pub struct ApiBackend {
    cfg: ApiBackendConfig,
    env: Arc<DaemonEnv>,
}

impl ApiBackend {
    /// `env` 是 daemon 给 agent 用的那份环境（§ 5.5），`api_key_env` 每次调用时从它里面读。
    pub fn new(cfg: ApiBackendConfig, env: Arc<DaemonEnv>) -> Self {
        Self { cfg, env }
    }
}

#[async_trait::async_trait]
impl Backend for ApiBackend {
    async fn call(&self, req: &JudgeRequest) -> Result<JudgeReply, CallError> {
        let work = async {
            let path = match self.cfg.format {
                ApiFormat::Anthropic => "/v1/messages",
                ApiFormat::Openai => "/chat/completions",
            };
            // 即使配置已校验，也在连接之前拒绝非回环明文地址。
            let target = https::parse_target(&format!("{}{path}", self.cfg.base_url)).map_err(map_error)?;
            let key = super::resolve_api_key(&self.cfg.api_key, &self.env.base);
            if key.is_none() && !https::is_loopback(&target.host) {
                return Err(CallError::Auth);
            }
            let request = make_request(&self.cfg, req, &target, key.as_deref())?;
            let mut stream = SseReply::new(self.cfg.format);
            let result = https::request_streaming(
                &target,
                request,
                self.cfg.timeout,
                |status| status.is_success(),
                &mut |chunk: &[u8]| stream.feed(chunk).map_err(|_| https::Error::Body),
            )
            .await;
            match result {
                // 解析时自己中止的，报解析记下的原因。
                Err(error) => Err(stream.failure.unwrap_or_else(|| map_error(error))),
                Ok(()) => stream.finish(),
            }
        };
        tokio::time::timeout(self.cfg.timeout, work).await.map_err(|_| CallError::Timeout)?
    }
}

fn make_request(
    cfg: &ApiBackendConfig,
    req: &JudgeRequest,
    target: &https::Target,
    key: Option<&str>,
) -> Result<http::Request<Full<Bytes>>, CallError> {
    let mut builder = http::Request::post(target.path.as_str())
        .header(http::header::HOST, target.authority.as_str())
        .header(http::header::USER_AGENT, concat!("anycon/", env!("CARGO_PKG_VERSION")))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "text/event-stream");
    let body = match cfg.format {
        ApiFormat::Anthropic => {
            builder = builder.header("anthropic-version", "2023-06-01");
            if let Some(key) = key {
                builder = builder.header("x-api-key", auth_header(key)?);
            }
            let mut body = json!({
                "model": cfg.model,
                "max_tokens": cfg.max_output_tokens,
                "system": req.system,
                "messages": [{"role": "user", "content": req.prompt}],
                "stream": true,
            });
            if let Some(effort) = cfg.effort {
                body["output_config"] = json!({"effort": effort.as_str()});
            }
            body
        }
        ApiFormat::Openai => {
            if let Some(key) = key {
                builder = builder.header(http::header::AUTHORIZATION, auth_header(&format!("Bearer {key}"))?);
            }
            let mut body = json!({
                "model": cfg.model,
                "messages": [
                    {"role": "system", "content": req.system},
                    {"role": "user", "content": req.prompt},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            let limit_field =
                if target.host.eq_ignore_ascii_case("api.openai.com") { "max_completion_tokens" } else { "max_tokens" };
            body[limit_field] = cfg.max_output_tokens.into();
            body
        }
    };
    let body = serde_json::to_vec(&body).map_err(|_| CallError::Internal)?;
    builder.body(Full::new(Bytes::from(body))).map_err(|_| CallError::Internal)
}

fn auth_header(value: &str) -> Result<http::HeaderValue, CallError> {
    let mut header = http::HeaderValue::from_str(value).map_err(|_| CallError::Internal)?;
    header.set_sensitive(true);
    Ok(header)
}

/// 边收边解析的 SSE 回应。只保留取出的文字和用量，流本身不留。
struct SseReply {
    format: ApiFormat,
    /// 还没遇到换行的那一行。
    line: Vec<u8>,
    /// 当前事件已经收到的 `data:` 行，多行之间用 `\n` 连接。
    data: Vec<u8>,
    total: usize,
    text: String,
    /// Anthropic：第一个文字块的 index。
    text_index: Option<u64>,
    usage: Usage,
    /// 读到了结束标记：Anthropic 的 `message_stop`，OpenAI 的 `[DONE]`。
    done: bool,
    /// OpenAI：第一项 choice 给出了 `finish_reason`。有的兼容服务不发 `[DONE]`，这时以它为准。
    finished: bool,
    /// 解析中止的原因，`feed` 返回错误时记下。
    failure: Option<CallError>,
}

impl SseReply {
    fn new(format: ApiFormat) -> Self {
        Self {
            format,
            line: Vec::new(),
            data: Vec::new(),
            total: 0,
            text: String::new(),
            text_index: None,
            usage: Usage::default(),
            done: false,
            finished: false,
            failure: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<(), CallError> {
        let result = self.feed_inner(chunk);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn feed_inner(&mut self, chunk: &[u8]) -> Result<(), CallError> {
        self.total += chunk.len();
        if self.total > MAX_STREAM_BYTES {
            return Err(CallError::BadOutput);
        }
        let mut rest = chunk;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..i]);
            rest = &rest[i + 1..];
            let line = std::mem::take(&mut self.line);
            self.on_line(&line)?;
        }
        self.line.extend_from_slice(rest);
        if self.line.len() > MAX_EVENT_BYTES {
            return Err(CallError::BadOutput);
        }
        Ok(())
    }

    /// 一行（不含换行符）。空行结束一个事件；只看 `data:`，`event:`、`id:`、`retry:` 和注释行忽略。
    fn on_line(&mut self, line: &[u8]) -> Result<(), CallError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.dispatch();
        }
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            if self.data.len() > MAX_EVENT_BYTES {
                return Err(CallError::BadOutput);
            }
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), CallError> {
        let data = std::mem::take(&mut self.data);
        if data.is_empty() || self.done {
            return Ok(());
        }
        if self.format == ApiFormat::Openai && data == b"[DONE]" {
            self.done = true;
            return Ok(());
        }
        let event: Value = serde_json::from_slice(&data).map_err(|_| CallError::BadOutput)?;
        match self.format {
            ApiFormat::Anthropic => self.on_anthropic(&event),
            ApiFormat::Openai => self.on_openai(&event),
        }
    }

    fn on_anthropic(&mut self, event: &Value) -> Result<(), CallError> {
        match event["type"].as_str() {
            Some("message_start") => self.take_usage(&event["message"]["usage"], "input_tokens", "output_tokens"),
            Some("content_block_start") => {
                if self.text_index.is_none() && event["content_block"]["type"] == "text" {
                    self.text_index = Some(event["index"].as_u64().ok_or(CallError::BadOutput)?);
                    let text = event["content_block"]["text"].as_str().ok_or(CallError::BadOutput)?;
                    self.push_text(text)?;
                }
            }
            Some("content_block_delta") => {
                if self.text_index.is_some()
                    && event["index"].as_u64() == self.text_index
                    && event["delta"]["type"] == "text_delta"
                {
                    let text = event["delta"]["text"].as_str().ok_or(CallError::BadOutput)?;
                    self.push_text(text)?;
                }
            }
            // `message_delta` 里的用量是累计值，后来的覆盖前面的。
            Some("message_delta") => self.take_usage(&event["usage"], "input_tokens", "output_tokens"),
            Some("message_stop") => self.done = true,
            // 已经回了 200 之后服务端出的错（过载等）。错误里的文字不带出来。
            Some("error") => {
                return Err(match event["error"]["type"].as_str() {
                    Some("rate_limit_error") => CallError::RateLimited,
                    Some("authentication_error" | "permission_error") => CallError::Auth,
                    Some("overloaded_error") => CallError::Http(529),
                    _ => CallError::Http(500),
                })
            }
            _ => {}
        }
        Ok(())
    }

    fn on_openai(&mut self, event: &Value) -> Result<(), CallError> {
        if event.get("error").is_some_and(|e| !e.is_null()) {
            return Err(CallError::Http(500));
        }
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices.iter().filter(|c| c["index"].as_u64().unwrap_or(0) == 0) {
                match &choice["delta"]["content"] {
                    Value::Null => {}
                    Value::String(text) => self.push_text(text)?,
                    Value::Array(parts) => {
                        for part in parts.iter().filter(|part| part["type"] == "text") {
                            if let Some(text) = part["text"].as_str() {
                                self.push_text(text)?;
                            }
                        }
                    }
                    _ => return Err(CallError::BadOutput),
                }
                if !choice["finish_reason"].is_null() {
                    self.finished = true;
                }
            }
        }
        self.take_usage(&event["usage"], "prompt_tokens", "completion_tokens");
        Ok(())
    }

    fn push_text(&mut self, text: &str) -> Result<(), CallError> {
        if self.text.len() + text.len() > MAX_TEXT_BYTES {
            return Err(CallError::BadOutput);
        }
        self.text.push_str(text);
        Ok(())
    }

    fn take_usage(&mut self, usage: &Value, input_field: &str, output_field: &str) {
        if let Some(n) = usage[input_field].as_u64() {
            self.usage.input_tokens = Some(n);
        }
        if let Some(n) = usage[output_field].as_u64() {
            self.usage.output_tokens = Some(n);
        }
    }

    /// 流正常结束：处理没有以空行收尾的最后一个事件，再检查读到了结束标记、取到了文字。
    fn finish(mut self) -> Result<JudgeReply, CallError> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.on_line(&line)?;
        }
        self.dispatch()?;
        if !(self.done || (self.finished && self.format == ApiFormat::Openai)) {
            return Err(CallError::Network);
        }
        if self.text.is_empty() {
            return Err(CallError::BadOutput);
        }
        Ok(JudgeReply { text: self.text, usage: self.usage })
    }
}

fn map_error(error: https::Error) -> CallError {
    match error {
        https::Error::BadUrl => CallError::Internal,
        https::Error::Timeout => CallError::Timeout,
        https::Error::Connect(_) | https::Error::Tls | https::Error::Http | https::Error::Body => CallError::Network,
        https::Error::Status(401 | 403) => CallError::Auth,
        https::Error::Status(429) => CallError::RateLimited,
        https::Error::Status(status) => CallError::Http(status),
        https::Error::TooLarge => CallError::BadOutput,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::env::EnvSource;
    use crate::judge::ApiKeySource;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const PRIVATE_BODY: &str = "private response words";

    struct Captured {
        line: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    /// 一次性本机 HTTP 服务：读完整请求后发指定字节，也可以等到客户端关闭连接。
    struct Server {
        url: String,
        task: tokio::task::JoinHandle<Captured>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        async fn start(response: Vec<u8>, wait_for_close: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                tokio::time::timeout(TEST_TIMEOUT, async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    let header_end = loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(n, 0);
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
                    let mut lines = head.split("\r\n").filter(|line| !line.is_empty());
                    let line = lines.next().unwrap().to_owned();
                    let headers: HashMap<String, String> = lines
                        .map(|line| {
                            let (name, value) = line.split_once(':').unwrap();
                            (name.to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect();
                    let len: usize = headers["content-length"].parse().unwrap();
                    while bytes.len() < header_end + len {
                        let n = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(n, 0);
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    let body = serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
                    let _ = socket.write_all(&response).await;
                    if wait_for_close {
                        assert_eq!(socket.read(&mut chunk).await.unwrap(), 0);
                    }
                    Captured { line, headers, body }
                })
                .await
                .expect("local HTTP server timed out")
            });
            Self { url, task }
        }

        async fn reply(status: u16, body: &[u8]) -> Self {
            Self::start(response(status, "", body), false).await
        }

        async fn captured(&mut self) -> Captured {
            tokio::time::timeout(TEST_TIMEOUT, &mut self.task).await.unwrap().unwrap()
        }
    }

    fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes =
            format!("HTTP/1.1 {status} Test\r\ncontent-length: {}\r\n{headers}\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn backend(format: ApiFormat, base_url: &str, api_key: ApiKeySource) -> ApiBackend {
        ApiBackend::new(
            ApiBackendConfig {
                format,
                base_url: base_url.into(),
                model: "test-model".into(),
                api_key,
                max_output_tokens: 321,
                effort: None,
                timeout: TEST_TIMEOUT,
            },
            Arc::new(DaemonEnv {
                base: Arc::new(HashMap::new()),
                source: EnvSource::Process,
                shell: None,
                warning: None,
            }),
        )
    }

    fn input() -> JudgeRequest {
        JudgeRequest { system: "系统提示\nwith quotes: \"".into(), prompt: "用户的决定".into() }
    }

    /// 把每个 JSON（或 `[DONE]`）写成一个 `data:` 事件。
    fn sse(events: &[&str]) -> Vec<u8> {
        events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect::<String>().into_bytes()
    }

    /// 一个只有一段文字「ok」的、完整的流。
    fn ok_stream(format: ApiFormat) -> Vec<u8> {
        match format {
            ApiFormat::Anthropic => sse(&[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"{"type":"message_stop"}"#,
            ]),
            ApiFormat::Openai => sse(&[r#"{"choices":[{"index":0,"delta":{"content":"ok"}}]}"#, "[DONE]"]),
        }
    }

    fn assert_common_headers(captured: &Captured, server: &Server) {
        assert_eq!(captured.headers["content-type"], "application/json");
        assert_eq!(captured.headers["accept"], "text/event-stream");
        assert_eq!(captured.headers["user-agent"], concat!("anycon/", env!("CARGO_PKG_VERSION")));
        assert_eq!(captured.headers["host"], server.url.strip_prefix("http://").unwrap());
        assert!(!captured.headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn anthropic_request_and_first_text_block_with_usage() {
        let stream = sse(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":17,"output_tokens":1}}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ignore"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"fi"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"r"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"st"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":"second"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":" ignore"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9},"cost_usd":1.5}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let mut server = Server::reply(200, &stream).await;
        let mut api = backend(ApiFormat::Anthropic, &server.url, ApiKeySource::Literal("test-key".into()));
        api.cfg.effort = Some(super::super::Effort::Xhigh);
        let reply = api.call(&input()).await.unwrap();
        assert_eq!(reply.text, "first");
        assert_eq!(reply.usage, Usage { input_tokens: Some(17), output_tokens: Some(9), cost_usd: None });
        let captured = server.captured().await;
        assert_eq!(captured.line, "POST /v1/messages HTTP/1.1");
        assert_common_headers(&captured, &server);
        assert_eq!(captured.headers["x-api-key"], "test-key");
        assert_eq!(captured.headers["anthropic-version"], "2023-06-01");
        assert!(!captured.headers.contains_key("authorization"));
        assert_eq!(
            captured.body,
            json!({
                "model": "test-model", "max_tokens": 321, "system": input().system,
                "messages": [{"role": "user", "content": input().prompt}],
                "stream": true, "output_config": {"effort": "xhigh"}
            })
        );
    }

    #[tokio::test]
    async fn anthropic_request_without_effort_leaves_output_config_out() {
        let mut server = Server::reply(200, &ok_stream(ApiFormat::Anthropic)).await;
        let api = backend(ApiFormat::Anthropic, &server.url, ApiKeySource::None);
        assert_eq!(api.call(&input()).await.unwrap().text, "ok");
        let body = server.captured().await.body;
        assert!(body.get("output_config").is_none());
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn openai_request_and_first_choice_with_usage() {
        let stream = sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"ans"}},{"index":1,"delta":{"content":"ignore"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"wer"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":0},"cost_usd":2}"#,
            "[DONE]",
        ]);
        let mut server = Server::reply(201, &stream).await;
        let mut api = backend(ApiFormat::Openai, &format!("{}/v1", server.url), ApiKeySource::Env("KEY".into()));
        Arc::make_mut(&mut Arc::make_mut(&mut api.env).base).insert("KEY".into(), "from-env".into());
        let reply = api.call(&input()).await.unwrap();
        assert_eq!(reply.text, "answer");
        assert_eq!(reply.usage, Usage { input_tokens: Some(20), output_tokens: Some(0), cost_usd: None });
        let captured = server.captured().await;
        assert_eq!(captured.line, "POST /v1/chat/completions HTTP/1.1");
        assert_common_headers(&captured, &server);
        assert_eq!(captured.headers["authorization"], "Bearer from-env");
        assert!(!captured.headers.contains_key("x-api-key"));
        assert!(!captured.headers.contains_key("anthropic-version"));
        assert_eq!(
            captured.body,
            json!({
                "model":"test-model", "max_tokens":321,
                "messages":[{"role":"system","content":input().system},{"role":"user","content":input().prompt}],
                "stream": true, "stream_options": {"include_usage": true}
            })
        );
    }

    #[tokio::test]
    async fn official_openai_host_uses_completion_limit_only() {
        for (host, field) in [
            ("api.openai.com", "max_completion_tokens"),
            ("API.OPENAI.COM:443", "max_completion_tokens"),
            ("gateway.example", "max_tokens"),
            ("api.openai.com.example", "max_tokens"),
        ] {
            let api = backend(ApiFormat::Openai, &format!("https://{host}/v1"), ApiKeySource::None);
            let target = https::parse_target(&format!("{}/chat/completions", api.cfg.base_url)).unwrap();
            let req = make_request(&api.cfg, &input(), &target, None).unwrap();
            let body: Value = serde_json::from_slice(&req.into_body().collect().await.unwrap().to_bytes()).unwrap();
            assert_eq!(body[field], 321);
            assert_eq!(body.as_object().unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn auth_headers_hide_secrets_and_invalid_keys_never_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        for format in [ApiFormat::Anthropic, ApiFormat::Openai] {
            let api = backend(format, &url, ApiKeySource::Literal("private-key\r\ninjected: value".into()));
            let error = api.call(&input()).await.unwrap_err();
            assert_eq!(error, CallError::Internal);
            assert!(!format!("{error} {error:?}").contains("private-key"));
            let target = https::parse_target(&url).unwrap();
            let request = make_request(&api.cfg, &input(), &target, Some("private-key")).unwrap();
            assert!(!format!("{:?}", request.headers()).contains("private-key"));
        }
        assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err());
    }

    #[tokio::test]
    async fn openai_text_parts_are_joined_in_order_and_usage_is_optional() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":[{"type":"image_url","text":"ignore"},{"type":"text","text":"one "}]}}]}"#,
            r#"{"choices":[{"delta":{"content":[{"type":"text","text":"two"}]}}]}"#,
            "[DONE]",
        ]);
        let mut server = Server::reply(200, &stream).await;
        let api = backend(ApiFormat::Openai, &server.url, ApiKeySource::None);
        let reply = api.call(&input()).await.unwrap();
        assert_eq!(reply.text, "one two");
        assert_eq!(reply.usage, Usage::default());
        server.captured().await;
    }

    #[tokio::test]
    async fn openai_stream_without_done_is_complete_after_finish_reason() {
        let finished = sse(&[r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"length"}]}"#]);
        let mut server = Server::reply(200, &finished).await;
        assert_eq!(
            backend(ApiFormat::Openai, &server.url, ApiKeySource::None).call(&input()).await.unwrap().text,
            "ok"
        );
        server.captured().await;
    }

    #[test]
    fn events_split_anywhere_with_crlf_comments_and_multiline_data_are_parsed() {
        let mut stream = b": comment\r\nevent: message_start\r\nid: 1\r\nretry: 10\r\n".to_vec();
        stream.extend_from_slice(b"data: {\"type\":\"content_block_start\",\r\ndata: \"index\":0,\"content_block\":");
        stream.extend_from_slice(b"{\"type\":\"text\",\"text\":\"\xe4\xbd\xa0\"}}\r\n\r\n");
        stream.extend_from_slice(b"data:{\"type\":\"content_block_delta\",\"index\":0,");
        stream.extend_from_slice(b"\"delta\":{\"type\":\"text_delta\",\"text\":\"\xe5\xa5\xbd\"}}\n\n");
        // 最后一个事件没有以空行收尾。
        stream.extend_from_slice(b"data: {\"type\":\"message_stop\"}");
        for size in [1, 2, 7, stream.len()] {
            let mut reply = SseReply::new(ApiFormat::Anthropic);
            for chunk in stream.chunks(size) {
                reply.feed(chunk).unwrap();
            }
            assert_eq!(reply.finish().unwrap().text, "你好", "{size}");
        }
    }

    #[test]
    fn events_after_the_end_marker_are_ignored() {
        let mut reply = SseReply::new(ApiFormat::Openai);
        let mut stream = ok_stream(ApiFormat::Openai);
        stream.extend_from_slice(b"data: not json\n\n");
        reply.feed(&stream).unwrap();
        assert_eq!(reply.finish().unwrap().text, "ok");
    }

    #[test]
    fn oversized_text_events_and_streams_fail() {
        let delta = |text: &str| {
            format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#)
        };
        let start = sse(&[r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#]);
        let half = "x".repeat(MAX_TEXT_BYTES / 2);
        let mut reply = SseReply::new(ApiFormat::Anthropic);
        reply.feed(&start).unwrap();
        reply.feed(&sse(&[&delta(&half), &delta(&half)])).unwrap();
        assert_eq!(reply.feed(&sse(&[&delta("x")])), Err(CallError::BadOutput));
        assert_eq!(reply.failure, Some(CallError::BadOutput));

        let mut reply = SseReply::new(ApiFormat::Anthropic);
        assert_eq!(reply.feed(&vec![b'x'; MAX_EVENT_BYTES + 1]), Err(CallError::BadOutput));
        let mut reply = SseReply::new(ApiFormat::Anthropic);
        let mut lines = b"data: ".to_vec();
        lines.extend(std::iter::repeat_n(b"data: x\n".as_slice(), MAX_EVENT_BYTES).flatten());
        assert_eq!(reply.feed(&lines), Err(CallError::BadOutput));

        let mut reply = SseReply::new(ApiFormat::Openai);
        let comments = b": keep-alive\n".repeat(1024);
        let mut fed = 0;
        let error = loop {
            match reply.feed(&comments) {
                Ok(()) => fed += comments.len(),
                Err(error) => break error,
            }
        };
        assert_eq!(error, CallError::BadOutput);
        assert!(fed <= MAX_STREAM_BYTES && fed + comments.len() > MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn http_errors_are_classified_without_exposing_content_or_following_redirects() {
        let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("location: http://{}/redirected\r\n", redirect.local_addr().unwrap());
        for (status, expected) in [
            (401, CallError::Auth),
            (403, CallError::Auth),
            (429, CallError::RateLimited),
            (500, CallError::Http(500)),
            (302, CallError::Http(302)),
        ] {
            let mut server = Server::start(response(status, &location, PRIVATE_BODY.as_bytes()), false).await;
            let api = backend(ApiFormat::Openai, &server.url, ApiKeySource::Literal("private-key".into()));
            let error = api.call(&input()).await.unwrap_err();
            assert_eq!(error, expected);
            for secret in [PRIVATE_BODY, "private-key", &input().system, &input().prompt] {
                assert!(!format!("{error} {error:?}").contains(secret));
            }
            server.captured().await;
        }
        assert!(tokio::time::timeout(Duration::from_millis(50), redirect.accept()).await.is_err());
    }

    #[tokio::test]
    async fn error_events_in_the_stream_are_classified_without_exposing_content() {
        let anthropic = |kind: &str| {
            let error = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"{PRIVATE_BODY}"}}}}"#);
            let mut stream = ok_stream(ApiFormat::Anthropic);
            stream.splice(0..0, sse(&[&error]));
            stream
        };
        let cases = [
            (ApiFormat::Anthropic, anthropic("overloaded_error"), CallError::Http(529)),
            (ApiFormat::Anthropic, anthropic("rate_limit_error"), CallError::RateLimited),
            (ApiFormat::Anthropic, anthropic("authentication_error"), CallError::Auth),
            (ApiFormat::Anthropic, anthropic("api_error"), CallError::Http(500)),
            (
                ApiFormat::Openai,
                sse(&[&format!(r#"{{"error":{{"message":"{PRIVATE_BODY}"}}}}"#), "[DONE]"]),
                CallError::Http(500),
            ),
        ];
        for (format, stream, expected) in cases {
            let mut server = Server::reply(200, &stream).await;
            let error = backend(format, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error} {error:?}").contains(PRIVATE_BODY));
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn timeout_covers_waiting_for_headers_and_body_and_closes_connection() {
        let mut partial = b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n".to_vec();
        partial.extend_from_slice(&ok_stream(ApiFormat::Openai)[..20]);
        for response in [Vec::new(), partial] {
            let mut server = Server::start(response, true).await;
            let mut api = backend(ApiFormat::Openai, &server.url, ApiKeySource::None);
            api.cfg.timeout = Duration::from_millis(100);
            assert_eq!(api.call(&input()).await.unwrap_err(), CallError::Timeout);
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn malformed_events_and_missing_or_empty_text_fail_without_exposing_content() {
        let anthropic_start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let stop = r#"{"type":"message_stop"}"#;
        let cases: [(ApiFormat, Vec<&str>); 9] = [
            (ApiFormat::Anthropic, vec![PRIVATE_BODY, stop]),
            (ApiFormat::Anthropic, vec![stop]),
            (ApiFormat::Anthropic, vec![anthropic_start, stop]),
            (
                ApiFormat::Anthropic,
                vec![r#"{"type":"content_block_start","index":0,"content_block":{"type":"image"}}"#, stop],
            ),
            (
                ApiFormat::Anthropic,
                vec![
                    anthropic_start,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":42}}"#,
                    stop,
                ],
            ),
            (ApiFormat::Openai, vec![PRIVATE_BODY, "[DONE]"]),
            (ApiFormat::Openai, vec!["[DONE]"]),
            (ApiFormat::Openai, vec![r#"{"choices":[{"delta":{"content":""}}]}"#, "[DONE]"]),
            (ApiFormat::Openai, vec![r#"{"choices":[{"delta":{"content":42}}]}"#, "[DONE]"]),
        ];
        for (format, events) in cases {
            let mut server = Server::reply(200, &sse(&events)).await;
            let error = backend(format, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err();
            assert_eq!(error, CallError::BadOutput, "{events:?}");
            assert!(!format!("{error} {error:?}").contains(PRIVATE_BODY));
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn missing_keys_are_allowed_on_loopback_without_auth_headers() {
        for format in [ApiFormat::Anthropic, ApiFormat::Openai] {
            for source in [ApiKeySource::None, ApiKeySource::Env("UNSET".into()), ApiKeySource::Literal(String::new())]
            {
                let mut server = Server::reply(200, &ok_stream(format)).await;
                let api = backend(format, &server.url, source);
                assert_eq!(api.call(&input()).await.unwrap().usage, Usage::default());
                let captured = server.captured().await;
                assert!(!captured.headers.contains_key("authorization"));
                assert!(!captured.headers.contains_key("x-api-key"));
            }
        }
    }

    #[tokio::test]
    async fn remote_missing_keys_and_remote_plaintext_fail_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for source in [ApiKeySource::None, ApiKeySource::Env("UNSET".into()), ApiKeySource::Literal(String::new())] {
            for host in ["example.invalid", "localhost."] {
                let api = backend(ApiFormat::Openai, &format!("https://{host}:{port}"), source.clone());
                assert_eq!(api.call(&input()).await.unwrap_err(), CallError::Auth);
            }
        }
        for host in ["example.invalid", "0.0.0.0", "localhost."] {
            for source in [ApiKeySource::None, ApiKeySource::Literal("key".into())] {
                let api = backend(ApiFormat::Openai, &format!("http://{host}:{port}"), source);
                assert_eq!(api.call(&input()).await.unwrap_err(), CallError::Internal);
            }
        }
        assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err());
    }

    #[tokio::test]
    async fn keys_are_resolved_again_on_each_call() {
        let mut api = backend(ApiFormat::Openai, "http://localhost", ApiKeySource::Env("KEY".into()));
        for key in ["first-key", "second-key"] {
            Arc::make_mut(&mut Arc::make_mut(&mut api.env).base).insert("KEY".into(), key.into());
            let mut server = Server::reply(200, &ok_stream(ApiFormat::Openai)).await;
            api.cfg.base_url = server.url.clone();
            api.call(&input()).await.unwrap();
            assert_eq!(server.captured().await.headers["authorization"], format!("Bearer {key}"));
        }
    }

    #[tokio::test]
    async fn connection_failures_and_truncated_streams_are_network_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            backend(ApiFormat::Openai, &url, ApiKeySource::None).call(&input()).await.unwrap_err(),
            CallError::Network
        );
        let mut short = b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n".to_vec();
        short.extend_from_slice(&ok_stream(ApiFormat::Openai));
        for (format, response) in [
            (ApiFormat::Openai, Vec::new()),
            (ApiFormat::Openai, short),
            // 状态和长度都对，但流在结束标记之前就完了。
            (ApiFormat::Openai, response(200, "", &sse(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#]))),
            (
                ApiFormat::Anthropic,
                response(
                    200,
                    "",
                    &sse(&[
                        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}"#,
                    ]),
                ),
            ),
        ] {
            let mut server = Server::start(response, false).await;
            assert_eq!(
                backend(format, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err(),
                CallError::Network
            );
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn tls_failures_are_network_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"not a TLS server\r\n").await.unwrap();
        });
        assert_eq!(
            backend(ApiFormat::Openai, &url, ApiKeySource::None).call(&input()).await.unwrap_err(),
            CallError::Network
        );
        tokio::time::timeout(TEST_TIMEOUT, server).await.unwrap().unwrap();
    }
}
