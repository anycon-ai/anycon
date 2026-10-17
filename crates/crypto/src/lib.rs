//! Noise 握手与传输加密，实现用 `snow`。公开 API 是各端共用的约定（daemon、cli、web wasm、python 都按这个写），
//! 不要随意改签名。
//!
//! - 配对：`Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`，PSK 由短码派生。**joiner（新设备）是 initiator，host（daemon）是 responder。**
//! - 重连：`Noise_IK_25519_ChaChaPoly_BLAKE2s`，**客户端是 initiator**（已知 daemon 静态公钥），daemon 是 responder。
//! - 传输：Noise 单条消息上限 65535 字节。[`Transport::encrypt`] 对更长的明文自动分片，
//!   输出格式为若干 `[u16 be 长度][密文]` 片段拼接；[`Transport::decrypt`] 还原整条明文。
//!   每条 Noise 消息的 nonce 由 snow 管理，一个 Transport 只能单线程顺序使用。
//!
//! 实现要点（消费方需要知道的）：
//! - 握手谁先写：两种模式都是 **initiator 先写**。XXpsk3：I→R、R→I、I→R 共三条，I 在写完第三条后完成，R 在读完第三条后完成；
//!   IK：I→R、R→I 共两条。
//! - 对方静态公钥何时可知：XX initiator 读完第二条后、XX responder 读完第三条后、IK responder 读完第一条后、IK initiator 一开始。
//! - 握手出错（`read` 返回 `Err`）后这个 [`Handshake`] 作废，后续调用都会报错，重新建一个。
//! - 传输层 nonce 是隐式计数器：丢包、重放、乱序都会 `Decrypt` 失败。`decrypt` 失败不会推进计数器，
//!   所以收到一条被篡改的消息后，通道仍可继续收下一条合法消息（对应的发送方没有跳号时）。
//! - RNG 用 `rand::rngs::OsRng`（native 走系统，wasm 走 `crypto.getRandomValues`）；snow 自带的 getrandom 0.3 后端被关掉了，
//!   这样 wasm 构建不需要 `--cfg getrandom_backend` 之类的 RUSTFLAGS。

use blake2::{Blake2s256, Digest};
use rand::{rngs::OsRng, RngCore};
use snow::{
    params::{DHChoice, NoiseParams},
    resolvers::{CryptoResolver, DefaultResolver},
    types::{Cipher, Dh, Hash, Random},
    Builder, HandshakeState, TransportState,
};
use zeroize::Zeroize;

pub const PAIR_PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
pub const RECONNECT_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// 配对 KDF 的盐。只能是全局常量：两端唯一的共享秘密就是短码，没有地方传随机盐。
/// v1 是 HKDF-BLAKE2s，拿到 `match_id` 的 hub 能离线穷举 8 位短码；v2 换成 Argon2id（ADR-29）。
pub const PAIR_KDF_SALT: &[u8] = b"anycon-pair-v2";
/// Argon2id 参数：64 MiB、3 轮、单线程，输出 64 字节（前 8 字节 → match_id，后 32 字节 → PSK）。
/// 本机 release 约 0.1–0.3 s，wasm 约 0.5–1 s；每次配对各端只算一次（[`derive_pair_secrets`]）。
pub const PAIR_KDF_M_COST_KIB: u32 = 64 * 1024;
pub const PAIR_KDF_T_COST: u32 = 3;
pub const PAIR_KDF_P_COST: u32 = 1;
const PAIR_KDF_OUT: usize = 64;
/// 单片明文上限（65535 - 16 字节 tag）。
pub const MAX_CHUNK: usize = 65519;

