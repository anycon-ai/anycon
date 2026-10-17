//! Web Push 消息加密：RFC 8291（`aes128gcm`，即 RFC 8188 的内容编码），单个记录，padding 用最小值（只有一个 `0x02` 分隔符）。
//!
//! ```text
//! ecdh_secret = ECDH(as_private, ua_public)
//! PRK_key     = HKDF-Extract(salt = auth_secret, IKM = ecdh_secret)
//! key_info    = "WebPush: info" 0x00 ua_public as_public
//! IKM         = HKDF-Expand(PRK_key, key_info, 32)
//! PRK         = HKDF-Extract(salt = salt, IKM = IKM)
//! CEK         = HKDF-Expand(PRK, "Content-Encoding: aes128gcm" 0x00, 16)
//! NONCE       = HKDF-Expand(PRK, "Content-Encoding: nonce" 0x00, 12)
//! body        = salt(16) rs(4, be) idlen(1) as_public(65) AES-128-GCM(CEK, NONCE, plaintext 0x02)
//! ```
//!
//! 测试里用 RFC 8291 附录 A 的向量逐字节核对。[`decrypt`] 是接收方（浏览器）那一半，hub 自己不用，
//! 留给测试和排查：假推送服务收到请求后用它解出载荷，确认发出去的东西真的是对的。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes128Gcm;
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::{PublicKey, SecretKey};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;

/// 记录大小。RFC 8291 § 4 推荐 4096（推送服务对整个请求体的上限也是 4096 字节）。
pub const RECORD_SIZE: u32 = 4096;
/// 请求体 = 86 字节头（16 salt + 4 rs + 1 idlen + 65 keyid）+ 明文 + 1 分隔符 + 16 tag ≤ 4096。
pub const MAX_PLAINTEXT: usize = 4096 - 86 - 1 - 16;
/// 头部长度（keyid 是 65 字节的未压缩公钥）。
const HEADER_LEN: usize = 16 + 4 + 1 + 65;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebPushError {
    #[error("the subscription's p256dh is not a valid uncompressed P-256 public key")]
    BadPublicKey,
    #[error("the subscription's auth secret must be 16 bytes")]
    BadAuthSecret,
    #[error("the payload is larger than a push message can carry ({MAX_PLAINTEXT} bytes)")]
    TooLarge,
    #[error("encryption failed")]
    Crypto,
    #[error("the encrypted body is malformed")]
    Malformed,
}

/// 浏览器给的公钥必须是 65 字节未压缩点（`0x04 ‖ X ‖ Y`）且在曲线上。
pub fn parse_ua_public(bytes: &[u8]) -> Result<PublicKey, WebPushError> {
    if bytes.len() != 65 || bytes[0] != 0x04 {
        return Err(WebPushError::BadPublicKey);
    }
    PublicKey::from_sec1_bytes(bytes).map_err(|_| WebPushError::BadPublicKey)
}

/// 公钥的 65 字节未压缩编码。
pub fn uncompressed(pk: &PublicKey) -> [u8; 65] {
    use p256::elliptic_curve::sec1::ToSec1Point;
    let point = pk.to_sec1_point(false);
    let mut out = [0u8; 65];
    out.copy_from_slice(point.as_bytes());
    out
}

/// 新的随机 P-256 私钥（用 `OsRng` 取 32 字节，落在 `[1, n-1]` 之外就重抽，概率可以忽略）。
pub fn random_secret_key() -> SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        if let Ok(k) = SecretKey::from_slice(&bytes) {
            return k;
        }
    }
}

struct Keys {
    cek: [u8; 16],
    nonce: [u8; 12],
}

/// 两边共用的密钥派生。`ecdh` 是 ECDH 共享密钥的 x 坐标。
fn derive(
    ecdh: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8; 65],
    as_public: &[u8; 65],
    salt: &[u8; 16],
) -> Result<Keys, WebPushError> {
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), ecdh).expand(&key_info, &mut ikm).map_err(|_| WebPushError::Crypto)?;

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| WebPushError::Crypto)?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| WebPushError::Crypto)?;
    Ok(Keys { cek, nonce })
}

fn cipher_and_nonce(keys: &Keys) -> Result<(Aes128Gcm, aes_gcm::Nonce<aes_gcm::aead::consts::U12>), WebPushError> {
    let cipher = Aes128Gcm::new_from_slice(&keys.cek).map_err(|_| WebPushError::Crypto)?;
    let nonce =
        aes_gcm::Nonce::<aes_gcm::aead::consts::U12>::try_from(&keys.nonce[..]).map_err(|_| WebPushError::Crypto)?;
    Ok((cipher, nonce))
}

