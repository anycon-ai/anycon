//! wasm-bindgen 导出层（feature `wasm`）：把 [`crate`] 的公开 API 原样搬到 JS。
//!
//! 约定：
//! - 密钥、PSK、公钥、消息全部是 `Uint8Array`；短码和指纹是 `string`。
//! - 所有失败都变成 JS `Error`，`message` 就是 Rust 侧 [`crate::Error`] 的文本
//!   （`decrypt failed`、`malformed frame`、`invalid pair code`……），调用方可以按文本区分。
//! - `role` 只接受 `"initiator"` / `"responder"`。
//! - JS 没有析构：不再使用的对象请调用 `.free()`。`WasmKeypair` 释放时私钥会被清零；
//!   `secret` getter 返回的是拷贝，调用方自己负责用完 `fill(0)`。
//! - `WasmHandshake.intoTransport()` 会消费握手对象，之后再碰它会抛 null pointer 错误。

use wasm_bindgen::prelude::*;

use crate::{Handshake, Keypair, Role, Transport};

fn js_err(e: crate::Error) -> JsError {
    JsError::new(&e.to_string())
}

fn key32(bytes: &[u8]) -> Result<[u8; 32], JsError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| js_err(crate::Error::InvalidKey))
}

fn parse_role(role: &str) -> Result<Role, JsError> {
    match role {
        "initiator" => Ok(Role::Initiator),
        "responder" => Ok(Role::Responder),
        other => Err(JsError::new(&format!("invalid role {other:?}, expected \"initiator\" or \"responder\""))),
    }
}

/// X25519 静态密钥对。
#[wasm_bindgen]
pub struct WasmKeypair {
    inner: Keypair,
}

#[wasm_bindgen]
impl WasmKeypair {
    /// 用 `crypto.getRandomValues` 生成新密钥对。
    pub fn generate() -> WasmKeypair {
        WasmKeypair { inner: Keypair::generate() }
    }

    /// 由 32 字节私钥还原。
    #[wasm_bindgen(js_name = fromSecret)]
    pub fn from_secret(secret: &[u8]) -> Result<WasmKeypair, JsError> {
        Ok(WasmKeypair { inner: Keypair::from_secret(key32(secret)?) })
    }

    #[wasm_bindgen(getter)]
    pub fn public(&self) -> Vec<u8> {
        self.inner.public.to_vec()
    }

    #[wasm_bindgen(getter)]
    pub fn secret(&self) -> Vec<u8> {
        self.inner.secret.to_vec()
    }
}

/// 进行中的握手。
#[wasm_bindgen]
pub struct WasmHandshake {
    inner: Handshake,
}

#[wasm_bindgen]
impl WasmHandshake {
    /// 配对握手（XXpsk3）。joiner 传 `"initiator"`，host 传 `"responder"`。
    pub fn pairing(role: &str, kp: &WasmKeypair, psk: &[u8]) -> Result<WasmHandshake, JsError> {
        let inner = Handshake::pairing(parse_role(role)?, &kp.inner, &key32(psk)?).map_err(js_err)?;
        Ok(WasmHandshake { inner })
    }

    /// 重连握手（IK）。initiator 必须传 `remotePublic`；responder 不传（或传期望的 initiator 公钥做钉扎）。
    // 参数名直接决定 .d.ts 里的名字，所以这里用 JS 风格的 camelCase。
    #[allow(non_snake_case)]
    pub fn reconnect(role: &str, kp: &WasmKeypair, remotePublic: Option<Vec<u8>>) -> Result<WasmHandshake, JsError> {
        let remote = remotePublic.as_deref().map(key32).transpose()?;
        let inner = Handshake::reconnect(parse_role(role)?, &kp.inner, remote.as_ref()).map_err(js_err)?;
        Ok(WasmHandshake { inner })
    }

    #[wasm_bindgen(js_name = isMyTurn)]
    pub fn is_my_turn(&self) -> bool {
        self.inner.is_my_turn()
    }

    /// 写下一条握手消息，返回要发给对方的字节。
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>, JsError> {
        self.inner.write(payload).map_err(js_err)
    }

