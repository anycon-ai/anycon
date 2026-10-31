//! 节点身份：Noise 静态私钥（`keys/node.key`）、hub 签发的设备 id / token。
//!
//! 私钥文件是 32 字节随机数的 hex，只有属主能读写（unix 0600；Windows 见 [`crate::sys::restrict_to_owner`]）。`anycon init` 生成；daemon 启动时只读取。
//! 公钥用 `anycon_crypto::Keypair::from_secret` 现算，所以本模块不依赖 crypto 的实现是否就绪来生成密钥。

use std::io::Write;
use std::path::Path;
use std::sync::RwLock;
use std::time::Duration;

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

/// 设备 token 签发满这么久，就在下一次连上 hub 之后请求轮换（docs/spec/hub.md § 5，daemon.md § 15）。
pub const TOKEN_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// 发出 `rotate_token` 之后没等到新 token，隔这么久再发一次。比 hub 的「同一设备 1 分钟内只轮换一次」长：
/// 回执丢了的话，上一次轮换已经过了限频窗口，这次能成。
pub const ROTATE_RETRY: Duration = Duration::from_secs(75);
/// 一条连接上头几次 `rotate_token` 之间隔 [`ROTATE_RETRY`]，之后改成每隔 [`ROTATE_RECHECK`]。
pub const ROTATE_QUICK_SENDS: u32 = 3;
/// 不需要发（还没到期）或头几次已经发完时的复查间隔：连着 hub 几十天不重连的 daemon 也要能轮换；
/// hub 一直不回复（比如不支持轮换）时也只是每小时一帧。
pub const ROTATE_RECHECK: Duration = Duration::from_secs(3600);

/// token 该轮换了吗？签发时间未知（轮换功能之前的身份）、在「未来」（时钟被往回拨过，不可信）、或已满 [`TOKEN_MAX_AGE`] 都算。
pub fn token_due(issued_at_ms: Option<u64>, now_ms: u64) -> bool {
    match issued_at_ms {
        None => true,
        Some(t) if t > now_ms => true,
        Some(t) => now_ms - t >= TOKEN_MAX_AGE.as_millis() as u64,
    }
}

/// 一条连接上已经发了 `sends` 次 `rotate_token`，多久之后再检查一次。
pub fn next_rotation_check(sends: u32) -> Duration {
    if sends < ROTATE_QUICK_SENDS {
        ROTATE_RETRY
    } else {
        ROTATE_RECHECK
    }
}

