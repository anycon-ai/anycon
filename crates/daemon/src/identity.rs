//! 节点身份：Noise 静态私钥（`keys/node.key`）、hub 签发的设备 id / token。
//!
//! 私钥文件是 32 字节随机数的 hex，只有属主能读写（unix 0600；Windows 见 [`crate::sys::restrict_to_owner`]）。`anycon init` 生成；daemon 启动时只读取。
//! 公钥用 `anycon_crypto::Keypair::from_secret` 现算，所以本模块不依赖 crypto 的实现是否就绪来生成密钥。

use std::io::Write;
use std::path::Path;
use std::sync::RwLock;

use anycon_crypto::Keypair;

use crate::config::Paths;
use crate::sys::OwnerOnly;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("not initialized: {0} is missing (run `anycon init`)")]
    NotInitialized(String),
    #[error("invalid key file {0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Identity {
    secret: [u8; 32],
    paths: Paths,
    device_id: RwLock<Option<String>>,
    device_token: RwLock<Option<String>>,
}

/// 写一个只有所有者可读写的文件（原子替换）。
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).owner_only().open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    // 已有的 tmp 文件不受 `owner_only` 影响；Windows 上数据目录可能还没限制过（只用 CLI 的机器），这里显式设一次
    crate::sys::restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, path)
}

/// 生成并写入新的 node.key，返回私钥。
pub fn create_node_key(paths: &Paths) -> std::io::Result<[u8; 32]> {
    let secret = crate::util::random_bytes::<32>();
    write_private(&paths.node_key(), &hex::encode(secret))?;
    Ok(secret)
}

pub fn read_secret(path: &Path) -> Result<[u8; 32], IdentityError> {
    let text = std::fs::read_to_string(path).map_err(|_| IdentityError::NotInitialized(path.display().to_string()))?;
    let bytes = hex::decode(text.trim()).map_err(|_| IdentityError::Invalid(path.display().to_string()))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| IdentityError::Invalid(path.display().to_string()))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl Identity {
    pub fn load(paths: &Paths) -> Result<Identity, IdentityError> {
        let secret = read_secret(&paths.node_key())?;
        Ok(Identity {
            secret,
            paths: paths.clone(),
            device_id: RwLock::new(read_trimmed(&paths.device_id())),
            device_token: RwLock::new(read_trimmed(&paths.device_token())),
        })
    }

    /// 测试用：直接给私钥。
    pub fn from_secret(paths: &Paths, secret: [u8; 32]) -> Identity {
        Identity { secret, paths: paths.clone(), device_id: RwLock::new(None), device_token: RwLock::new(None) }
    }

    pub fn keypair(&self) -> Keypair {
        Keypair::from_secret(self.secret)
    }

    pub fn public(&self) -> [u8; 32] {
        self.keypair().public
    }

    pub fn device_id(&self) -> Option<String> {
        self.device_id.read().unwrap().clone()
    }

    pub fn device_token(&self) -> Option<String> {
        self.device_token.read().unwrap().clone()
    }

    /// hub 签发的正式身份（首次 `PairDone` 或轮换后）。
    pub fn set_device(&self, id: Option<&str>, token: Option<&str>) -> std::io::Result<()> {
        if let Some(t) = token {
            write_private(&self.paths.device_token(), t)?;
            *self.device_token.write().unwrap() = Some(t.to_string());
        }
        if let Some(i) = id {
            write_private(&self.paths.device_id(), i)?;
            *self.device_id.write().unwrap() = Some(i.to_string());
        }
        Ok(())
    }

    /// `AuthOk` 带回的 device_id（不改 token）。
    pub fn note_device_id(&self, id: &str) {
        if self.device_id().as_deref() != Some(id) {
            crate::util::warn_err("persist the device id", write_private(&self.paths.device_id(), id));
            *self.device_id.write().unwrap() = Some(id.to_string());
        }
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.secret = [0u8; 32];
    }
}