/// 短码字母表（去掉易混淆的 0/O、1/I/L）。与 `anycon_protocol::PAIR_CODE_ALPHABET` 一致，测试里有断言。
/// 这里不依赖 protocol crate，避免把 serde / schemars 拖进 wasm。
const PAIR_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
/// 短码长度（不含 `-`）。
const PAIR_CODE_LEN: usize = 8;
/// ChaChaPoly 的认证 tag 长度。
const TAG_LEN: usize = 16;
/// Noise 单条消息上限。
const MAX_MSG: usize = 65535;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("noise: {0}")]
    Noise(String),
    #[error("handshake not finished")]
    NotFinished,
    #[error("handshake already finished")]
    Finished,
    #[error("not your turn to write")]
    NotMyTurn,
    #[error("decrypt failed")]
    Decrypt,
    #[error("malformed frame")]
    Malformed,
    #[error("invalid key")]
    InvalidKey,
    #[error("invalid pair code")]
    InvalidCode,
    #[error("kdf: {0}")]
    Kdf(String),
    /// 节点口令太短（ADR-36：NFC 之后至少 8 个字符）。
    #[error("enroll password too short")]
    WeakPassword,
}

pub type Result<T> = std::result::Result<T, Error>;

/// X25519 静态密钥对。`secret` 在 Drop 时清零。
#[derive(Clone)]
pub struct Keypair {
    pub public: [u8; 32],
    pub secret: [u8; 32],
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair").field("public", &hex(&self.public)).finish_non_exhaustive()
    }
}

impl Drop for Keypair {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl Keypair {
    /// 用系统 CSPRNG 生成新密钥对。系统随机数不可用时 panic（与 `OsRng::fill_bytes` 一致，此时没有安全的退路）。
    pub fn generate() -> Keypair {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        let kp = Keypair::from_secret(secret);
        secret.zeroize();
        kp
    }

    /// 由已有私钥还原密钥对（公钥由 X25519 基点乘法得出，和握手里 snow 用的是同一份实现）。
    pub fn from_secret(secret: [u8; 32]) -> Keypair {
        let mut dh: Box<dyn Dh> = Resolver.resolve_dh(&DHChoice::Curve25519).expect("curve25519 compiled in");
        dh.set(&secret);
        let mut public = [0u8; 32];
        public.copy_from_slice(dh.pubkey());
        Keypair { public, secret }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

/// 短码规范化：大写、去掉 `-` 和空白。不校验字母表。
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect()
}

/// 生成 8 位短码，字母表见 `anycon_protocol::PAIR_CODE_ALPHABET`，返回 `XXXX-XXXX` 形式。
pub fn generate_pair_code() -> String {
    let raw = generate_code_raw(PAIR_CODE_LEN);
    format!("{}-{}", &raw[..4], &raw[4..])
}

/// 从 CSPRNG 取 `len` 个字母表字符。31 个字符，用拒绝采样避免取模偏差：只接受 < 248（= 31 * 8）的字节。
fn generate_code_raw(len: usize) -> String {
    let n = PAIR_CODE_ALPHABET.len();
    let limit = 256 - 256 % n;
    let mut code = String::with_capacity(len);
    let mut buf = [0u8; 32];
    while code.len() < len {
        OsRng.fill_bytes(&mut buf);
        for &b in &buf {
            if (b as usize) < limit {
                code.push(PAIR_CODE_ALPHABET[b as usize % n] as char);
                if code.len() == len {
                    break;
                }
            }
        }
    }
    code
}

/// 一次配对 KDF 的两个产物：`match_id`（给 hub 撮合，16 个 hex 字符）和 `psk`（Noise XXpsk3 的 PSK）。
/// `psk` 在 Drop 时清零。
pub struct PairSecrets {
    pub match_id: String,
    pub psk: [u8; 32],
}

impl Drop for PairSecrets {
    fn drop(&mut self) {
        self.psk.zeroize();
    }
}

impl std::fmt::Debug for PairSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairSecrets").field("match_id", &self.match_id).finish_non_exhaustive()
    }
}

