//! 授权流程里和「这一次流程」绑定的小东西：PKCE、`next` 校验、`anycon_oauth` cookie。
//!
//! `state` 本身是 32 字节随机数的 hex：它出现在授权 URL 和浏览器的 `anycon_oauth` cookie 里，库里只存哈希。
//! 回调时 cookie 必须和 URL 里的 `state` 一致（常数时间比较），这把「授权码注入」和登录 CSRF 挡在外面：
//! 攻击者能让受害者的浏览器访问一个带攻击者 `state` 的回调，但没法给受害者的浏览器种下对应的 cookie。

use std::time::Duration;

use axum::http::HeaderValue;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 授权 state 的有效期。
pub const STATE_TTL: Duration = Duration::from_secs(10 * 60);

pub const COOKIE_NAME: &str = "anycon_oauth";
/// cookie 只发给回调路径。
pub const COOKIE_PATH: &str = "/auth/callback";

/// `next` 的长度上限（字节）。
const NEXT_MAX_BYTES: usize = 512;

/// PKCE 的 `code_verifier`：32 字节随机数的 base64url（43 个字符，RFC 7636 § 4.1）。
pub fn new_verifier() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `code_challenge = BASE64URL(SHA256(code_verifier))`（`code_challenge_method=S256`）。
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 校验登录成功之后要回到的站内路径。通过才返回；不通过（调用方当作没带，回 `/`）：
/// 必须以 `/` 开头、不能以 `//` 或 `/\` 开头（浏览器会把它们当协议相对地址 / 反斜杠换成斜杠，变成别的站点）、
/// 不含控制字符（浏览器会吞掉 Tab / 换行，`/\t/evil.example` 就成了 `//evil.example`）、最多 512 字节。
pub fn sanitize_next(raw: &str) -> Option<String> {
    let ok = raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.starts_with("/\\")
        && !raw.chars().any(char::is_control)
        && raw.len() <= NEXT_MAX_BYTES;
    ok.then(|| raw.to_string())
}

/// 写进 `Location` 之前的编码：空格、控制字符和非 ASCII 字符百分号编码（`HeaderValue` 只收可见 ASCII），其余原样。
const LOCATION: &AsciiSet = &CONTROLS.add(b' ');

/// 把（已校验的）站内路径变成可以放进 `Location` 的值。
pub fn location_value(path: &str) -> String {
    utf8_percent_encode(path, LOCATION).to_string()
}

/// 授权开始时种的 cookie：`anycon_oauth=<state>; Path=/auth/callback; Max-Age=600; HttpOnly; SameSite=Lax[; Secure]`。
/// `SameSite=Lax` 够用：提供方回到回调是顶层 GET 导航。
pub fn set_cookie_header(state: &str, secure: bool) -> HeaderValue {
    cookie_header(state, STATE_TTL.as_secs(), secure)
}

/// 回调时清掉这个 cookie。
pub fn clear_cookie_header(secure: bool) -> HeaderValue {
    cookie_header("", 0, secure)
}

fn cookie_header(value: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={value}; Path={COOKIE_PATH}; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie header is ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        // RFC 7636 附录 B
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_random_urlsafe_and_the_right_length() {
        let a = new_verifier();
        let b = new_verifier();
        assert_ne!(a, b);
        for v in [&a, &b] {
            assert_eq!(v.len(), 43, "{v}");
            assert!(v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "{v}");
        }
        assert_eq!(challenge(&a).len(), 43);
    }

    #[test]
    fn next_accepts_only_same_site_paths() {
        for ok in ["/", "/chat", "/chat?x=1&y=2#frag", "/a/b/c", "/%2F%2Fevil.example", "/settings/login?linked=github"]
        {
            assert_eq!(sanitize_next(ok).as_deref(), Some(ok), "{ok}");
        }
        let at_limit = format!("/{}", "a".repeat(511));
        assert_eq!(sanitize_next(&at_limit).as_deref(), Some(at_limit.as_str()));
        for bad in [
            "",
            "chat",
            "//evil.example",
            "///evil.example",
            "/\\evil.example",
            "/\\\\evil.example",
            "https://evil.example/",
            "http:/evil.example",
            "javascript:alert(1)",
            "/\t/evil.example",
            "/\n/evil.example",
            "/ok\r\nSet-Cookie: x=1",
            "/nul\0",
            "/del\u{7f}",
            "/\u{85}",
            " /chat",
        ] {
            assert_eq!(sanitize_next(bad), None, "{bad:?}");
        }
        assert_eq!(sanitize_next(&format!("/{}", "a".repeat(512))), None, "513 bytes");
        // 按字节算，不是按字符
        assert_eq!(sanitize_next(&format!("/{}", "é".repeat(256))), None, "513 bytes of two-byte chars");
    }

    #[test]
    fn locations_are_encoded_into_visible_ascii() {
        assert_eq!(location_value("/chat?q=a b&n=é#x"), "/chat?q=a%20b&n=%C3%A9#x");
        assert_eq!(location_value("/plain"), "/plain");
        assert!(HeaderValue::from_str(&location_value("/ü ö")).is_ok());
    }

    #[test]
    fn cookie_attributes() {
        let set = set_cookie_header("abc", false);
        let s = set.to_str().unwrap();
        assert_eq!(s, "anycon_oauth=abc; Path=/auth/callback; Max-Age=600; HttpOnly; SameSite=Lax");
        assert_eq!(set_cookie_header("abc", true).to_str().unwrap(), format!("{s}; Secure"));
        let clear = clear_cookie_header(true);
        let c = clear.to_str().unwrap();
        assert!(c.starts_with("anycon_oauth=;") && c.contains("Max-Age=0") && c.ends_with("; Secure"), "{c}");
        assert!(!s.contains("Domain"));
    }
}
