//! 转发到本机端口时的头部改写和响应体编码选择（docs/spec/daemon.md § 19.5）。全是纯函数。
//!
//! 请求头：`host` 改成 `localhost:<端口>`；`origin` / `referer` 里的预览 origin 换成 `http://localhost:<端口>`；
//! 去掉逐跳头和 `forwarded` / `x-forwarded-*`；`accept-encoding` 改成 `gzip`。
//! 响应头：去掉逐跳头；`location` 是指向本机同一端口的绝对 URL 时改成只有路径的形式；`set-cookie` 原样。
//! 响应体：上游已经是 gzip 的原样转发并标 `encoding = gzip`；没压缩的文本类且没有 `content-range` 的由 daemon 边读边压缩。

use anycon_protocol::PreviewBodyEncoding;

/// 逐跳头（RFC 9110 § 7.6.1 加上常见的 `proxy-connection`）。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 小于等于这么多字节的响应体不压缩（压缩省不了什么，还多一份 gzip 头尾）。
pub const COMPRESS_MIN_BYTES: u64 = 1024;

pub type Headers = Vec<(String, String)>;

/// `Connection` 头里列出的、也要当作逐跳头去掉的头名。
fn connection_listed(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k == "connection")
        .flat_map(|(_, v)| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn is_hop(name: &str, listed: &[String]) -> bool {
    HOP_BY_HOP.contains(&name) || listed.iter().any(|l| l == name)
}

/// WebSocket 升级请求里由上游握手自己生成的头（daemon 生成 `sec-websocket-key`，不带扩展）。
const WS_GENERATED: &[&str] =
    &["sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions", "sec-websocket-accept"];

/// 改写请求头。`label` 是这条隧道的预览 label（预览 origin 的第一段主机名），`port` 是目标端口。
/// 头名已经是小写；输入顺序保留，`host` 放在最前面。
pub fn request_headers(headers: &[(String, String)], label: &str, port: u16, websocket: bool) -> Headers {
    let listed = connection_listed(headers);
    let local_origin = format!("http://localhost:{port}");
    let mut out: Headers = vec![("host".to_string(), format!("localhost:{port}"))];
    for (k, v) in headers {
        let k = k.to_ascii_lowercase();
        if k == "host" || is_hop(&k, &listed) || k == "forwarded" || k.starts_with("x-forwarded-") || k == "expect" {
            continue;
        }
        if websocket && WS_GENERATED.contains(&k.as_str()) {
            continue;
        }
        match k.as_str() {
            "origin" | "referer" => out.push((k, rewrite_origin(v, label, &local_origin))),
            // 下面统一补 `accept-encoding: gzip`
            "accept-encoding" => {}
            _ => out.push((k, v.clone())),
        }
    }
    // 只要 gzip：daemon 不解 br / zstd。service worker 看不到 `accept-encoding`（浏览器的禁止头），所以预览页发来的请求
    // 里通常没有它，这里总是补上——上游据此压缩，daemon 直接转发。WebSocket 握手不带这个头
    if !websocket {
        out.push(("accept-encoding".to_string(), "gzip".to_string()));
    }
    out
}

/// `origin` / `referer` 的值如果指向这条隧道的预览 origin（主机名是 `<label>.<预览域名>`），换成本机 origin，
/// 路径和查询串保留；其他值（别的站点、`null`）原样返回。
pub fn rewrite_origin(value: &str, label: &str, local_origin: &str) -> String {
    let Some((_, rest)) = value.split_once("://") else { return value.to_string() };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host =
        host_port
            .rsplit_once(':')
            .map_or(host_port, |(h, p)| if p.bytes().all(|b| b.is_ascii_digit()) { h } else { host_port });
    let host = host.to_ascii_lowercase();
    match host.split_once('.') {
        Some((first, rest_host)) if first == label && !rest_host.is_empty() => format!("{local_origin}{tail}"),
        _ => value.to_string(),
    }
}

/// 响应体怎么处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyPlan {
    /// 原样转发（`content-encoding` 和 `content-length` 都保留）。
    Identity,
    /// 上游已经是 gzip：原样转发，标 `encoding = gzip`，去掉 `content-encoding` / `content-length`。
    PassGzip,
    /// 上游没压缩、是文本类：daemon 边读边 gzip，去掉 `content-length`。
    Compress,
}

