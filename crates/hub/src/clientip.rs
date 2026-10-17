//! 真实客户端 IP 与请求协议。
//!
//! 只有当直连对端（TCP peer）落在 `ANYCON_TRUSTED_PROXIES` 内时，才相信 `X-Forwarded-For` /
//! `X-Forwarded-Proto`。限速靠这个 IP，配错了限速就形同虚设（docs/deploy.md § 3）。

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderName};
use ipnet::IpNet;

/// 解析后的客户端上下文。
#[derive(Debug, Clone, Copy)]
pub struct ClientCtx {
    /// 真实客户端 IP（可信代理之外的最右一跳）。
    pub ip: IpAddr,
    /// 请求是否经由 https（只信任可信代理转发的 `X-Forwarded-Proto`）。
    pub secure: bool,
}

const XFF: HeaderName = HeaderName::from_static("x-forwarded-for");
const XFP: HeaderName = HeaderName::from_static("x-forwarded-proto");

fn canonical(ip: IpAddr) -> IpAddr {
    ip.to_canonical()
}

fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|n| n.contains(&ip))
}

/// 解析 `X-Forwarded-For` 里的一项（允许带端口、方括号）。
fn parse_hop(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(canonical(ip));
    }
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Some(canonical(sa.ip()));
    }
    s.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok().map(canonical)
}

/// 由直连对端和请求头得出 [`ClientCtx`]。
pub fn resolve(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpNet]) -> ClientCtx {
    let peer_ip = canonical(peer.ip());
    if !is_trusted(peer_ip, trusted) {
        return ClientCtx { ip: peer_ip, secure: false };
    }

    // 从右往左跳过可信代理，第一个不可信的就是客户端；全可信则取最左。
    let hops: Vec<IpAddr> = headers
        .get_all(&XFF)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(parse_hop)
        .collect();
    let ip =
        hops.iter().rev().find(|ip| !is_trusted(**ip, trusted)).or_else(|| hops.first()).copied().unwrap_or(peer_ip);

    let secure = headers
        .get(&XFP)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    ClientCtx { ip, secure }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn headers(xff: Option<&str>, xfp: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = xff {
            h.insert(XFF, HeaderValue::from_str(v).unwrap());
        }
        if let Some(v) = xfp {
            h.insert(XFP, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        let peer: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let c = resolve(peer, &headers(Some("1.2.3.4"), Some("https")), &nets(&["10.0.0.0/8"]));
        assert_eq!(c.ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        assert!(!c.secure);
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_hop() {
        let peer: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let c = resolve(peer, &headers(Some("9.9.9.9, 1.2.3.4, 10.0.0.7"), Some("https")), &nets(&["10.0.0.0/8"]));
        assert_eq!(c.ip, "1.2.3.4".parse::<IpAddr>().unwrap());
        assert!(c.secure);
    }

    #[test]
    fn trusted_peer_without_headers_is_the_peer() {
        let peer: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let c = resolve(peer, &HeaderMap::new(), &nets(&["10.0.0.0/8"]));
        assert_eq!(c.ip, "10.0.0.2".parse::<IpAddr>().unwrap());
        assert!(!c.secure);
    }

    #[test]
    fn mapped_ipv6_peer_is_canonicalised() {
        let peer: SocketAddr = "[::ffff:10.0.0.2]:5000".parse().unwrap();
        let c = resolve(peer, &headers(Some("1.2.3.4"), None), &nets(&["10.0.0.0/8"]));
        assert_eq!(c.ip, "1.2.3.4".parse::<IpAddr>().unwrap());
    }
}
