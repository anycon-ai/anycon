//! `kind = "api"` 的判断后端（docs/spec/daemon.md § 25.1）：一次性调用 Anthropic Messages 或 OpenAI Chat Completions。
//! 请求只有系统提示、用户消息、模型和输出上限，不用流式、工具或 temperature；User-Agent 只有 `anycon/<版本>`。
//! OpenAI 官方主机用 `max_completion_tokens`，兼容网关用 `max_tokens`。密钥每次从 daemon 环境或配置读取；
//! 缺少密钥的非回环地址返回 Auth，非回环的明文 HTTP 返回 Internal，均不连接。HTTPS 校验 webpki 根证书。
//! 整次调用受配置的 timeout 约束，不重试、不跟随重定向，回应体最多 1 MiB。
//! Anthropic 取第一个文字块，OpenAI 取第一项 choice 的文字（文字 part 数组按顺序拼接），只报告服务提供的 token 用量。
//! 网络与断连、认证、限流、HTTP 状态、超时、超限或无效回应分别映射到固定错误；提示、回应和密钥不进日志、错误或 Debug。

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use serde_json::{json, Value};

use super::{ApiBackendConfig, ApiFormat, Backend, CallError, JudgeReply, JudgeRequest, Usage};
use crate::env::DaemonEnv;
use crate::https;