/// 加密一条推送载荷，返回可以直接做请求体的 `aes128gcm` 内容（头 + 密文）。
///
/// `as_secret` 和 `salt` 由调用方给：生产里每条消息都用新的随机值（[`encrypt_fresh`]），测试里用 RFC 的固定值。
pub fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, WebPushError> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(WebPushError::TooLarge);
    }
    if auth_secret.len() != 16 {
        return Err(WebPushError::BadAuthSecret);
    }
    let ua_pk = parse_ua_public(ua_public)?;
    let ua_bytes = uncompressed(&ua_pk);
    let as_public = uncompressed(&as_secret.public_key());

    let shared = diffie_hellman(as_secret.to_nonzero_scalar(), ua_pk.as_affine());
    let keys = derive(shared.raw_secret_bytes().as_slice(), auth_secret, &ua_bytes, &as_public, salt)?;
    let (cipher, nonce) = cipher_and_nonce(&keys)?;

    // 最后一个（也是唯一一个）记录：明文后面跟 0x02 分隔符，不加 padding。
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ciphertext = cipher.encrypt(&nonce, record.as_slice()).map_err(|_| WebPushError::Crypto)?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(65);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 每条消息用新的临时密钥和随机 salt 加密。
pub fn encrypt_fresh(plaintext: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>, WebPushError> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    encrypt(plaintext, ua_public, auth_secret, &random_secret_key(), &salt)
}