/// `Argon2id(password = normalize_code(code), salt = PAIR_KDF_SALT, m = 64 MiB, t = 3, p = 1)` → 64 字节。
fn pair_kdf(code: &str) -> Result<[u8; PAIR_KDF_OUT]> {
    let mut normalized = validated_code(code)?;
    let params = argon2::Params::new(PAIR_KDF_M_COST_KIB, PAIR_KDF_T_COST, PAIR_KDF_P_COST, Some(PAIR_KDF_OUT))
        .map_err(|e| Error::Kdf(e.to_string()))?;
    let kdf = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut okm = [0u8; PAIR_KDF_OUT];
    let r = kdf.hash_password_into(normalized.as_bytes(), PAIR_KDF_SALT, &mut okm);
    normalized.zeroize();
    if let Err(e) = r {
        okm.zeroize();
        return Err(Error::Kdf(e.to_string()));
    }
    Ok(okm)
}

/// 从短码一次算出 `match_id` 和 PSK。配对双方都应该只调这一次（Argon2id 不便宜），
/// 而不是分别调 [`derive_match_id`] 和 [`derive_psk`]。
///
/// 规范化后必须恰好是 8 或 16 个字母表内的字符，否则 `Err(InvalidCode)`。
pub fn derive_pair_secrets(code: &str) -> Result<PairSecrets> {
    let mut okm = pair_kdf(code)?;
    let match_id = hex(&okm[..8]);
    let mut psk = [0u8; 32];
    psk.copy_from_slice(&okm[32..64]);
    okm.zeroize();
    Ok(PairSecrets { match_id, psk })
}

/// Noise XXpsk3 的 PSK，即 `derive_pair_secrets(code).psk`。只要 PSK 时用；两个都要就用 [`derive_pair_secrets`]，省一次 KDF。
pub fn derive_psk(code: &str) -> Result<[u8; 32]> {
    Ok(derive_pair_secrets(code)?.psk)
}

/// 短码的长形式：16 位（约 79 bit），用于链接 / 二维码，或 `anycon pair --long`。
pub const PAIR_CODE_LEN_LONG: usize = 16;

/// 规范化并校验：8 位或 16 位，只含字母表字符。返回规范化后的串（调用方负责 zeroize）。
fn validated_code(code: &str) -> Result<String> {
    let mut normalized = normalize_code(code);
    let len = normalized.chars().count();
    let valid = (len == PAIR_CODE_LEN || len == PAIR_CODE_LEN_LONG)
        && normalized.bytes().all(|b| PAIR_CODE_ALPHABET.contains(&b));
    if !valid {
        normalized.zeroize();
        return Err(Error::InvalidCode);
    }
    Ok(normalized)
}

/// hub 用来撮合的匹配 id，即 `derive_pair_secrets(code).match_id`（16 个 hex 字符）。两端各自算好发给 hub，
/// hub 永远看不到短码本身。match id 仍是短码的函数：对 8 位短码（约 40 bit）拿到它的 hub 可以穷举，
/// 但每次猜测都要付一次 Argon2id（64 MiB × 3 轮）的代价；16 位短码（约 79 bit）不可行。hub 不应记录它。
pub fn derive_match_id(code: &str) -> Result<String> {
    Ok(derive_pair_secrets(code)?.match_id.clone())
}

/// 16 位长短码，`XXXX-XXXX-XXXX-XXXX`。
pub fn generate_pair_code_long() -> String {
    let raw = generate_code_raw(PAIR_CODE_LEN_LONG);
    raw.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-")
}

/// 公钥指纹：BLAKE2s-256 的前 8 字节 hex，用 `:` 分隔，如 `3f:a2:…`。
pub fn fingerprint(public: &[u8; 32]) -> String {
    let digest = Blake2s256::digest(public);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// snow 的 resolver：原语全部用 snow 默认实现，只把 RNG 换成 rand 0.8 的 `OsRng`。
#[derive(Default)]
struct Resolver;

struct SysRng;

impl Random for SysRng {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), snow::Error> {
        OsRng.try_fill_bytes(dest).map_err(|_| snow::Error::Rng)
    }
}

impl CryptoResolver for Resolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(SysRng))
    }
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        DefaultResolver.resolve_dh(choice)
    }
    fn resolve_hash(&self, choice: &snow::params::HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(choice)
    }
    fn resolve_cipher(&self, choice: &snow::params::CipherChoice) -> Option<Box<dyn Cipher>> {
        DefaultResolver.resolve_cipher(choice)
    }
}