/// 读签发时间文件：十进制 Unix 毫秒；缺失或写坏了都当作未知。
pub fn read_issued_at(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

pub struct Identity {
    secret: [u8; 32],
    paths: Paths,
    device_id: RwLock<Option<String>>,
    device_token: RwLock<Option<String>>,
    /// `device_token` 的签发时间（Unix 毫秒）；未知 = 当作该轮换了。
    token_issued_at_ms: RwLock<Option<u64>>,
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
        let token = read_trimmed(&paths.device_token());
        let issued = token.as_ref().and_then(|_| read_issued_at(&paths.device_token_issued_at()));
        Ok(Identity {
            secret,
            paths: paths.clone(),
            device_id: RwLock::new(read_trimmed(&paths.device_id())),
            device_token: RwLock::new(token),
            token_issued_at_ms: RwLock::new(issued),
        })
    }

    /// 测试用：直接给私钥。
    pub fn from_secret(paths: &Paths, secret: [u8; 32]) -> Identity {
        Identity {
            secret,
            paths: paths.clone(),
            device_id: RwLock::new(None),
            device_token: RwLock::new(None),
            token_issued_at_ms: RwLock::new(None),
        }
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

    /// token 的签发时间（Unix 毫秒）；没有 token 或不知道是 `None`。
    pub fn token_issued_at_ms(&self) -> Option<u64> {
        *self.token_issued_at_ms.read().unwrap()
    }

    /// 持有正式 token 且它该轮换了（[`token_due`]）。没有 token 的临时身份不轮换。
    pub fn token_due(&self, now_ms: u64) -> bool {
        self.device_token().is_some() && token_due(self.token_issued_at_ms(), now_ms)
    }

    /// 把 token 和它的签发时间写盘：**先 token、后签发时间**，各自是「临时文件 + rename」的原子替换。
    /// 中途崩溃只可能留下「新 token + 旧 / 没有签发时间」（下次连上 hub 多轮换一次），不会留下「新签发时间 + 旧 token」
    /// （那会让一个已经被 hub 换掉的 token 被当成新的，之后宽限一过就认证失败）。
    fn persist_token(&self, token: &str, now_ms: u64) -> std::io::Result<()> {
        write_private(&self.paths.device_token(), token)?;
        write_private(&self.paths.device_token_issued_at(), &now_ms.to_string())
    }

    /// hub 签发的正式身份（首次 `PairDone`，或账号内接入完成）。token 的签发时间记为现在。
    pub fn set_device(&self, id: Option<&str>, token: Option<&str>) -> std::io::Result<()> {
        if let Some(t) = token {
            let now = crate::util::now_ms();
            write_private(&self.paths.device_token(), t)?;
            *self.device_token.write().unwrap() = Some(t.to_string());
            *self.token_issued_at_ms.write().unwrap() = Some(now);
            // 签发时间是这条记录的另一半，写不进去只意味着下次连上 hub 会多轮换一次，不影响 token 本身
            crate::util::warn_err(
                "persist the device token issue time",
                write_private(&self.paths.device_token_issued_at(), &now.to_string()),
            );
        }
        if let Some(i) = id {
            write_private(&self.paths.device_id(), i)?;
            *self.device_id.write().unwrap() = Some(i.to_string());
        }
        Ok(())
    }

    /// 轮换：hub 回了 `token_rotated`。先写盘（[`Self::persist_token`]），再让这个进程之后的重连用新 token。
    ///
    /// 写盘失败**也**换上内存里的 token：hub 已经换了，旧 token 只再有效 60 秒，继续用旧的只会让下一次重连认证失败。
    /// 代价是进程重启之前没有成功的轮换的话，重启后读到的是旧 token（已失效）——所以错误交给调用方大声记下来。
    pub fn rotate_token(&self, token: &str, now_ms: u64) -> std::io::Result<()> {
        let persisted = self.persist_token(token, now_ms);
        *self.device_token.write().unwrap() = Some(token.to_string());
        *self.token_issued_at_ms.write().unwrap() = Some(now_ms);
        persisted
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

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 24 * 3600 * 1000;

    fn fresh(dir: &Path) -> (Paths, Identity) {
        let paths = Paths::new(dir.join("home"));
        create_node_key(&paths).unwrap();
        let id = Identity::load(&paths).unwrap();
        (paths, id)
    }

    #[test]
    fn a_token_is_due_when_its_age_is_unknown_in_the_future_or_at_least_30_days() {
        let now = 100 * DAY_MS;
        assert!(token_due(None, now), "no record: from before rotation existed, rotate once");
        assert!(token_due(Some(now + 1), now), "issued in the future: the clock went backwards");
        assert!(!token_due(Some(now), now));
        assert!(!token_due(Some(now - 30 * DAY_MS + 1), now));
        assert!(token_due(Some(now - 30 * DAY_MS), now), "exactly 30 days is due");
        assert!(token_due(Some(0), now));
    }

    #[test]
    fn retries_are_75_s_apart_for_the_first_three_sends_and_hourly_after_that() {
        assert_eq!(next_rotation_check(0), Duration::from_secs(75));
        assert_eq!(next_rotation_check(2), Duration::from_secs(75));
        assert_eq!(next_rotation_check(3), Duration::from_secs(3600));
        assert_eq!(next_rotation_check(40), Duration::from_secs(3600));
        // 要比 hub 的「同一设备 1 分钟内只轮换一次」长，重发才不会撞在限频窗口里
        assert!(ROTATE_RETRY > Duration::from_secs(60));
    }

    #[test]
    fn pairing_records_the_issue_time_and_a_restart_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, id) = fresh(dir.path());
        assert!(id.device_token().is_none() && id.token_issued_at_ms().is_none());
        assert!(!id.token_due(u64::MAX), "a temporary identity has no token to rotate");

        let before = crate::util::now_ms();
        id.set_device(Some("dev"), Some("tok1")).unwrap();
        let issued = id.token_issued_at_ms().unwrap();
        assert!(issued >= before);
        assert_eq!(std::fs::read_to_string(paths.device_token_issued_at()).unwrap(), issued.to_string());
        assert!(!id.token_due(issued + DAY_MS));
        assert!(id.token_due(issued + 31 * DAY_MS));

        let again = Identity::load(&paths).unwrap();
        assert_eq!((again.device_token().as_deref(), again.token_issued_at_ms()), (Some("tok1"), Some(issued)));
    }

    #[test]
    fn a_token_without_an_issue_time_file_counts_as_old() {
        // 轮换功能之前配对的 daemon：只有 device-token
        let dir = tempfile::tempdir().unwrap();
        let (paths, _) = fresh(dir.path());
        write_private(&paths.device_token(), "legacy").unwrap();
        let id = Identity::load(&paths).unwrap();
        assert_eq!(id.device_token().as_deref(), Some("legacy"));
        assert_eq!(id.token_issued_at_ms(), None);
        assert!(id.token_due(crate::util::now_ms()));
        // 签发时间文件写坏了也一样
        write_private(&paths.device_token_issued_at(), "not a number").unwrap();
        assert!(Identity::load(&paths).unwrap().token_due(crate::util::now_ms()));
        // 没有 token 时留着的签发时间文件不算数
        std::fs::remove_file(paths.device_token()).unwrap();
        write_private(&paths.device_token_issued_at(), "12345").unwrap();
        assert_eq!(Identity::load(&paths).unwrap().token_issued_at_ms(), None);
    }

    #[test]
    fn rotating_saves_token_and_issue_time_atomically_with_private_permissions_and_switches_to_the_new_token() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, id) = fresh(dir.path());
        id.set_device(Some("dev"), Some("tok1")).unwrap();
        id.rotate_token("tok2", 5_000).unwrap();

        assert_eq!(id.device_token().as_deref(), Some("tok2"));
        assert_eq!(id.token_issued_at_ms(), Some(5_000));
        assert_eq!(std::fs::read_to_string(paths.device_token()).unwrap(), "tok2");
        assert_eq!(std::fs::read_to_string(paths.device_token_issued_at()).unwrap(), "5000");
        for f in [paths.device_token(), paths.device_token_issued_at()] {
            assert!(crate::sys::mode_bits(&std::fs::metadata(&f).unwrap()).is_none_or(|m| m == 0o600), "{f:?}");
        }
        // 原子替换：目录里没有留下临时文件
        let leftovers: Vec<_> = std::fs::read_dir(paths.keys_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // 设备 id 不受影响；重启后读到新的
        assert_eq!(id.device_id().as_deref(), Some("dev"));
        let again = Identity::load(&paths).unwrap();
        assert_eq!((again.device_token().as_deref(), again.token_issued_at_ms()), (Some("tok2"), Some(5_000)));
    }

    #[cfg(unix)]
    #[test]
    fn when_saving_fails_the_new_token_is_still_used_in_memory_and_the_error_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (paths, id) = fresh(dir.path());
        id.set_device(Some("dev"), Some("tok1")).unwrap();
        // keys/ 不可写：临时文件建不出来
        std::fs::set_permissions(paths.keys_dir(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable = std::fs::File::create(paths.keys_dir().join("probe")).is_ok();
        if writable {
            // 以 root 跑（权限位不起作用）：这个场景造不出来
            std::fs::set_permissions(paths.keys_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let r = id.rotate_token("tok2", 7_000);
        std::fs::set_permissions(paths.keys_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err());
        assert_eq!(id.device_token().as_deref(), Some("tok2"), "the hub already switched; the old one dies in 60 s");
        assert_eq!(id.token_issued_at_ms(), Some(7_000));
        assert_eq!(std::fs::read_to_string(paths.device_token()).unwrap(), "tok1", "the saved copy is untouched");
    }
}
