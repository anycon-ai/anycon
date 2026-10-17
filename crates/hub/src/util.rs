//! 小工具：时间、随机 id / token、token 哈希、IP 前缀。
//!
//! 所有 token（登录 token、设备 token、邀请码、cookie 值）都是 32 字节随机数的 hex，
//! 库里只存 blake2s-256 的 hex 哈希。

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use blake2::{Blake2s256, Digest};
use rand::rngs::OsRng;
use rand::RngCore;

/// 当前 Unix 毫秒。
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `n` 字节的密码学随机数，hex 编码（长度 `2n`）。
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// 对外 id（设备、账号、会话、邀请……）：128 bit 随机，32 位小写 hex。
pub fn new_id() -> String {
    random_hex(16)
}

/// 秘密 token：256 bit 随机，64 位小写 hex。
pub fn new_secret() -> String {
    random_hex(32)
}

/// token 的存库形式：blake2s-256 的 hex。
pub fn hash_token(token: &str) -> String {
    hex::encode(Blake2s256::digest(token.as_bytes()))
}

/// 日志里代替邮箱地址的标记：规范化邮箱的 blake2s 哈希的前 8 位（docs/spec/hub.md § 10）。
pub fn email_tag(email: &str) -> String {
    hash_token(email)[..8].to_string()
}

/// 常量时间比较两个字符串（用于哈希比较）。
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 审计 / 会话列表用的 IP 前缀：IPv4 `/24`，IPv6 `/48`。不存完整 IP。
pub fn ip_prefix(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
    }
}

/// 截断过长的客户端自报字符串（UA、设备名等），避免被塞满库。
pub fn clip(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 两个设备 id 的归一化顺序（`pairings` 表约定 `a < b`）。
pub fn ordered<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let h = hash_token("abc");
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash_token("abc"));
        assert_ne!(h, hash_token("abd"));
    }

    #[test]
    fn email_tags_are_short_and_stable_and_do_not_contain_the_address() {
        let t = email_tag("alice@example.com");
        assert_eq!(t.len(), 8);
        assert_eq!(t, email_tag("alice@example.com"));
        assert_ne!(t, email_tag("bob@example.com"));
        assert!(!t.contains("alice"));
    }

    #[test]
    fn ip_prefixes() {
        assert_eq!(ip_prefix("10.1.2.3".parse().unwrap()), "10.1.2.0/24");
        assert_eq!(ip_prefix("2001:db8:1:2::5".parse().unwrap()), "2001:db8:1::/48");
    }

    #[test]
    fn ordering() {
        assert_eq!(ordered("b", "a"), ("a", "b"));
        assert_eq!(ordered("a", "b"), ("a", "b"));
    }
}