fn map_snow(e: snow::Error) -> Error {
    match e {
        snow::Error::Decrypt => Error::Decrypt,
        other => Error::Noise(other.to_string()),
    }
}

fn builder(pattern: &str) -> Result<Builder<'static>> {
    let params: NoiseParams = pattern.parse().map_err(map_snow)?;
    Ok(Builder::with_resolver(params, Box::new(Resolver)))
}

fn to_key(bytes: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).ok()
}

/// 进行中的握手。
pub struct Handshake {
    state: HandshakeState,
    /// IK responder 可选的「必须是这把公钥」约束；读完第一条消息后比对。
    expected_remote: Option<[u8; 32]>,
    /// `read` 出错（或公钥不符）后置位：snow 的状态此时已不可信，整个握手作废。
    poisoned: bool,
}

impl Handshake {
    fn from_builder(b: Builder<'_>, role: Role, expected_remote: Option<[u8; 32]>) -> Result<Handshake> {
        let state = match role {
            Role::Initiator => b.build_initiator(),
            Role::Responder => b.build_responder(),
        }
        .map_err(map_snow)?;
        Ok(Handshake { state, expected_remote, poisoned: false })
    }

    /// 配对握手（XXpsk3）。initiator = joiner（新设备），responder = host（daemon）。
    pub fn pairing(role: Role, local: &Keypair, psk: &[u8; 32]) -> Result<Handshake> {
        let b =
            builder(PAIR_PATTERN)?.local_private_key(&local.secret).map_err(map_snow)?.psk(3, psk).map_err(map_snow)?;
        Handshake::from_builder(b, role, None)
    }

    /// 重连握手（IK）。initiator 必须给 `remote_public`；responder 传 `None`。
    ///
    /// responder 若传了 `Some(pk)`，读完第一条消息后会校验 initiator 的静态公钥必须等于 `pk`，不符则 `read` 返回 `InvalidKey`。
    pub fn reconnect(role: Role, local: &Keypair, remote_public: Option<&[u8; 32]>) -> Result<Handshake> {
        let mut b = builder(RECONNECT_PATTERN)?.local_private_key(&local.secret).map_err(map_snow)?;
        match (role, remote_public) {
            (Role::Initiator, None) => Err(Error::InvalidKey),
            (Role::Initiator, Some(pk)) => {
                b = b.remote_public_key(pk).map_err(map_snow)?;
                Handshake::from_builder(b, role, None)
            }
            (Role::Responder, expected) => Handshake::from_builder(b, role, expected.copied()),
        }
    }

    /// 现在该我写消息吗。握手已完成或已作废时恒为 `false`。
    pub fn is_my_turn(&self) -> bool {
        !self.poisoned && !self.state.is_handshake_finished() && self.state.is_my_turn()
    }