impl BodyPlan {
    pub fn encoding(self) -> PreviewBodyEncoding {
        match self {
            BodyPlan::Identity => PreviewBodyEncoding::Identity,
            BodyPlan::PassGzip | BodyPlan::Compress => PreviewBodyEncoding::Gzip,
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// content-type 是不是值得压缩的文本类：`text/*`、JavaScript、JSON、XML、SVG、wasm。
/// `text/event-stream` 不算：事件流要即时到达，压缩器会攒着。
pub fn is_compressible(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime == "text/event-stream" {
        return false;
    }
    mime.starts_with("text/")
        || mime.contains("javascript")
        || mime.contains("ecmascript")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "image/svg+xml"
        || mime == "application/wasm"
}

/// 按上游响应选择处理方式。`head_request`：请求是 HEAD（没有响应体，不压缩）。
pub fn plan_body(status: u16, headers: &[(String, String)], head_request: bool) -> BodyPlan {
    let encoding = header(headers, "content-encoding").map(|v| v.trim().to_ascii_lowercase());
    match encoding.as_deref() {
        Some("gzip") | Some("x-gzip") => return BodyPlan::PassGzip,
        Some("") | Some("identity") | None => {}
        // br / deflate / zstd 等：daemon 不解也不重新压缩，连同 content-encoding 头原样给预览页
        Some(_) => return BodyPlan::Identity,
    }
    if head_request || status < 200 || status == 204 || status == 304 || status == 101 {
        return BodyPlan::Identity;
    }
    if header(headers, "content-range").is_some() {
        return BodyPlan::Identity;
    }
    if header(headers, "cache-control").is_some_and(|v| v.to_ascii_lowercase().contains("no-transform")) {
        return BodyPlan::Identity;
    }
    let text_like = header(headers, "content-type").is_some_and(is_compressible);
    // 已知长度不大于 1 KiB 的不压；长度未知（chunked）的先压，反正压出来也不会更大太多
    let small = header(headers, "content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .is_some_and(|n| n <= COMPRESS_MIN_BYTES);
    if text_like && !small {
        BodyPlan::Compress
    } else {
        BodyPlan::Identity
    }
}

/// 改写响应头并给出响应体的处理方式。
pub fn response_headers(
    status: u16,
    headers: &[(String, String)],
    port: u16,
    head_request: bool,
) -> (Headers, BodyPlan) {
    let plan = plan_body(status, headers, head_request);
    let listed = connection_listed(headers);
    let mut out = Headers::with_capacity(headers.len());
    for (k, v) in headers {
        let k = k.to_ascii_lowercase();
        if is_hop(&k, &listed) {
            continue;
        }
        match (k.as_str(), plan) {
            ("content-encoding", BodyPlan::PassGzip) => continue,
            ("content-length" | "content-encoding", BodyPlan::Compress) => continue,
            ("content-length", BodyPlan::PassGzip) => continue,
            ("location", _) => out.push((k, rewrite_location(v, port))),
            _ => out.push((k, v.clone())),
        }
    }
    (out, plan)
}

/// 响应体编码不动的响应头改写（WebSocket 升级被拒绝时上游的响应）：去逐跳头，`location` 同样改成路径，其余原样。
pub fn response_headers_verbatim(headers: &[(String, String)], port: u16) -> Headers {
    let listed = connection_listed(headers);
    headers
        .iter()
        .filter(|(k, _)| !is_hop(&k.to_ascii_lowercase(), &listed))
        .map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            let v = if k == "location" { rewrite_location(v, port) } else { v.clone() };
            (k, v)
        })
        .collect()
}

/// `location` 是指向 `localhost` / 回环地址同一端口的绝对 URL 时，改成只有路径（含查询串和片段）的形式。
pub fn rewrite_location(value: &str, port: u16) -> String {
    let Some((scheme, rest)) = value.split_once("://") else { return value.to_string() };
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return value.to_string(),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, loc_port) = if let Some(v6) = host_port.strip_prefix('[') {
        match v6.split_once(']') {
            Some((h, t)) => (h.to_string(), t.strip_prefix(':').map(str::to_string)),
            None => return value.to_string(),
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (host_port.to_string(), None),
        }
    };
    let loc_port = match loc_port {
        Some(p) => match p.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return value.to_string(),
        },
        None => default_port,
    };
    let host = host.to_ascii_lowercase();
    let loopback = host == "localhost"
        || host == "::1"
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a == std::net::Ipv4Addr::LOCALHOST);
    if !loopback || loc_port != port {
        return value.to_string();
    }
    match tail {
        "" => "/".to_string(),
        t if t.starts_with('/') => t.to_string(),
        t => format!("/{t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(list: &[(&str, &str)]) -> Headers {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const LABEL: &str = "abcdefghijklmnopqrstuvwxyz";

    #[test]
    fn request_headers_are_rewritten_for_the_local_server() {
        let input = h(&[
            ("host", "abcdefghijklmnopqrstuvwxyz.aiplace.app"),
            ("accept", "text/html"),
            ("accept-encoding", "br, gzip, deflate"),
            ("origin", "https://abcdefghijklmnopqrstuvwxyz.aiplace.app"),
            ("referer", "https://abcdefghijklmnopqrstuvwxyz.aiplace.app/app/page?x=1#frag"),
            ("cookie", "a=b; c=d"),
            ("connection", "keep-alive, x-custom"),
            ("keep-alive", "timeout=5"),
            ("x-custom", "drop-me, it is listed in connection"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("proxy-authorization", "Basic xx"),
            ("forwarded", "for=1.2.3.4"),
            ("x-forwarded-for", "1.2.3.4"),
            ("x-forwarded-proto", "https"),
            ("expect", "100-continue"),
            ("content-type", "application/json"),
            ("x-keep", "yes"),
        ]);
        let out = request_headers(&input, LABEL, 5173, false);
        assert_eq!(
            out,
            h(&[
                ("host", "localhost:5173"),
                ("accept", "text/html"),
                ("origin", "http://localhost:5173"),
                ("referer", "http://localhost:5173/app/page?x=1#frag"),
                ("cookie", "a=b; c=d"),
                ("content-type", "application/json"),
                ("x-keep", "yes"),
                ("accept-encoding", "gzip"),
            ])
        );
        // 请求里没有 accept-encoding（service worker 看不到它）也补上；只补一次，放在最后
        let added = request_headers(&h(&[("accept", "*/*")]), LABEL, 80, false);
        assert_eq!(added.last(), Some(&("accept-encoding".to_string(), "gzip".to_string())));
        let twice = request_headers(&h(&[("accept-encoding", "br"), ("accept-encoding", "zstd")]), LABEL, 80, false);
        assert_eq!(twice.iter().filter(|(k, _)| k == "accept-encoding").count(), 1);
    }

    #[test]
    fn websocket_upgrade_requests_drop_what_the_upstream_handshake_generates() {
        let input = h(&[
            ("host", "x.aiplace.app"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "k"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("sec-websocket-protocol", "vite-hmr"),
            ("origin", "https://abcdefghijklmnopqrstuvwxyz.aiplace.app"),
            ("accept-encoding", "gzip"),
            ("cookie", "s=1"),
        ]);
        let out = request_headers(&input, LABEL, 24678, true);
        assert_eq!(
            out,
            h(&[
                ("host", "localhost:24678"),
                ("sec-websocket-protocol", "vite-hmr"),
                ("origin", "http://localhost:24678"),
                ("cookie", "s=1"),
            ])
        );
    }

    #[test]
    fn origins_are_replaced_only_when_they_are_the_preview_origin() {
        let local = "http://localhost:3000";
        let r = |v: &str| rewrite_origin(v, LABEL, local);
        assert_eq!(r("https://abcdefghijklmnopqrstuvwxyz.aiplace.app"), local);
        assert_eq!(r("https://abcdefghijklmnopqrstuvwxyz.aiplace.app:8443/a/b?c=d"), format!("{local}/a/b?c=d"));
        assert_eq!(r("http://ABCDEFGHIJKLMNOPQRSTUVWXYZ.preview.localhost:8080/x"), format!("{local}/x"));
        assert_eq!(r("https://other-label.aiplace.app"), "https://other-label.aiplace.app");
        assert_eq!(
            r("https://evil.com/abcdefghijklmnopqrstuvwxyz.aiplace.app"),
            "https://evil.com/abcdefghijklmnopqrstuvwxyz.aiplace.app"
        );
        assert_eq!(
            r("https://abcdefghijklmnopqrstuvwxyz"),
            "https://abcdefghijklmnopqrstuvwxyz",
            "no registrable part"
        );
        assert_eq!(
            r("https://abcdefghijklmnopqrstuvwxyzx.aiplace.app"),
            "https://abcdefghijklmnopqrstuvwxyzx.aiplace.app"
        );
        assert_eq!(r("null"), "null");
        assert_eq!(r(""), "");
    }

    #[test]
    fn location_to_the_same_local_port_becomes_a_path() {
        for (input, port, want) in [
            ("http://localhost:3000/login", 3000, "/login"),
            ("http://localhost:3000", 3000, "/"),
            ("http://127.0.0.1:3000/a?b=c#d", 3000, "/a?b=c#d"),
            ("https://[::1]:3000/x", 3000, "/x"),
            ("http://LocalHost:3000?q=1", 3000, "/?q=1"),
            ("http://localhost/x", 80, "/x"),
            ("https://localhost/x", 443, "/x"),
            // 不同端口、别的主机、相对路径、别的 scheme：不动
            ("http://localhost:3001/login", 3000, "http://localhost:3001/login"),
            ("http://localhost/x", 3000, "http://localhost/x"),
            ("http://example.com:3000/x", 3000, "http://example.com:3000/x"),
            ("http://192.168.1.2:3000/x", 3000, "http://192.168.1.2:3000/x"),
            ("/already/relative", 3000, "/already/relative"),
            ("//localhost:3000/x", 3000, "//localhost:3000/x"),
            ("mailto://localhost:3000", 3000, "mailto://localhost:3000"),
            ("http://localhost:abc/x", 3000, "http://localhost:abc/x"),
        ] {
            assert_eq!(rewrite_location(input, port), want, "{input}");
        }
    }

    #[test]
    fn compressible_content_types() {
        for t in [
            "text/html; charset=utf-8",
            "text/css",
            "text/plain",
            "application/javascript",
            "text/javascript; charset=utf-8",
            "application/x-javascript",
            "application/json",
            "application/ld+json",
            "application/xml",
            "application/atom+xml",
            "image/svg+xml",
            "application/wasm",
            "TEXT/HTML",
        ] {
            assert!(is_compressible(t), "{t}");
        }
        for t in [
            "image/png",
            "application/octet-stream",
            "font/woff2",
            "video/mp4",
            "text/event-stream",
            "application/zip",
            "",
        ] {
            assert!(!is_compressible(t), "{t:?}");
        }
    }

    #[test]
    fn body_plans() {
        let big = ("content-length", "5000");
        let html = ("content-type", "text/html");
        // 上游已经 gzip
        assert_eq!(plan_body(200, &h(&[("content-encoding", "gzip"), html]), false), BodyPlan::PassGzip);
        assert_eq!(plan_body(200, &h(&[("content-encoding", "GZIP")]), false), BodyPlan::PassGzip);
        // 其他编码原样
        assert_eq!(plan_body(200, &h(&[("content-encoding", "br"), html, big]), false), BodyPlan::Identity);
        // 没压缩的文本类，大于 1 KiB 或长度未知：压
        assert_eq!(plan_body(200, &h(&[html, big]), false), BodyPlan::Compress);
        assert_eq!(plan_body(200, &h(&[html]), false), BodyPlan::Compress);
        assert_eq!(plan_body(404, &h(&[html, big]), false), BodyPlan::Compress);
        // 不压的：小的、range、非文本、no-transform、事件流、没有响应体的
        assert_eq!(plan_body(200, &h(&[html, ("content-length", "1024")]), false), BodyPlan::Identity);
        assert_eq!(plan_body(200, &h(&[html, ("content-length", "1025")]), false), BodyPlan::Compress);
        assert_eq!(plan_body(206, &h(&[html, big, ("content-range", "bytes 0-4999/9999")]), false), BodyPlan::Identity);
        assert_eq!(plan_body(200, &h(&[("content-type", "image/png"), big]), false), BodyPlan::Identity);
        assert_eq!(
            plan_body(200, &h(&[html, big, ("cache-control", "no-transform, max-age=1")]), false),
            BodyPlan::Identity
        );
        assert_eq!(plan_body(200, &h(&[("content-type", "text/event-stream")]), false), BodyPlan::Identity);
        assert_eq!(plan_body(204, &h(&[html]), false), BodyPlan::Identity);
        assert_eq!(plan_body(304, &h(&[html, big]), false), BodyPlan::Identity);
        assert_eq!(plan_body(200, &h(&[html, big]), true), BodyPlan::Identity, "HEAD");
        assert_eq!(BodyPlan::Compress.encoding(), PreviewBodyEncoding::Gzip);
        assert_eq!(BodyPlan::Identity.encoding(), PreviewBodyEncoding::Identity);
    }

    #[test]
    fn response_headers_are_rewritten_per_plan() {
        let upstream = h(&[
            ("content-type", "text/html; charset=utf-8"),
            ("content-length", "5000"),
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("location", "http://localhost:3000/next"),
            ("set-cookie", "a=1; Path=/; HttpOnly"),
            ("set-cookie", "b=2; Path=/"),
            ("etag", "W/\"x\""),
        ]);
        let (out, plan) = response_headers(302, &upstream, 3000, false);
        assert_eq!(plan, BodyPlan::Compress);
        assert_eq!(
            out,
            h(&[
                ("content-type", "text/html; charset=utf-8"),
                ("location", "/next"),
                ("set-cookie", "a=1; Path=/; HttpOnly"),
                ("set-cookie", "b=2; Path=/"),
                ("etag", "W/\"x\""),
            ])
        );
        // 上游 gzip：去掉 content-encoding 和 content-length，标 gzip
        let gz = h(&[("content-encoding", "gzip"), ("content-length", "300"), ("content-type", "text/css")]);
        let (out, plan) = response_headers(200, &gz, 3000, false);
        assert_eq!(plan, BodyPlan::PassGzip);
        assert_eq!(out, h(&[("content-type", "text/css")]));
        // 原样：content-length 保留
        let png = h(&[("content-type", "image/png"), ("content-length", "9999")]);
        let (out, plan) = response_headers(200, &png, 3000, false);
        assert_eq!((out, plan), (png, BodyPlan::Identity));
        // 未知编码保留 content-encoding
        let br = h(&[("content-encoding", "br"), ("content-length", "10")]);
        assert_eq!(response_headers(200, &br, 1, false).0, br);
    }
}
