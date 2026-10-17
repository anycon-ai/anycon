//! VAPID 密钥（RFC 8292）：P-256 密钥对，首次启动生成，存 `${ANYCON_DATA_DIR}/vapid.json`（0600）。
//!
//! 文件坏了**启动报错，不静默重新生成**——重新生成会让所有浏览器里已有的订阅失效（订阅绑定了公钥）。
//! 公钥经 `/api/config.vapid_public_key` 下发给浏览器（base64url，65 字节未压缩点）；私钥只在这个文件里，不进日志。

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde::{Deserialize, Serialize};

use super::webpush::{random_secret_key, uncompressed};

pub const VAPID_FILE: &str = "vapid.json";

#[derive(Debug, thiserror::Error)]
pub enum VapidError {
    #[error("cannot read or write {path}: {reason}")]
    Io { path: String, reason: String },
    #[error(
        "{path} is corrupt ({reason}). Refusing to generate a new key pair: every browser that enabled push would \
         silently stop receiving notifications. Restore the file from a backup, or delete it on purpose to start over \
         (all browsers must then turn push on again)"
    )]
    Corrupt { path: String, reason: String },
}

#[derive(Serialize, Deserialize)]
struct VapidFile {
    version: u32,
    /// base64url，32 字节标量。
    private_key: String,
    /// base64url，65 字节未压缩点。
    public_key: String,
}

/// 服务端的 VAPID 密钥对。
pub struct VapidKeys {
    signing: SigningKey,
    public: [u8; 65],
    public_b64: String,
}

/// 只输出公钥。
impl std::fmt::Debug for VapidKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VapidKeys").field("public_key", &self.public_b64).finish_non_exhaustive()
    }
}

impl VapidKeys {
    fn from_signing(signing: SigningKey) -> Self {
        let public = uncompressed(&p256::PublicKey::from(signing.verifying_key()));
        let public_b64 = URL_SAFE_NO_PAD.encode(public);
        Self { signing, public, public_b64 }
    }

    /// 新生成一对（不落盘）。
    pub fn generate() -> Self {
        Self::from_signing(SigningKey::from(random_secret_key()))
    }

    /// 读 `<dir>/vapid.json`；不存在就生成并以 0600 写入；存在但坏了返回 [`VapidError::Corrupt`]。
    pub fn load_or_create(dir: &Path) -> Result<Self, VapidError> {
        let path = dir.join(VAPID_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&path, &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let keys = Self::generate();
                match write_new(&path, &keys) {
                    Ok(()) => {
                        tracing::info!("generated a new VAPID key pair for Web Push (vapid.json); back it up together with the data directory");
                        Ok(keys)
                    }
                    // 另一个进程刚好抢先写了：用它写的那份。
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let text = std::fs::read_to_string(&path).map_err(|e| io_err(&path, &e))?;
                        Self::parse(&path, &text)
                    }
                    Err(e) => Err(io_err(&path, &e)),
                }
            }
            Err(e) => Err(io_err(&path, &e)),
        }
    }

    fn parse(path: &Path, text: &str) -> Result<Self, VapidError> {
        let corrupt =
            |reason: &str| VapidError::Corrupt { path: path.display().to_string(), reason: reason.to_string() };
        let file: VapidFile =
            serde_json::from_str(text).map_err(|_| corrupt("not valid JSON of the expected shape"))?;
        if file.version != 1 {
            return Err(corrupt("unknown version"));
        }
        let private =
            URL_SAFE_NO_PAD.decode(file.private_key.trim()).map_err(|_| corrupt("private_key is not base64url"))?;
        let signing = SigningKey::from_slice(&private).map_err(|_| corrupt("private_key is not a valid P-256 key"))?;
        let keys = Self::from_signing(signing);
        let stored =
            URL_SAFE_NO_PAD.decode(file.public_key.trim()).map_err(|_| corrupt("public_key is not base64url"))?;
        if stored != keys.public {
            return Err(corrupt("public_key does not belong to private_key"));
        }
        tighten_permissions(path);
        Ok(keys)
    }

    /// base64url 的 65 字节未压缩公钥（`/api/config.vapid_public_key`，也是 `Authorization: vapid k=`）。
    pub fn public_key(&self) -> &str {
        &self.public_b64
    }

    /// 签一个 VAPID JWT（ES256）：`aud` 是推送服务 endpoint 的 origin，`exp` 是 Unix 秒（≤ 24 小时后），`sub` 是联系方式。
    pub fn jwt(&self, audience: &str, expires_at_unix: u64, subject: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": audience, "exp": expires_at_unix, "sub": subject });
        let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        // ES256 的签名是固定 64 字节的 r ‖ s（不是 DER）。RFC 6979 确定性签名，不需要随机数。
        let sig: Signature = self.signing.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes().as_slice()))
    }
}