    /// 写下一条握手消息（可带 payload，通常为空）。
    ///
    /// 注意：XXpsk3 的 PSK 到第三条才混入密钥，所以第一、二条的 payload 对主动中间人（hub）既不保密也不可信，
    /// 只有第三条（及握手之后的 Transport）才绑定了短码。配对时需要交换的东西放第三条或握手完成后再发。
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(Error::Noise("handshake aborted".into()));
        }
        if self.state.is_handshake_finished() {
            return Err(Error::Finished);
        }
        if !self.state.is_my_turn() {
            return Err(Error::NotMyTurn);
        }
        let mut buf = vec![0u8; MAX_MSG];
        match self.state.write_message(payload, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(snow::Error::Input) => Err(Error::Noise("handshake payload too large".into())),
            Err(e) => {
                self.poisoned = true;
                Err(map_snow(e))
            }
        }
    }

    /// 读对方的握手消息，返回其 payload。出错后这个握手作废。
    pub fn read(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(Error::Noise("handshake aborted".into()));
        }
        if self.state.is_handshake_finished() {
            return Err(Error::Finished);
        }
        if self.state.is_my_turn() {
            return Err(Error::Noise("not your turn to read".into()));
        }
        let mut buf = vec![0u8; MAX_MSG];
        let n = match self.state.read_message(message, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                self.poisoned = true;
                return Err(map_snow(e));
            }
        };
        if let Some(expected) = self.expected_remote {
            if self.remote_public() != Some(expected) {
                self.poisoned = true;
                return Err(Error::InvalidKey);
            }
        }
        buf.truncate(n);
        Ok(buf)
    }

    pub fn is_finished(&self) -> bool {
        !self.poisoned && self.state.is_handshake_finished()
    }

    /// 对方静态公钥（握手进行到能知道的时候起可用；IK responder 在读完第一条消息后就有）。
    pub fn remote_public(&self) -> Option<[u8; 32]> {
        self.state.get_remote_static().and_then(to_key)
    }

    pub fn into_transport(self) -> Result<Transport> {
        if self.poisoned {
            return Err(Error::Noise("handshake aborted".into()));
        }
        if !self.state.is_handshake_finished() {
            return Err(Error::NotFinished);
        }
        let remote = self.remote_public().ok_or_else(|| Error::Noise("missing remote static key".into()))?;
        let state = self.state.into_transport_mode().map_err(map_snow)?;
        Ok(Transport { state, remote })
    }
}

/// 握手完成后的双向加密通道。
pub struct Transport {
    state: TransportState,
    remote: [u8; 32],
}

impl Transport {
    /// 加密任意长度明文，自动分片，见模块文档。空明文也产出一个（只含 tag 的）分片。
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // 空明文也要发一片（只有 tag），否则对端无法区分「空消息」和「没消息」。
        let chunks: Vec<&[u8]> = if plaintext.is_empty() { vec![&[]] } else { plaintext.chunks(MAX_CHUNK).collect() };
        let mut out = Vec::with_capacity(plaintext.len() + chunks.len() * (2 + TAG_LEN));
        let mut buf = vec![0u8; MAX_MSG];
        for chunk in chunks {
            let n = self.state.write_message(chunk, &mut buf).map_err(map_snow)?;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    /// 解密 `encrypt` 的输出，还原整条明文。
    ///
    /// 先整体校验分片边界（不动 nonce），再逐片解密；任何一片失败都会把接收 nonce 回滚到调用前，
    /// 所以一次失败的 `decrypt` 对通道状态没有副作用。
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut frames: Vec<&[u8]> = Vec::new();
        let mut rest = ciphertext;
        while !rest.is_empty() {
            if rest.len() < 2 {
                return Err(Error::Malformed);
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            if len < TAG_LEN || rest.len() < 2 + len {
                return Err(Error::Malformed);
            }
            frames.push(&rest[2..2 + len]);
            rest = &rest[2 + len..];
        }
        if frames.is_empty() {
            return Err(Error::Malformed);
        }

        let start_nonce = self.state.receiving_nonce();
        let mut out = Vec::with_capacity(ciphertext.len());
        let mut buf = vec![0u8; MAX_MSG];
        let mut result = Ok(());
        for frame in frames {
            match self.state.read_message(frame, &mut buf) {
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => {
                    result = Err(map_snow(e));
                    break;
                }
            }
        }
        buf.zeroize();
        match result {
            Ok(()) => Ok(out),
            Err(e) => {
                self.state.set_receiving_nonce(start_nonce);
                out.zeroize();
                Err(e)
            }
        }
    }

    pub fn remote_public(&self) -> [u8; 32] {
        self.remote
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    // 只接受 ASCII 十六进制：既避免按字节切 UTF-8 时 panic，也拒绝 `from_str_radix` 会放过的 `+f` 之类。
    if s.len() % 2 == 1 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidKey);
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| Error::InvalidKey)).collect()
}

pub mod enroll;
pub use enroll::*;

#[cfg(feature = "python")]
pub mod python;
#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(test)]
mod tests;
