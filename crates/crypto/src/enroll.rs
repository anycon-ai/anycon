//! 账号内接入（ADR-36，docs/protocol.md § 4.4）用到的密码学：CPace 和几个派生函数。
//!
//! 三种接入方法最后都是一次 [`crate::Handshake::pairing`]（`Noise_XXpsk3`），只差 PSK 从哪来：
//! - `scan`：新设备生成 32 字节随机密钥放进二维码，已配对设备扫码后交给 daemon；PSK = [`derive_enroll_scan_psk`]
//! - `code` / `password`：低熵秘密（6 位码、人记得住的口令），双方先跑一轮 [`Cpace`]，输出当 PSK。
//!   口令先经 [`derive_enroll_password_prs`]（Argon2id + 节点盐）再进 CPace；码直接用规范化后的 6 位数字
//!
//! **CPace**：按 draft-irtf-cfrg-cpace 的 ristretto255 + SHA-512 构造实现（generator_string、lv_cat、
//! 发起方在前的 transcript、ISK 的 `_ISK` 域分隔），生成元、份额、K、ISK 都对上了 draft-irtf-cfrg-cpace-14
//! 附录 B.3 的测试向量（`cpace_matches_draft_test_vectors`）。daemon、web（wasm）、python 用的是这同一份代码。
//! 我们不用 AD（身份绑在 CI 里），CI 是我们自己的 `lv_cat` 拼法。
//! 性质：主动参与的一方（包括恶意 hub）每次在线交互至多验证一个口令猜测，抓到的记录不能离线穷举。

use curve25519_dalek::{
    ristretto::{CompressedRistretto, RistrettoPoint},
    scalar::Scalar,
    traits::Identity,
};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha512};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroize;

use crate::{Error, Result, Role, PAIR_KDF_M_COST_KIB, PAIR_KDF_P_COST, PAIR_KDF_T_COST};

/// CPace 信道标识的前缀：CI = lv_cat(`ENROLL_CI_LABEL`, node_id, method)。
pub const ENROLL_CI_LABEL: &[u8] = b"anycon-enroll-v1";
/// 扫码 PSK 的 HKDF info 前缀：info = `ENROLL_SCAN_INFO` ‖ node_id。
pub const ENROLL_SCAN_INFO: &[u8] = b"anycon-enroll-scan-v1";
/// `EnrollHello.nonce` / `EnrollReply.nonce` 的字节数。
pub const ENROLL_NONCE_LEN: usize = 16;
/// 节点口令盐的字节数。
pub const ENROLL_SALT_LEN: usize = 16;
/// 扫码密钥的字节数。
pub const ENROLL_SECRET_LEN: usize = 32;
/// 接入码位数。
pub const ENROLL_CODE_LEN: usize = 6;
/// 节点口令最少几个字符（NFC 规范化之后按 Unicode 标量计）。
pub const ENROLL_PASSWORD_MIN_CHARS: usize = 8;

/// CPace 的域分隔串（ristretto255 套件）。
const CPACE_DSI: &[u8] = b"CPaceRistretto255";
/// SHA-512 的输入块大小，generator_string 按它补零。
const SHA512_BLOCK: usize = 128;
/// BLAKE2s 的块大小（HMAC 用）。
const BLAKE2S_BLOCK: usize = 64;

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

/// `EnrollHello` / `EnrollReply` 的 16 字节随机数。
pub fn generate_enroll_nonce() -> [u8; ENROLL_NONCE_LEN] {
    random()
}

/// 节点口令的 16 字节随机盐（设置口令时生成一次，存在 daemon）。
pub fn generate_enroll_salt() -> [u8; ENROLL_SALT_LEN] {
    random()
}

/// 扫码接入的 32 字节随机密钥（新设备生成，放进二维码）。
pub fn generate_enroll_secret() -> [u8; ENROLL_SECRET_LEN] {
    random()
}

