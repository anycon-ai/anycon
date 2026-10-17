//! WebSocket 升级时的 `Origin` 校验（docs/architecture.md § 3.3、ADR-23）。
//!
//! 放行：
//! 1. 没有 `Origin` 头——daemon / CLI / SDK 不是浏览器，不带这个头；
//! 2. `ANYCON_PUBLIC_URL` 的 origin；
//! 3. 请求自身的 `Host`（只用于这个校验和 cookie 属性，**不**用于生成任何链接）；
//! 4. 主机是回环 / RFC 1918 / 链路本地 / ULA 地址（或 `localhost`）的 **http** origin——局域网访问是一等场景。
//!
//! 其余一律 403。

use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

pub fn origin_allowed(origin: Option<&str>, host_header: Option<&str>, public_url: &Url) -> bool {
    let Some(origin) = origin else { return true };
    let Ok(o) = Url::parse(origin) else { return false };
    if !matches!(o.scheme(), "http" | "https") || o.host().is_none() {
        return false;
    }
    // 2. 公网 origin
    if o.origin() == public_url.origin() {
        return true;
    }
    // 3. 请求自身的 Host
    if let Some(h) = host_header {
        if host_matches(&o, h) {
            return true;
        }
    }
    // 4. 私有网段 / localhost 的 http
    o.scheme() == "http" && host_is_local(o.host().expect("checked above"))
}

/// `Host` 头（`host[:port]`）与 origin 的主机、端口是否一致。
fn host_matches(origin: &Url, host_header: &str) -> bool {
    // 借 Url 解析 authority，避免自己处理 IPv6 方括号。
    let Ok(h) = Url::parse(&format!("{}://{}/", origin.scheme(), host_header)) else { return false };
    h.host_str().is_some_and(|a| origin.host_str().is_some_and(|b| a.eq_ignore_ascii_case(b)))
        && h.port_or_known_default() == origin.port_or_known_default()
}

fn host_is_local(host: Host<&str>) -> bool {
    match host {
        Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        Host::Ipv4(ip) => ipv4_is_local(ip),
        Host::Ipv6(ip) => ipv6_is_local(ip),
    }
}

fn ipv4_is_local(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn ipv6_is_local(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_local(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public() -> Url {
        Url::parse("https://hub.example.com").unwrap()
    }

    fn ok(origin: &str, host: &str) -> bool {
        origin_allowed(Some(origin), Some(host), &public())
    }

    #[test]
    fn no_origin_is_allowed() {
        assert!(origin_allowed(None, Some("anything"), &public()));
    }

    #[test]
    fn public_origin_is_allowed() {
        assert!(ok("https://hub.example.com", "10.0.0.5:8080"));
        assert!(ok("https://hub.example.com:443", "10.0.0.5:8080"));
        assert!(!ok("http://hub.example.com", "10.0.0.5:8080")); // 协议不同 = origin 不同
        assert!(!ok("https://hub.example.com:8443", "10.0.0.5:8080"));
    }

    #[test]
    fn own_host_is_allowed() {
        assert!(ok("https://other.example.org", "other.example.org"));
        assert!(ok("http://other.example.org:8080", "other.example.org:8080"));
        assert!(!ok("http://other.example.org:8080", "other.example.org:9090"));
        assert!(!ok("https://evil.example", "hub.internal:8080"));
    }

    #[test]
    fn private_and_loopback_http_are_allowed() {
        for o in [
            "http://127.0.0.1:5173",
            "http://localhost:5173",
            "http://app.localhost",
            "http://[::1]:5173",
            "http://10.1.2.3",
            "http://172.16.0.9:3000",
            "http://192.168.1.10:8080",
            "http://169.254.1.1",
            "http://[fd00::1]",
            "http://[fe80::1]",
        ] {
            assert!(ok(o, "hub.example.com"), "{o} should be allowed");
        }
    }

    #[test]
    fn public_ips_and_https_private_are_rejected() {
        for o in [
            "http://8.8.8.8",
            "http://172.32.0.1",
            "http://evil.example",
            "https://192.168.1.10", // 只自动放行 http
            "null",
            "file://",
            "ftp://127.0.0.1",
            "",
        ] {
            assert!(!ok(o, "hub.example.com"), "{o:?} should be rejected");
        }
    }
}