    /// 读对方的握手消息，返回对方带的 payload。
    pub fn read(&mut self, message: &[u8]) -> Result<Vec<u8>, JsError> {
        self.inner.read(message).map_err(js_err)
    }

    #[wasm_bindgen(js_name = isFinished)]
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    /// 对方静态公钥；还不知道时是 `undefined`。
    #[wasm_bindgen(js_name = remotePublic)]
    pub fn remote_public(&self) -> Option<Vec<u8>> {
        self.inner.remote_public().map(|k| k.to_vec())
    }

    /// 握手完成后换成传输通道。会消费本对象。
    #[wasm_bindgen(js_name = intoTransport)]
    pub fn into_transport(self) -> Result<WasmTransport, JsError> {
        Ok(WasmTransport { inner: self.inner.into_transport().map_err(js_err)? })
    }
}

/// 握手完成后的双向加密通道。
#[wasm_bindgen]
pub struct WasmTransport {
    inner: Transport,
}

#[wasm_bindgen]
impl WasmTransport {
    /// 加密任意长度明文；超过 65519 字节自动分片（`[u16 be 长度][密文]` 拼接）。
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, JsError> {
        self.inner.encrypt(plaintext).map_err(js_err)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, JsError> {
        self.inner.decrypt(ciphertext).map_err(js_err)
    }

    #[wasm_bindgen(js_name = remotePublic)]
    pub fn remote_public(&self) -> Vec<u8> {
        self.inner.remote_public().to_vec()
    }
}

/// 一次 Argon2id 得到的 `matchId` + `psk`。用完 `.free()`（PSK 会被清零）。
#[wasm_bindgen]
pub struct WasmPairSecrets {
    inner: crate::PairSecrets,
}

#[wasm_bindgen]
impl WasmPairSecrets {
    #[wasm_bindgen(getter, js_name = matchId)]
    pub fn match_id(&self) -> String {
        self.inner.match_id.clone()
    }
    /// 32 字节 PSK 的拷贝，调用方用完 `fill(0)`。
    #[wasm_bindgen(getter)]
    pub fn psk(&self) -> Vec<u8> {
        self.inner.psk.to_vec()
    }
}

/// 短码 → `{ matchId, psk }`，一次 KDF（Argon2id，64 MiB × 3 轮，浏览器里约 0.5–1 s）。
/// 配对时用它，不要分别调 `deriveMatchId` 和 `derivePsk`（会算两次）。短码无效时抛 `invalid pair code`。
#[wasm_bindgen(js_name = derivePairSecrets)]
pub fn derive_pair_secrets(code: &str) -> Result<WasmPairSecrets, JsError> {
    crate::derive_pair_secrets(code).map(|inner| WasmPairSecrets { inner }).map_err(js_err)
}

/// 短码 → 32 字节 PSK（一次完整 KDF）。短码无效时抛 `invalid pair code`。
#[wasm_bindgen(js_name = derivePsk)]
pub fn derive_psk(code: &str) -> Result<Vec<u8>, JsError> {
    crate::derive_psk(code).map(|k| k.to_vec()).map_err(js_err)
}

/// 短码 → hub 撮合用的 match id（一次完整 KDF）。
#[wasm_bindgen(js_name = deriveMatchId)]
pub fn derive_match_id(code: &str) -> Result<String, JsError> {
    crate::derive_match_id(code).map_err(js_err)
}

#[wasm_bindgen(js_name = generatePairCodeLong)]
pub fn generate_pair_code_long() -> String {
    crate::generate_pair_code_long()
}

#[wasm_bindgen(js_name = normalizeCode)]
pub fn normalize_code(code: &str) -> String {
    crate::normalize_code(code)
}

/// 生成 `XXXX-XXXX` 形式的 8 位短码。
#[wasm_bindgen(js_name = generatePairCode)]
pub fn generate_pair_code() -> String {
    crate::generate_pair_code()
}

/// 公钥指纹，如 `3f:a2:…`（8 字节）。公钥必须是 32 字节。
#[wasm_bindgen]
pub fn fingerprint(public: &[u8]) -> Result<String, JsError> {
    Ok(crate::fingerprint(&key32(public)?))
}

// ---- 账号内接入（ADR-36）----

/// 一方的 CPace 状态。新设备用 `"initiator"`。`finish` 会消费本对象（之后再碰会抛 null pointer 错误）。
#[wasm_bindgen]
pub struct WasmCpace {
    inner: crate::Cpace,
}

#[wasm_bindgen]
impl WasmCpace {
    /// `prs`：口令的 `deriveEnrollPasswordPrs`，或规范化后 6 位码的 ASCII 字节；`ci`：`enrollChannelId`；`sid`：`enrollSid`。
    #[wasm_bindgen(constructor)]
    pub fn new(prs: &[u8], ci: &[u8], sid: &[u8]) -> Result<WasmCpace, JsError> {
        Ok(WasmCpace { inner: crate::Cpace::new(prs, ci, sid).map_err(js_err)? })
    }