/// 6 位数字接入码，均匀分布（拒绝采样，不取模偏差）。
pub fn generate_enroll_code() -> String {
    const SPACE: u32 = 1_000_000;
    // 2^32 以内最大的 SPACE 倍数，超过它的样本丢掉重抽
    let limit = u32::MAX - (u32::MAX % SPACE);
    loop {
        let n = OsRng.next_u32();
        if n < limit {
            return format!("{:06}", n % SPACE);
        }
    }
}

/// 接入码规范化：去掉空白和 `-`，必须恰好是 6 个 ASCII 数字，否则 `InvalidCode`。
pub fn normalize_enroll_code(code: &str) -> Result<String> {
    let normalized: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if normalized.len() == ENROLL_CODE_LEN && normalized.bytes().all(|b| b.is_ascii_digit()) {
        Ok(normalized)
    } else {
        Err(Error::InvalidCode)
    }
}

/// 节点口令规范化：Unicode NFC。设置口令的一端和接入的一端都要先过这一步，否则同一个口令在不同输入法下派生不同的值。
pub fn normalize_enroll_password(password: &str) -> String {
    password.nfc().collect()
}

/// 设置口令时的校验：NFC 之后至少 [`ENROLL_PASSWORD_MIN_CHARS`] 个字符。不做复杂度规则（在线猜测有硬上限）。
pub fn validate_enroll_password(password: &str) -> Result<()> {
    let mut normalized = normalize_enroll_password(password);
    let ok = normalized.chars().count() >= ENROLL_PASSWORD_MIN_CHARS;
    normalized.zeroize();
    if ok {
        Ok(())
    } else {
        Err(Error::WeakPassword)
    }
}

/// 节点口令的派生值：`Argon2id(NFC(口令), salt, m = 64 MiB, t = 3, p = 1, out = 32)`。
/// daemon 设置口令时算一次存下来，接入时新设备用 `EnrollReply.salt` 再算一次；两边的结果就是 CPace 的口令输入。
pub fn derive_enroll_password_prs(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    if salt.len() != ENROLL_SALT_LEN {
        return Err(Error::Kdf(format!("salt must be {ENROLL_SALT_LEN} bytes")));
    }
    let mut normalized = normalize_enroll_password(password);
    let params = argon2::Params::new(PAIR_KDF_M_COST_KIB, PAIR_KDF_T_COST, PAIR_KDF_P_COST, Some(32))
        .map_err(|e| Error::Kdf(e.to_string()))?;
    let kdf = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    let r = kdf.hash_password_into(normalized.as_bytes(), salt, &mut out);
    normalized.zeroize();
    if let Err(e) = r {
        out.zeroize();
        return Err(Error::Kdf(e.to_string()));
    }
    Ok(out)
}