const MAX_BODY_BYTES: usize = 1024 * 1024;

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
            let body = https::request(&target, request, self.cfg.timeout, MAX_BODY_BYTES, |status| status.is_success())
                .await
                .map_err(map_error)?;
            parse_reply(self.cfg.format, &body)
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
        .header(http::header::CONTENT_TYPE, "application/json");
    let body = match cfg.format {
        ApiFormat::Anthropic => {
            builder = builder.header("anthropic-version", "2023-06-01");
            if let Some(key) = key {
                builder = builder.header("x-api-key", auth_header(key)?);
            }
            json!({
                "model": cfg.model,
                "max_tokens": cfg.max_output_tokens,
                "system": req.system,
                "messages": [{"role": "user", "content": req.prompt}],
            })
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

fn parse_reply(format: ApiFormat, body: &[u8]) -> Result<JudgeReply, CallError> {
    let reply: Value = serde_json::from_slice(body).map_err(|_| CallError::BadOutput)?;
    let (text, input_field, output_field) = match format {
        ApiFormat::Anthropic => {
            let text = reply["content"]
                .as_array()
                .and_then(|parts| parts.iter().find(|part| part["type"] == "text"))
                .and_then(|part| part["text"].as_str())
                .ok_or(CallError::BadOutput)?;
            (text.to_owned(), "input_tokens", "output_tokens")
        }
        ApiFormat::Openai => {
            let content = &reply["choices"][0]["message"]["content"];
            let text = match content {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect(),
                _ => return Err(CallError::BadOutput),
            };
            (text, "prompt_tokens", "completion_tokens")
        }
    };
    if text.is_empty() {
        return Err(CallError::BadOutput);
    }
    Ok(JudgeReply {
        text,
        usage: Usage {
            input_tokens: reply["usage"][input_field].as_u64(),
            output_tokens: reply["usage"][output_field].as_u64(),
            cost_usd: None,
        },
    })
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

    fn assert_common_headers(captured: &Captured, server: &Server) {
        assert_eq!(captured.headers["content-type"], "application/json");
        assert_eq!(captured.headers["user-agent"], concat!("anycon/", env!("CARGO_PKG_VERSION")));
        assert_eq!(captured.headers["host"], server.url.strip_prefix("http://").unwrap());
        assert!(!captured.headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn anthropic_request_and_first_text_with_usage() {
        let mut server = Server::reply(200, br#"{
            "content":[{"type":"thinking","text":"ignore"},{"type":"text","text":"first"},{"type":"text","text":"second"}],
            "usage":{"input_tokens":17,"output_tokens":9},"cost_usd":1.5
        }"#).await;
        let api = backend(ApiFormat::Anthropic, &server.url, ApiKeySource::Literal("test-key".into()));
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
                "messages": [{"role": "user", "content": input().prompt}]
            })
        );
    }

    #[tokio::test]
    async fn openai_request_and_first_choice_with_usage() {
        let mut server = Server::reply(
            201,
            br#"{
            "choices":[{"message":{"content":"answer"}},{"message":{"content":"ignore"}}],
            "usage":{"prompt_tokens":20,"completion_tokens":0},"cost_usd":2
        }"#,
        )
        .await;
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
                "messages":[{"role":"system","content":input().system},{"role":"user","content":input().prompt}]
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
            assert_eq!(body.as_object().unwrap().len(), 3);
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
        let mut server = Server::reply(
            200,
            br#"{
            "choices":[{"message":{"content":[
                {"type":"image_url","text":"ignore"},
                {"type":"text","text":"one "},{"type":"text","text":"two"}
            ]}}]
        }"#,
        )
        .await;
        let api = backend(ApiFormat::Openai, &server.url, ApiKeySource::None);
        let reply = api.call(&input()).await.unwrap();
        assert_eq!(reply.text, "one two");
        assert_eq!(reply.usage, Usage::default());
        server.captured().await;
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
    async fn timeout_covers_waiting_for_headers_and_body_and_closes_connection() {
        for response in [Vec::new(), b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{".to_vec()] {
            let mut server = Server::start(response, true).await;
            let mut api = backend(ApiFormat::Openai, &server.url, ApiKeySource::None);
            api.cfg.timeout = Duration::from_millis(100);
            assert_eq!(api.call(&input()).await.unwrap_err(), CallError::Timeout);
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn oversized_fixed_and_chunked_bodies_fail_but_exact_limit_succeeds() {
        let mut body = br#"{"choices":[{"message":{"content":"ok"}}]}"#.to_vec();
        body.resize(MAX_BODY_BYTES, b' ');
        let mut server = Server::reply(200, &body).await;
        assert_eq!(
            backend(ApiFormat::Openai, &server.url, ApiKeySource::None).call(&input()).await.unwrap().text,
            "ok"
        );
        server.captured().await;
        body.push(b' ');
        let fixed = response(200, "", &body);
        let mut chunked =
            format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n", body.len()).into_bytes();
        chunked.extend_from_slice(&body);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        for response in [fixed, chunked] {
            let mut server = Server::start(response, false).await;
            assert_eq!(
                backend(ApiFormat::Openai, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err(),
                CallError::BadOutput
            );
            server.captured().await;
        }
    }

    #[tokio::test]
    async fn malformed_json_and_missing_or_empty_text_fail_without_exposing_content() {
        for format in [ApiFormat::Anthropic, ApiFormat::Openai] {
            for body in [
                PRIVATE_BODY,
                "{}",
                "null",
                r#"{"content":[],"choices":[]}"#,
                r#"{"content":[{"type":"text","text":""}],"choices":[{"message":{"content":""}}]}"#,
                r#"{"content":[{"type":"image","text":"ignore"}],"choices":[{"message":{"content":[{"type":"image","text":"ignore"}]}}]}"#,
                r#"{"content":[{"type":"text","text":42}],"choices":[{"message":{"content":42}}]}"#,
            ] {
                let mut server = Server::reply(200, body.as_bytes()).await;
                let error = backend(format, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err();
                assert_eq!(error, CallError::BadOutput);
                assert!(!format!("{error} {error:?}").contains(PRIVATE_BODY));
                server.captured().await;
            }
        }
    }

    #[tokio::test]
    async fn missing_keys_are_allowed_on_loopback_without_auth_headers() {
        for format in [ApiFormat::Anthropic, ApiFormat::Openai] {
            for source in [ApiKeySource::None, ApiKeySource::Env("UNSET".into()), ApiKeySource::Literal(String::new())]
            {
                let mut server = Server::reply(
                    200,
                    br#"{"content":[{"type":"text","text":"ok"}],"choices":[{"message":{"content":"ok"}}]}"#,
                )
                .await;
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
            let mut server = Server::reply(200, br#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
            api.cfg.base_url = server.url.clone();
            api.call(&input()).await.unwrap();
            assert_eq!(server.captured().await.headers["authorization"], format!("Bearer {key}"));
        }
    }

    #[tokio::test]
    async fn connection_failures_and_truncated_bodies_are_network_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            backend(ApiFormat::Openai, &url, ApiKeySource::None).call(&input()).await.unwrap_err(),
            CallError::Network
        );
        for response in [Vec::new(), b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort".to_vec()] {
            let mut server = Server::start(response, false).await;
            assert_eq!(
                backend(ApiFormat::Openai, &server.url, ApiKeySource::None).call(&input()).await.unwrap_err(),
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
