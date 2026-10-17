//! 首次启动：管理员账号与 admin token。
//!
//! - `ANYCON_ADMIN_TOKEN` 设置了就用它；
//! - 否则读 `<data>/admin-token`；
//! - 都没有就生成 32 字节随机数（hex），以 0600 写入 `<data>/admin-token`，并把明文交给调用方打印一次。
//!
//! bootstrap token 不在库里：登录时直接和它的哈希比较。token 变了（改环境变量重启）会清掉管理员账号
//! 已有的浏览器会话——「改它重启即重置登录」。

use std::path::Path;

use crate::app::AdminAuth;
use crate::store::{Store, StoreError};
use crate::util::{hash_token, new_secret, now_ms};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot use data directory {path}: {reason}")]
    DataDir { path: String, reason: String },
    #[error("cannot read or write {path}: {reason}")]
    TokenFile { path: String, reason: String },
}

pub struct Bootstrapped {
    pub admin: AdminAuth,
    /// 本次启动新生成的 token 明文（仅首次，需要打印给用户）。
    pub generated_token: Option<String>,
    pub token_file: std::path::PathBuf,
}

/// 确保数据目录存在且可写。
pub fn prepare_data_dir(dir: &Path) -> Result<(), BootstrapError> {
    let err = |reason: String| BootstrapError::DataDir { path: dir.display().to_string(), reason };
    std::fs::create_dir_all(dir).map_err(|e| err(format!("{e} (create it and make it writable by the hub user)")))?;
    let probe = dir.join(".anycon-write-test");
    std::fs::write(&probe, b"").map_err(|e| {
        err(format!(
            "not writable: {e}. In Docker, the image runs as uid 65532: `chown -R 65532:65532 <host dir>` or use a named volume"
        ))
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

pub async fn bootstrap_admin(
    store: &dyn Store,
    data_dir: &Path,
    env_token: Option<&str>,
) -> Result<Bootstrapped, BootstrapError> {
    let token_file = data_dir.join("admin-token");
    let admin = match store.first_admin().await? {
        Some(a) => a,
        None => store.create_account("admin", true, now_ms()).await?,
    };

    let mut generated = None;
    let token = match env_token {
        Some(t) => t.to_string(),
        None => match read_token_file(&token_file)? {
            Some(t) => t,
            None => {
                let t = new_secret();
                write_token_file(&token_file, &t)?;
                generated = Some(t.clone());
                t
            }
        },
    };
    let token_hash = hash_token(&token);

    // token 换了：旧 token 登出来的会话全部作废。
    let known = store.kv_get("admin_token_hash").await?;
    if known.as_deref() != Some(token_hash.as_str()) {
        if known.is_some() {
            let n = store.delete_web_sessions_of_account(&admin.id).await?;
            tracing::info!(sessions_revoked = n, "admin token changed, admin sessions reset");
        }
        store.kv_set("admin_token_hash", &token_hash).await?;
    }

    Ok(Bootstrapped { admin: AdminAuth { token_hash, account_id: admin.id }, generated_token: generated, token_file })
}

fn read_token_file(path: &Path) -> Result<Option<String>, BootstrapError> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let t = s.trim().to_string();
            Ok((!t.is_empty()).then_some(t))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(BootstrapError::TokenFile { path: path.display().to_string(), reason: e.to_string() }),
    }
}

fn write_token_file(path: &Path, token: &str) -> Result<(), BootstrapError> {
    use std::io::Write;
    let err = |e: std::io::Error| BootstrapError::TokenFile { path: path.display().to_string(), reason: e.to_string() };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(err)?;
    #[cfg(unix)]
    {
        // 文件已存在（空文件）时 mode 不生效，显式收紧。
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(err)?;
    }
    writeln!(f, "{token}").map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    #[tokio::test]
    async fn generates_token_once_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        let first = bootstrap_admin(&store, dir.path(), None).await.unwrap();
        let token = first.generated_token.clone().expect("generated on first run");
        assert_eq!(token.len(), 64);
        let on_disk = std::fs::read_to_string(dir.path().join("admin-token")).unwrap();
        assert_eq!(on_disk.trim(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("admin-token")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let second = bootstrap_admin(&store, dir.path(), None).await.unwrap();
        assert!(second.generated_token.is_none());
        assert_eq!(second.admin.token_hash, first.admin.token_hash);
        assert_eq!(second.admin.account_id, first.admin.account_id);
    }

    #[tokio::test]
    async fn env_token_wins_and_changing_it_resets_admin_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        let a = bootstrap_admin(&store, dir.path(), Some("first-token-value")).await.unwrap();
        assert!(a.generated_token.is_none());
        assert!(!dir.path().join("admin-token").exists());
        store.create_web_session(&a.admin.account_id, "h", "", "", 1, u64::MAX / 2).await.unwrap();
        bootstrap_admin(&store, dir.path(), Some("first-token-value")).await.unwrap();
        assert!(store.find_web_session("h", 2).await.unwrap().is_some());
        let b = bootstrap_admin(&store, dir.path(), Some("second-token-value")).await.unwrap();
        assert_ne!(a.admin.token_hash, b.admin.token_hash);
        assert!(store.find_web_session("h", 2).await.unwrap().is_none());
    }

    #[test]
    fn unwritable_data_dir_fails_clearly() {
        let err = prepare_data_dir(Path::new("/proc/definitely/not/writable")).unwrap_err();
        assert!(err.to_string().contains("cannot use data directory"));
    }
}