fn hmac_blake2s(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use blake2::{Blake2s256, Digest as _};
    let mut block = [0u8; BLAKE2S_BLOCK];
    if key.len() > BLAKE2S_BLOCK {
        block[..32].copy_from_slice(&Blake2s256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = block.map(|b| b ^ 0x36);
    let mut opad = block.map(|b| b ^ 0x5c);
    let mut inner = Blake2s256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let inner = inner.finalize();
    let mut outer = Blake2s256::new();
    outer.update(opad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    block.zeroize();
    ipad.zeroize();
    opad.zeroize();
    out
}

/// 扫码接入的 PSK：`HKDF-BLAKE2s(ikm = secret, salt = 空, info = ENROLL_SCAN_INFO ‖ node_id)` 的前 32 字节（RFC 5869）。
pub fn derive_enroll_scan_psk(secret: &[u8; ENROLL_SECRET_LEN], node_id: &str) -> [u8; 32] {
    let mut prk = hmac_blake2s(&[0u8; 32], &[secret]);
    let okm = hmac_blake2s(&prk, &[ENROLL_SCAN_INFO, node_id.as_bytes(), &[1u8]]);
    prk.zeroize();
    okm
}

/// LEB128 长度前缀 + 数据（CPace 的 `prepend_len`）。
fn prepend_len(out: &mut Vec<u8>, data: &[u8]) {
    let mut len = data.len();
    loop {
        let byte = (len & 0x7f) as u8;
        len >>= 7;
        if len == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(data);
}

fn lv_cat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        prepend_len(&mut out, p);
    }
    out
}

/// CPace 信道标识：lv_cat(`ENROLL_CI_LABEL`, node_id, method)。`method` 是 `"code"` 或 `"password"`。
pub fn enroll_channel_id(node_id: &str, method: &str) -> Vec<u8> {
    lv_cat(&[ENROLL_CI_LABEL, node_id.as_bytes(), method.as_bytes()])
}

/// CPace 会话标识：发起方（新设备）的 nonce ‖ 响应方（daemon）的 nonce。不用 hub 分配的 `enroll_id`。
pub fn enroll_sid(initiator_nonce: &[u8], responder_nonce: &[u8]) -> Vec<u8> {
    [initiator_nonce, responder_nonce].concat()
}

/// CPace 的生成元：`from_uniform_bytes(SHA-512(lv_cat(DSI, PRS, zpad, CI, sid)))`。
fn generator(prs: &[u8], ci: &[u8], sid: &[u8]) -> RistrettoPoint {
    let mut dsi_len = Vec::new();
    prepend_len(&mut dsi_len, CPACE_DSI);
    let mut prs_len = Vec::new();
    prepend_len(&mut prs_len, prs);
    let zpad = SHA512_BLOCK.saturating_sub(1 + dsi_len.len() + prs_len.len());
    let zeros = vec![0u8; zpad];
    let mut gen_str = lv_cat(&[CPACE_DSI, prs, &zeros, ci, sid]);
    let mut hash = [0u8; 64];
    hash.copy_from_slice(&Sha512::digest(&gen_str));
    let g = RistrettoPoint::from_uniform_bytes(&hash);
    gen_str.zeroize();
    prs_len.zeroize();
    hash.zeroize();
    g
}

/// `ISK = SHA-512(lv_cat(DSI ‖ "_ISK", sid, K) ‖ transcript_ir(Ya, ADa, Yb, ADb))`，
/// `transcript_ir = lv_cat(Ya, ADa) ‖ lv_cat(Yb, ADb)`（发起方在前）。
fn isk_ir(sid: &[u8], k: &[u8], ya: &[u8], ad_a: &[u8], yb: &[u8], ad_b: &[u8]) -> [u8; 64] {
    let dsi_isk = [CPACE_DSI, b"_ISK"].concat();
    let mut h = Sha512::new();
    h.update(lv_cat(&[&dsi_isk, sid, k]));
    h.update(lv_cat(&[ya, ad_a]));
    h.update(lv_cat(&[yb, ad_b]));
    let mut out = [0u8; 64];
    out.copy_from_slice(&h.finalize());
    out
}

/// CPace 的一方。`new` 算出自己的份额，`finish` 拿对方的份额得到 32 字节 PSK。私有标量在 Drop 时清零。
pub struct Cpace {
    y: Scalar,
    share: [u8; 32],
    sid: Vec<u8>,
}

impl Drop for Cpace {
    fn drop(&mut self) {
        self.y.zeroize();
    }
}

impl std::fmt::Debug for Cpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cpace").finish_non_exhaustive()
    }
}

impl Cpace {
    /// `prs` 是口令输入（口令的 [`derive_enroll_password_prs`]，或规范化后的 6 位码的 ASCII），
    /// `ci` 是 [`enroll_channel_id`]，`sid` 是 [`enroll_sid`]。
    pub fn new(prs: &[u8], ci: &[u8], sid: &[u8]) -> Result<Cpace> {
        if sid.is_empty() {
            return Err(Error::InvalidKey);
        }
        // 生成元是口令的函数：留着它就能离线比对猜测，用完清零
        let mut g = generator(prs, ci, sid);
        let mut wide = random::<64>();
        let y = Scalar::from_bytes_mod_order_wide(&wide);
        wide.zeroize();
        let share = (g * y).compress().to_bytes();
        g.zeroize();
        Ok(Cpace { y, share, sid: sid.to_vec() })
    }

    /// 自己的份额（32 字节），发给对方。
    pub fn share(&self) -> [u8; 32] {
        self.share
    }