    /// 自己的份额（32 字节），发给对方。
    #[wasm_bindgen(getter)]
    pub fn share(&self) -> Vec<u8> {
        self.inner.share().to_vec()
    }

    /// 用对方的份额得到 32 字节 PSK。份额不合法时抛 `invalid key`。
    #[allow(non_snake_case)]
    pub fn finish(self, role: &str, peerShare: &[u8]) -> Result<Vec<u8>, JsError> {
        self.inner.finish(parse_role(role)?, peerShare).map(|k| k.to_vec()).map_err(js_err)
    }
}

/// 16 字节随机数（`EnrollHello.nonce`）。
#[wasm_bindgen(js_name = generateEnrollNonce)]
pub fn generate_enroll_nonce() -> Vec<u8> {
    crate::generate_enroll_nonce().to_vec()
}

/// 32 字节扫码密钥（放进二维码）。
#[wasm_bindgen(js_name = generateEnrollSecret)]
pub fn generate_enroll_secret() -> Vec<u8> {
    crate::generate_enroll_secret().to_vec()
}

/// 接入码规范化：去掉空白和 `-`，必须是 6 个数字，否则抛 `invalid pair code`。
#[wasm_bindgen(js_name = normalizeEnrollCode)]
pub fn normalize_enroll_code(code: &str) -> Result<String, JsError> {
    crate::normalize_enroll_code(code).map_err(js_err)
}

/// 节点口令校验（设置时用）：NFC 之后至少 8 个字符，否则抛 `enroll password too short`。
#[wasm_bindgen(js_name = validateEnrollPassword)]
pub fn validate_enroll_password(password: &str) -> Result<(), JsError> {
    crate::validate_enroll_password(password).map_err(js_err)
}

/// 节点口令 → CPace 的口令输入：`Argon2id(NFC(口令), salt)`，浏览器里约 0.5–1 s。`salt` 必须是 16 字节。
#[wasm_bindgen(js_name = deriveEnrollPasswordPrs)]
pub fn derive_enroll_password_prs(password: &str, salt: &[u8]) -> Result<Vec<u8>, JsError> {
    crate::derive_enroll_password_prs(password, salt).map(|k| k.to_vec()).map_err(js_err)
}

/// 扫码接入的 PSK。`secret` 必须是 32 字节。
#[allow(non_snake_case)]
#[wasm_bindgen(js_name = deriveEnrollScanPsk)]
pub fn derive_enroll_scan_psk(secret: &[u8], nodeId: &str) -> Result<Vec<u8>, JsError> {
    Ok(crate::derive_enroll_scan_psk(&key32(secret)?, nodeId).to_vec())
}

/// CPace 信道标识。`method` 是 `"code"` 或 `"password"`。
#[allow(non_snake_case)]
#[wasm_bindgen(js_name = enrollChannelId)]
pub fn enroll_channel_id(nodeId: &str, method: &str) -> Vec<u8> {
    crate::enroll_channel_id(nodeId, method)
}

/// CPace 会话标识：新设备的 nonce ‖ daemon 的 nonce。
#[allow(non_snake_case)]
#[wasm_bindgen(js_name = enrollSid)]
pub fn enroll_sid(initiatorNonce: &[u8], responderNonce: &[u8]) -> Vec<u8> {
    crate::enroll_sid(initiatorNonce, responderNonce)
}
