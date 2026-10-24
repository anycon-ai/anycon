//! OAuth 出站 HTTP 客户端：hyper + hyper-rustls（`ring` + 内置 webpki 根证书），和 Web Push 同一套 TLS 配置，
//! 不另引 HTTP 客户端。
//!
//! - 每次调用（连接 + 发送 + 读完响应体）一个总超时；
//! - 响应体最多 [`MAX_BODY`]，超了就当失败；
//! - 不跟随重定向、不读系统代理；
//! - 错误只分类，**不带 URL 和响应内容**（URL 里可能有秘密，响应里可能有 token），日志只会看到类别。

use std::time::Duration;

use axum::body::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// 单个响应体的上限。提供方的 JSON 都很小。
pub const MAX_BODY: usize = 256 * 1024;

/// 请求失败的类别（不含 URL、不含响应内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("timed out")]
    Timeout,
    #[error("cannot connect")]
    Connect,
    #[error("the request failed")]
    Request,
    #[error("the response is too large")]
    TooLarge,
    #[error("the request cannot be built")]
    BadRequest,
}

/// 一次响应：状态码和（有上限的）响应体。
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Bytes,
}

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct HttpClient {
    client: HttpsClient,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(timeout: Duration) -> Result<Self, String> {
        let tls = crate::push::tls_config().map_err(|e| e.to_string())?;
        let mut http = HttpConnector::new();
        // 交给 hyper-rustls 判断 scheme：生产端点全是 https；只有测试里指向 127.0.0.1 假提供方的才是 http。
        http.enforce_http(false);
        http.set_connect_timeout(Some(timeout));
        http.set_nodelay(true);
        let connector =
            HttpsConnectorBuilder::new().with_tls_config(tls).https_or_http().enable_http1().wrap_connector(http);
        // 登录是用户触发的、不频繁：不留闲置连接，免得复用到对端已经关掉的连接。
        let client = Client::builder(TokioExecutor::new()).pool_max_idle_per_host(0).build(connector);
        Ok(Self { client, timeout })
    }

    /// `GET`，带这些请求头。
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, HttpError> {
        self.send("GET", url, headers, Bytes::new()).await
    }

    /// `POST application/x-www-form-urlencoded`。`form` 是已经编码好的表单。
    pub async fn post_form(&self, url: &str, headers: &[(&str, &str)], form: String) -> Result<Response, HttpError> {
        let mut headers = headers.to_vec();
        headers.push(("Content-Type", "application/x-www-form-urlencoded"));
        self.send("POST", url, &headers, Bytes::from(form)).await
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> Result<Response, HttpError> {
        let mut request = hyper::Request::builder().method(method).uri(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Full::new(body)).map_err(|_| HttpError::BadRequest)?;
        let call = async {
            let response = self.client.request(request).await.map_err(|e| {
                if e.is_connect() {
                    HttpError::Connect
                } else {
                    HttpError::Request
                }
            })?;
            let status = response.status().as_u16();
            let body = Limited::new(response.into_body(), MAX_BODY)
                .collect()
                .await
                .map_err(|e| {
                    if e.is::<http_body_util::LengthLimitError>() {
                        HttpError::TooLarge
                    } else {
                        HttpError::Request
                    }
                })?
                .to_bytes();
            Ok(Response { status, body })
        };
        tokio::time::timeout(self.timeout, call).await.unwrap_or(Err(HttpError::Timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 一个只回固定响应的本机 HTTP 服务；`delay` 之后才开始回。
    async fn serve_once(response: Vec<u8>, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{addr}/x")
    }

    fn ok(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[tokio::test]
    async fn reads_status_and_body() {
        let url = serve_once(ok("hello"), Duration::ZERO).await;
        let c = HttpClient::new(Duration::from_secs(5)).unwrap();
        let r = c.get(&url, &[("Accept", "application/json")]).await.unwrap();
        assert_eq!((r.status, &r.body[..]), (200, &b"hello"[..]));
        let r = c.post_form(&url, &[], "a=b".into()).await.unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn a_slow_peer_times_out_without_leaking_the_url() {
        let url = serve_once(ok("late"), Duration::from_secs(3)).await;
        let c = HttpClient::new(Duration::from_millis(150)).unwrap();
        let started = std::time::Instant::now();
        let e = c.get(&url, &[]).await.unwrap_err();
        assert_eq!(e, HttpError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!e.to_string().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn refuses_to_follow_redirects_and_caps_the_body() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let url = serve_once(redirect, Duration::ZERO).await;
        let c = HttpClient::new(Duration::from_secs(5)).unwrap();
        let r = c.get(&url, &[]).await.unwrap();
        assert_eq!(r.status, 302, "the redirect is reported, not followed");

        let big = "x".repeat(MAX_BODY + 1);
        let url = serve_once(ok(&big), Duration::ZERO).await;
        assert_eq!(c.get(&url, &[]).await.unwrap_err(), HttpError::TooLarge);
    }

    #[tokio::test]
    async fn connection_failures_are_classified() {
        let c = HttpClient::new(Duration::from_secs(2)).unwrap();
        let e = c.get("http://127.0.0.1:1/x", &[]).await.unwrap_err();
        assert!(matches!(e, HttpError::Connect | HttpError::Request), "{e:?}");
        assert_eq!(c.get("not a url", &[]).await.unwrap_err(), HttpError::BadRequest);
    }
}