    /// 用对方的份额算出 PSK（ISK 的前 32 字节）。新设备是 `Initiator`，daemon 是 `Responder`；
    /// transcript 里发起方的份额在前。对方份额不是合法的 ristretto255 点，或乘出单位元，返回 `InvalidKey`。
    pub fn finish(self, role: Role, peer_share: &[u8]) -> Result<[u8; 32]> {
        let peer: [u8; 32] = peer_share.try_into().map_err(|_| Error::InvalidKey)?;
        let point = CompressedRistretto(peer).decompress().ok_or(Error::InvalidKey)?;
        let k = point * self.y;
        if k == RistrettoPoint::identity() {
            return Err(Error::InvalidKey);
        }
        let mut k_bytes = k.compress().to_bytes();
        let (first, second) = match role {
            Role::Initiator => (self.share, peer),
            Role::Responder => (peer, self.share),
        };
        // 我们不用 AD（身份已经绑在 CI 里）
        let mut isk = isk_ir(&self.sid, &k_bytes, &first, b"", &second, b"");
        let mut psk = [0u8; 32];
        psk.copy_from_slice(&isk[..32]);
        isk.zeroize();
        k_bytes.zeroize();
        Ok(psk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(prs_a: &[u8], prs_b: &[u8], ci_a: &[u8], ci_b: &[u8]) -> ([u8; 32], [u8; 32]) {
        let (na, nb) = (generate_enroll_nonce(), generate_enroll_nonce());
        let sid = enroll_sid(&na, &nb);
        let a = Cpace::new(prs_a, ci_a, &sid).unwrap();
        let b = Cpace::new(prs_b, ci_b, &sid).unwrap();
        let (sa, sb) = (a.share(), b.share());
        (a.finish(Role::Initiator, &sb).unwrap(), b.finish(Role::Responder, &sa).unwrap())
    }

    #[test]
    fn cpace_same_secret_agrees() {
        let ci = enroll_channel_id("node1", "code");
        let (a, b) = run(b"123456", b"123456", &ci, &ci);
        assert_eq!(a, b);
    }

    #[test]
    fn cpace_wrong_secret_disagrees() {
        let ci = enroll_channel_id("node1", "code");
        let (a, b) = run(b"123456", b"123457", &ci, &ci);
        assert_ne!(a, b);
    }

    #[test]
    fn cpace_binds_channel_id() {
        let (a, b) =
            run(b"123456", b"123456", &enroll_channel_id("node1", "code"), &enroll_channel_id("node2", "code"));
        assert_ne!(a, b);
        let (a, b) =
            run(b"123456", b"123456", &enroll_channel_id("node1", "code"), &enroll_channel_id("node1", "password"));
        assert_ne!(a, b);
    }

    #[test]
    fn cpace_binds_sid_and_roles() {
        let ci = enroll_channel_id("n", "code");
        let a = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        let b = Cpace::new(b"123456", &ci, &[2u8; 32]).unwrap();
        let (sa, sb) = (a.share(), b.share());
        assert_ne!(a.finish(Role::Initiator, &sb).unwrap(), b.finish(Role::Responder, &sa).unwrap());

        // 两边都以为自己是发起方：transcript 顺序不同，结果不同
        let sid = [3u8; 32];
        let a = Cpace::new(b"123456", &ci, &sid).unwrap();
        let b = Cpace::new(b"123456", &ci, &sid).unwrap();
        let (sa, sb) = (a.share(), b.share());
        assert_ne!(a.finish(Role::Initiator, &sb).unwrap(), b.finish(Role::Initiator, &sa).unwrap());
    }

    #[test]
    fn cpace_rejects_bad_shares() {
        let ci = enroll_channel_id("n", "code");
        let a = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        // 单位元（全零的压缩编码）
        assert!(matches!(a.finish(Role::Initiator, &[0u8; 32]), Err(Error::InvalidKey)));
        let a = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        // 不是合法编码
        assert!(matches!(a.finish(Role::Initiator, &[0xffu8; 32]), Err(Error::InvalidKey)));
        let a = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        assert!(matches!(a.finish(Role::Initiator, &[1u8; 31]), Err(Error::InvalidKey)));
        assert!(Cpace::new(b"123456", &ci, &[]).is_err());
    }

    #[test]
    fn shares_are_fresh() {
        let ci = enroll_channel_id("n", "code");
        let a = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        let b = Cpace::new(b"123456", &ci, &[1u8; 32]).unwrap();
        assert_ne!(a.share(), b.share());
    }

    #[test]
    fn enroll_code_format() {
        for _ in 0..200 {
            let c = generate_enroll_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(normalize_enroll_code(" 123-456 ").unwrap(), "123456");
        assert!(normalize_enroll_code("12345").is_err());
        assert!(normalize_enroll_code("12345a").is_err());
        assert!(normalize_enroll_code("１２３４５６").is_err()); // 全角数字不算
    }

    #[test]
    fn password_rules() {
        assert!(validate_enroll_password("1234567").is_err());
        assert!(validate_enroll_password("12345678").is_ok());
        // 按字符而不是字节计：8 个汉字可以，7 个不行
        assert!(validate_enroll_password("一二三四五六七八").is_ok());
        assert!(validate_enroll_password("一二三四五六七").is_err());
        // NFC：e + 组合重音 和 预组合的 é 是同一个口令
        assert_eq!(normalize_enroll_password("cafe\u{301}"), normalize_enroll_password("caf\u{e9}"));
    }

    #[test]
    fn password_prs_is_deterministic_and_salted() {
        let salt = [7u8; ENROLL_SALT_LEN];
        let a = derive_enroll_password_prs("correct horse", &salt).unwrap();
        let b = derive_enroll_password_prs("correct horse", &salt).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, derive_enroll_password_prs("correct horse", &[8u8; ENROLL_SALT_LEN]).unwrap());
        assert_ne!(a, derive_enroll_password_prs("correct hors3", &salt).unwrap());
        assert_eq!(
            derive_enroll_password_prs("cafe\u{301}xxxx", &salt).unwrap(),
            derive_enroll_password_prs("caf\u{e9}xxxx", &salt).unwrap()
        );
        assert!(derive_enroll_password_prs("x", &[0u8; 8]).is_err());
    }

    #[test]
    fn scan_psk_binds_node() {
        let s = [9u8; ENROLL_SECRET_LEN];
        assert_eq!(derive_enroll_scan_psk(&s, "a"), derive_enroll_scan_psk(&s, "a"));
        assert_ne!(derive_enroll_scan_psk(&s, "a"), derive_enroll_scan_psk(&s, "b"));
        assert_ne!(derive_enroll_scan_psk(&s, "a"), derive_enroll_scan_psk(&[8u8; 32], "a"));
    }

    #[test]
    fn hmac_blake2s_matches_known_structure() {
        // RFC 2104 结构自检：长 key 先哈希，结果和用哈希值当 key 一样
        use blake2::{Blake2s256, Digest as _};
        let long = [0x42u8; 100];
        let hashed = Blake2s256::digest(long);
        assert_eq!(hmac_blake2s(&long, &[b"m"]), hmac_blake2s(&hashed, &[b"m"]));
        assert_eq!(hmac_blake2s(b"k", &[b"ab", b"c"]), hmac_blake2s(b"k", &[b"abc"]));
    }

    /// draft-irtf-cfrg-cpace-14 附录 B.3（CPACE-RISTR255-SHA512）的测试向量：生成元、两边份额、K、ISK_IR 逐项对上。
    #[test]
    fn cpace_matches_draft_test_vectors() {
        let h = |s: &str| crate::from_hex(&s.split_whitespace().collect::<String>()).unwrap();
        let prs = b"Password";
        let ci = h("6f630b425f726573706f6e6465720b415f696e69746961746f72");
        let sid = h("7e4b4791d6a8ef019b936c79fb7f2c57");
        let g = generator(prs, &ci, &sid);
        assert_eq!(
            g.compress().to_bytes().to_vec(),
            h("a6fc82c3b8968fbb2e06fee81ca858586dea50d248f0c7ca6a18b0902a30b36b")
        );
        let scalar = |hex: &str| Scalar::from_bytes_mod_order(h(hex).try_into().unwrap());
        let ya = scalar("da3d23700a9e5699258aef94dc060dfda5ebb61f02a5ea77fad53f4ff0976d08");
        let yb = scalar("d2316b454718c35362d83d69df6320f38578ed5984651435e2949762d900b80d");
        let big_ya = (g * ya).compress().to_bytes();
        let big_yb = (g * yb).compress().to_bytes();
        assert_eq!(big_ya.to_vec(), h("d40fb265a7abeaee7939d91a585fe59f7053f982c296ec413c624c669308f87a"));
        assert_eq!(big_yb.to_vec(), h("08bcf6e9777a9c313a3db6daa510f2d398403319c2341bd506a92e672eb7e307"));
        let k = (CompressedRistretto(big_yb).decompress().unwrap() * ya).compress().to_bytes();
        assert_eq!(k.to_vec(), h("e22b1ef7788f661478f3cddd4c600774fc0f41e6b711569190ff88fa0e607e09"));
        let isk = isk_ir(&sid, &k, &big_ya, b"ADa", &big_yb, b"ADb");
        assert_eq!(
            isk.to_vec(),
            h("4c5469a16b2364c4b944ebc1a79e51d1674ad47db26e8718154f59fa
               ebfaa52d8346f30aa58377117eb20d527f2cbc5c76381f7fd372e89d
               f8239f87f2e02ed1")
        );
    }

    #[test]
    fn lv_cat_is_unambiguous() {
        assert_ne!(lv_cat(&[b"ab", b"c"]), lv_cat(&[b"a", b"bc"]));
        let mut v = Vec::new();
        prepend_len(&mut v, &[0u8; 200]);
        assert_eq!(&v[..2], &[0xc8, 0x01]);
    }

    #[test]
    fn end_to_end_pairing_with_cpace_psk() {
        use crate::{Handshake, Keypair};
        let ci = enroll_channel_id("node", "code");
        let sid = enroll_sid(&[1u8; 16], &[2u8; 16]);
        let n = Cpace::new(b"424242", &ci, &sid).unwrap();
        let d = Cpace::new(b"424242", &ci, &sid).unwrap();
        let (sn, sd) = (n.share(), d.share());
        let psk_n = n.finish(Role::Initiator, &sd).unwrap();
        let psk_d = d.finish(Role::Responder, &sn).unwrap();
        let (kn, kd) = (Keypair::generate(), Keypair::generate());
        let mut hn = Handshake::pairing(Role::Initiator, &kn, &psk_n).unwrap();
        let mut hd = Handshake::pairing(Role::Responder, &kd, &psk_d).unwrap();
        let m1 = hn.write(b"").unwrap();
        hd.read(&m1).unwrap();
        let m2 = hd.write(b"").unwrap();
        hn.read(&m2).unwrap();
        let m3 = hn.write(b"client-info").unwrap();
        assert_eq!(hd.read(&m3).unwrap(), b"client-info");
        assert!(hn.is_finished() && hd.is_finished());

        // 码不对：daemon 读第 3 条失败
        let n = Cpace::new(b"424242", &ci, &sid).unwrap();
        let d = Cpace::new(b"424243", &ci, &sid).unwrap();
        let (sn, sd) = (n.share(), d.share());
        let psk_n = n.finish(Role::Initiator, &sd).unwrap();
        let psk_d = d.finish(Role::Responder, &sn).unwrap();
        let mut hn = Handshake::pairing(Role::Initiator, &kn, &psk_n).unwrap();
        let mut hd = Handshake::pairing(Role::Responder, &kd, &psk_d).unwrap();
        let m1 = hn.write(b"").unwrap();
        hd.read(&m1).unwrap();
        let m2 = hd.write(b"").unwrap();
        hn.read(&m2).unwrap();
        let m3 = hn.write(b"").unwrap();
        assert!(hd.read(&m3).is_err());
    }
}