fn io_err(path: &Path, e: &std::io::Error) -> VapidError {
    VapidError::Io { path: path.display().to_string(), reason: e.to_string() }
}

fn write_new(path: &PathBuf, keys: &VapidKeys) -> std::io::Result<()> {
    use std::io::Write;
    let file = VapidFile {
        version: 1,
        private_key: URL_SAFE_NO_PAD.encode(keys.signing.to_bytes().as_slice()),
        public_key: keys.public_b64.clone(),
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    writeln!(f, "{json}")?;
    f.sync_all()
}

/// 文件权限被放宽过（比如备份恢复时）：收紧回 0600，并提醒一声。
fn tighten_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o077 != 0 {
                tracing::warn!("vapid.json was readable by other users; tightening it to 0600");
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    #[test]
    fn first_start_generates_a_0600_file_and_later_starts_reuse_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = VapidKeys::load_or_create(dir.path()).unwrap();
        let path = dir.path().join(VAPID_FILE);
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let second = VapidKeys::load_or_create(dir.path()).unwrap();
        assert_eq!(first.public_key(), second.public_key(), "restarts keep the key pair");
        // 公钥是 65 字节未压缩点的 base64url
        let raw = URL_SAFE_NO_PAD.decode(first.public_key()).unwrap();
        assert_eq!((raw.len(), raw[0]), (65, 4));
        // 另一份数据目录得到另一对
        let other = tempfile::tempdir().unwrap();
        assert_ne!(VapidKeys::load_or_create(other.path()).unwrap().public_key(), first.public_key());
    }

    #[test]
    fn a_corrupt_file_is_an_error_and_is_never_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VAPID_FILE);
        let good = VapidKeys::load_or_create(dir.path()).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();

        let other = VapidKeys::generate();
        let mismatched = original.replace(good.public_key(), other.public_key());
        for (what, content) in [
            ("empty", String::new()),
            ("garbage", "not json at all".to_string()),
            ("wrong shape", r#"{"hello":"world"}"#.to_string()),
            ("truncated", original[..original.len() / 2].to_string()),
            ("unknown version", original.replace("\"version\": 1", "\"version\": 2")),
            ("bad private key", original.replace("\"private_key\": \"", "\"private_key\": \"AAAA")),
            ("public key of another pair", mismatched),
        ] {
            std::fs::write(&path, &content).unwrap();
            let err = VapidKeys::load_or_create(dir.path()).unwrap_err();
            assert!(matches!(err, VapidError::Corrupt { .. }), "{what}: {err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "{what}: the file is left alone");
        }
        // 恢复原文件又能用
        std::fs::write(&path, &original).unwrap();
        assert_eq!(VapidKeys::load_or_create(dir.path()).unwrap().public_key(), good.public_key());
    }

    #[test]
    fn debug_output_has_only_the_public_key() {
        let k = VapidKeys::generate();
        let dump = format!("{k:?}");
        assert!(dump.contains(k.public_key()));
        assert!(!dump.contains(&URL_SAFE_NO_PAD.encode(k.signing.to_bytes().as_slice())), "{dump}");
    }

    #[test]
    fn jwt_is_a_valid_es256_token_that_the_public_key_verifies() {
        let keys = VapidKeys::generate();
        let jwt = keys.jwt("https://fcm.googleapis.com", 1_900_000_000, "mailto:admin@example.com");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, serde_json::json!({ "typ": "JWT", "alg": "ES256" }));
        let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({ "aud": "https://fcm.googleapis.com", "exp": 1_900_000_000u64, "sub": "mailto:admin@example.com" })
        );
        assert!(!jwt.contains('=') && !jwt.contains('+') && !jwt.contains('/'), "base64url without padding");

        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64, "ES256 signatures are the raw 64-byte r || s, not DER");
        let verifier = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(keys.public_key()).unwrap()).unwrap();
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        verifier.verify(signing_input.as_bytes(), &sig).expect("the VAPID public key verifies the token");
        // 篡改载荷验不过；别人的公钥验不过
        let forged =
            format!("{}.{}", parts[0], URL_SAFE_NO_PAD.encode(br#"{"aud":"https://evil.example","exp":1,"sub":"x"}"#));
        assert!(verifier.verify(forged.as_bytes(), &sig).is_err());
        let stranger = VapidKeys::generate();
        let stranger_verifier =
            VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(stranger.public_key()).unwrap()).unwrap();
        assert!(stranger_verifier.verify(signing_input.as_bytes(), &sig).is_err());
    }
}
