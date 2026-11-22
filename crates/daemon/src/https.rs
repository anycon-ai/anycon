//! 一次性 HTTP/1.1 客户端：HTTPS 使用 webpki 根证书，明文只允许回环地址。
//! 不跟随重定向、不添加身份标识；调用方提供请求头、可接受的状态码、回应体上限和整体超时。
//! 错误只包含固定分类、状态码或 I/O 错误种类，不携带 URL、请求头或请求与回应的内容。
//! 超时或取消会终止连接任务，避免请求在后台继续运行。

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("unsupported URL")]
    BadUrl,
    #[error("timed out")]
    Timeout,
    #[error("connect: {0}")]
    Connect(std::io::ErrorKind),
    #[error("TLS handshake failed")]
    Tls,
    #[error("HTTP exchange failed")]
    Http,
    #[error("unexpected HTTP status {0}")]
    Status(u16),
    #[error("response body too large")]
    TooLarge,
    #[error("response body unreadable")]
    Body,
}

pub(crate) fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 解析好的请求地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) tls: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `Host` 头。
    pub(crate) authority: String,
    pub(crate) path: String,
}

/// `https://host[:port]/path`；`http://` 只接受本机回环地址。不接受查询串、片段和用户信息。
pub(crate) fn parse_target(url: &str) -> Result<Target, Error> {
    let (scheme, rest) = url.split_once("://").ok_or(Error::BadUrl)?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        _ => return Err(Error::BadUrl),
    };
    if rest.contains(['?', '#', '@']) || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::BadUrl);
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (h, p) = stripped.split_once(']').ok_or(Error::BadUrl)?;
        let port = match p {
            "" => default_port,
            p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or(Error::BadUrl)?,
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| Error::BadUrl)?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(Error::BadUrl);
    }
    if !tls {
        let loopback = is_loopback(&host);
        if !loopback {
            return Err(Error::BadUrl);
        }
    }
    Ok(Target { tls, host, port, authority: authority.to_string(), path: path.to_string() })
}

/// 被丢弃时结束连接任务，超时和调用方取消都不会留下后台任务。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn exchange<S>(
    io: S,
    req: http::Request<Full<Bytes>>,
    max_body_bytes: usize,
    accept_status: fn(http::StatusCode) -> bool,
) -> Result<Bytes, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(TokioIo::new(io)).await.map_err(|_| Error::Http)?;
    let _conn = AbortOnDrop(tokio::spawn(async move {
        let _ = conn.await;
    }));
    let resp = sender.send_request(req).await.map_err(|_| Error::Http)?;
    // 错误状态直接返回，既不跟随 Location，也不读取可能包含用户内容的错误体。
    if !accept_status(resp.status()) {
        return Err(Error::Status(resp.status().as_u16()));
    }
    let body = Limited::new(resp.into_body(), max_body_bytes).collect().await.map_err(|e| {
        if e.is::<LengthLimitError>() {
            Error::TooLarge
        } else {
            Error::Body
        }
    })?;
    Ok(body.to_bytes())
}

/// 时限覆盖 DNS、TCP、TLS、发送请求和读完有上限的回应体。
pub(crate) async fn request(
    target: &Target,
    req: http::Request<Full<Bytes>>,
    timeout: Duration,
    max_body_bytes: usize,
    accept_status: fn(http::StatusCode) -> bool,
) -> Result<Bytes, Error> {
    let work = async {
        let tcp =
            TcpStream::connect((target.host.as_str(), target.port)).await.map_err(|e| Error::Connect(e.kind()))?;
        if target.tls {
            let name = rustls::pki_types::ServerName::try_from(target.host.clone()).map_err(|_| Error::BadUrl)?;
            let tls = tokio_rustls::TlsConnector::from(crate::hub::tls_config())
                .connect(name, tcp)
                .await
                .map_err(|_| Error::Tls)?;
            exchange(tls, req, max_body_bytes, accept_status).await
        } else {
            exchange(tcp, req, max_body_bytes, accept_status).await
        }
    };
    tokio::time::timeout(timeout, work).await.map_err(|_| Error::Timeout)?
}