/// 接收方（浏览器）那一半：用订阅的私钥和 auth 密钥解开 [`encrypt`] 的输出，返回明文。
/// hub 自己不解密，这个函数留给测试和排查用。只支持单记录消息。
pub fn decrypt(body: &[u8], ua_secret: &SecretKey, auth_secret: &[u8]) -> Result<Vec<u8>, WebPushError> {
    if auth_secret.len() != 16 {
        return Err(WebPushError::BadAuthSecret);
    }
    if body.len() < 21 {
        return Err(WebPushError::Malformed);
    }
    let salt: [u8; 16] = body[..16].try_into().map_err(|_| WebPushError::Malformed)?;
    let rs = u32::from_be_bytes(body[16..20].try_into().map_err(|_| WebPushError::Malformed)?) as usize;
    let idlen = body[20] as usize;
    if idlen != 65 || body.len() < 21 + idlen + 17 {
        return Err(WebPushError::Malformed);
    }
    let as_public_bytes: [u8; 65] = body[21..21 + idlen].try_into().map_err(|_| WebPushError::Malformed)?;
    let ciphertext = &body[21 + idlen..];
    if rs < 18 || ciphertext.len() > rs {
        // 单记录：密文（含 tag）不能超过记录大小
        return Err(WebPushError::Malformed);
    }
    let as_pk = parse_ua_public(&as_public_bytes)?;
    let ua_public = uncompressed(&ua_secret.public_key());

    let shared = diffie_hellman(ua_secret.to_nonzero_scalar(), as_pk.as_affine());
    let keys = derive(shared.raw_secret_bytes().as_slice(), auth_secret, &ua_public, &as_public_bytes, &salt)?;
    let (cipher, nonce) = cipher_and_nonce(&keys)?;
    let mut record = cipher.decrypt(&nonce, ciphertext).map_err(|_| WebPushError::Crypto)?;

    // 去掉 padding：从尾部跳过 0x00，遇到的必须是最后一个记录的分隔符 0x02。
    while record.last() == Some(&0) {
        record.pop();
    }
    if record.pop() != Some(0x02) {
        return Err(WebPushError::Malformed);
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    fn b64(s: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(s).expect("valid base64url")
    }

    // RFC 8291 附录 A（"Intermediate Values for Encryption"）和第 5 节的完整示例。
    const PLAINTEXT: &str = "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const AS_PUBLIC: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const ECDH_SECRET: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const CEK: &str = "oIhVW04MRdy2XN9CiKLxTg";
    const NONCE: &str = "4h_95klXJ5E_qnoN";
    const ENCRYPTED_BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    #[test]
    fn rfc8291_appendix_a_vector_matches_byte_for_byte() {
        let as_secret = SecretKey::from_slice(&b64(AS_PRIVATE)).unwrap();
        // 发送方公钥是私钥推出来的那个
        assert_eq!(uncompressed(&as_secret.public_key()).to_vec(), b64(AS_PUBLIC));
        let salt: [u8; 16] = b64(SALT).try_into().unwrap();

        // 中间值：ECDH 共享密钥、CEK、NONCE
        let ua_pk = parse_ua_public(&b64(UA_PUBLIC)).unwrap();
        let shared = diffie_hellman(as_secret.to_nonzero_scalar(), ua_pk.as_affine());
        assert_eq!(shared.raw_secret_bytes().as_slice(), b64(ECDH_SECRET).as_slice());
        let ua_bytes = uncompressed(&ua_pk);
        let as_public: [u8; 65] = b64(AS_PUBLIC).try_into().unwrap();
        let keys =
            derive(shared.raw_secret_bytes().as_slice(), &b64(AUTH_SECRET), &ua_bytes, &as_public, &salt).unwrap();
        assert_eq!(keys.cek.to_vec(), b64(CEK));
        assert_eq!(keys.nonce.to_vec(), b64(NONCE));

        // 整个请求体
        let body = encrypt(&b64(PLAINTEXT), &b64(UA_PUBLIC), &b64(AUTH_SECRET), &as_secret, &salt).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(&body), ENCRYPTED_BODY);
        assert_eq!(body, b64(ENCRYPTED_BODY), "byte for byte");
        // 头部：salt ‖ rs=4096 ‖ idlen=65 ‖ keyid
        assert_eq!(&body[..16], b64(SALT).as_slice());
        assert_eq!(&body[16..20], &[0, 0, 0x10, 0]);
        assert_eq!(body[20], 65);
        assert_eq!(&body[21..86], b64(AS_PUBLIC).as_slice());
        assert_eq!(body.len(), 86 + b64(PLAINTEXT).len() + 1 + 16);
        assert_eq!(b64(PLAINTEXT), b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn rfc8291_example_decrypts_with_the_user_agent_private_key() {
        // 附录 A 的接收方私钥：用它能解开 RFC 里的那条完整消息。
        let ua_private = SecretKey::from_slice(&b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94")).unwrap();
        assert_eq!(uncompressed(&ua_private.public_key()).to_vec(), b64(UA_PUBLIC));
        let plain = decrypt(&b64(ENCRYPTED_BODY), &ua_private, &b64(AUTH_SECRET)).unwrap();
        assert_eq!(plain, b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn fresh_encryptions_roundtrip_and_never_repeat() {
        let ua = random_secret_key();
        let ua_public = uncompressed(&ua.public_key());
        let auth = [7u8; 16];
        let payload = br#"{"category":"permission_requested","node":"abc","session":"s1"}"#;
        let a = encrypt_fresh(payload, &ua_public, &auth).unwrap();
        let b = encrypt_fresh(payload, &ua_public, &auth).unwrap();
        assert_ne!(a, b, "a fresh ephemeral key and salt every time");
        assert_eq!(decrypt(&a, &ua, &auth).unwrap(), payload);
        assert_eq!(decrypt(&b, &ua, &auth).unwrap(), payload);
        // 别人的密钥 / 错的 auth 解不开
        assert!(decrypt(&a, &random_secret_key(), &auth).is_err());
        assert!(decrypt(&a, &ua, &[8u8; 16]).is_err());
        // 篡改密文 / 截断都失败
        let mut bad = a.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decrypt(&bad, &ua, &auth).is_err());
        assert!(decrypt(&a[..60], &ua, &auth).is_err());
        assert_eq!(decrypt(&[], &ua, &auth), Err(WebPushError::Malformed));
    }

    #[test]
    fn empty_and_maximum_payloads_work_and_oversize_is_refused() {
        let ua = random_secret_key();
        let ua_public = uncompressed(&ua.public_key());
        let auth = [1u8; 16];
        assert_eq!(decrypt(&encrypt_fresh(b"", &ua_public, &auth).unwrap(), &ua, &auth).unwrap(), b"");
        let max = vec![b'x'; MAX_PLAINTEXT];
        let body = encrypt_fresh(&max, &ua_public, &auth).unwrap();
        assert_eq!(body.len(), 4096, "the largest message fills exactly one 4096-byte request body");
        assert_eq!(decrypt(&body, &ua, &auth).unwrap(), max);
        assert_eq!(encrypt_fresh(&vec![b'x'; MAX_PLAINTEXT + 1], &ua_public, &auth), Err(WebPushError::TooLarge));
    }

    #[test]
    fn bad_subscription_keys_are_rejected() {
        let ua = random_secret_key();
        let good = uncompressed(&ua.public_key());
        let auth = [1u8; 16];
        // 压缩点、长度不对、不在曲线上、全零
        let mut off_curve = good;
        off_curve[64] ^= 0xff;
        for bad in [&good[..64], &good[1..], &[0u8; 65][..], &off_curve[..], &[][..]] {
            assert_eq!(encrypt_fresh(b"x", bad, &auth), Err(WebPushError::BadPublicKey), "{} bytes", bad.len());
        }
        use p256::elliptic_curve::sec1::ToSec1Point;
        let compressed = ua.public_key().to_sec1_point(true);
        assert_eq!(encrypt_fresh(b"x", compressed.as_bytes(), &auth), Err(WebPushError::BadPublicKey));
        // auth 密钥必须 16 字节
        for len in [0usize, 15, 17, 32] {
            assert_eq!(encrypt_fresh(b"x", &good, &vec![0u8; len]), Err(WebPushError::BadAuthSecret));
        }
    }
}
