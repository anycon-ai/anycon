//! 首次启动：管理员账号与 admin token。
//!
//! - `ANYCON_ADMIN_TOKEN` 设置了就用它；
//! - 否则读 `<data>/admin-token`；
//! - 都没有就生成 32 字节随机数（hex），以 0600 写入 `<data>/admin-token`，并把明文交给调用方打印一次。
//!
//! bootstrap token 不在库里：登录时直接和它的哈希比较。token 变了（改环境变量重启）会清掉管理员账号
//! 已有的浏览器会话——「改它重启即重置登录」。
//!
//! token 登录进的那个账号（bootstrap 管理员）的 id 记在 `kv.bootstrap_admin_id`，之后每次启动都认它，
//! 不去库里找「最早的管理员」：管理员标记现在可以授予别的账号（`POST /api/admin/accounts/:id/admin`，两种模式都行），
//! 比如 `multi` 跑过一阵再切回 `single`，最早的管理员可能是某个普通用户，token 不能登录到别人的账号里。
//! 这个键还没有时（升级前建的库）：库里有 `admin_token_hash` 说明这是 `single` 已经启动过的库，那时最早的管理员
//! 就是 bootstrap 账号，沿用并记下来（[`pin_bootstrap_admin`]，**每次启动、不管哪种模式**都先做这一步，在任何东西能改管理员之前：
//! 升级后第一次是在 `multi` 下启动的话，管理员可能先被取消、被授予别人，再切回 `single` 就认错账号了）；
//! 否则新建一个 `admin` 账号并记下来（账号和记录在一个事务里写）。记着的账号被删了同样新建。

use std::path::Path;

use crate::app::AdminAuth;
use crate::store::{Account, Store, StoreError};
use crate::util::{hash_token, new_secret, now_ms};

/// `kv` 里记 bootstrap 管理员账号 id 的键。
const KV_BOOTSTRAP_ADMIN_ID: &str = "bootstrap_admin_id";
/// `kv` 里记 bootstrap token 哈希的键（升级前的库只有它能说明 `single` 启动过）。
const KV_ADMIN_TOKEN_HASH: &str = "admin_token_hash";

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

/// 升级：升级前启动过的 `single` 库（有 `admin_token_hash`、还没有 `bootstrap_admin_id`）里，最早的管理员就是 bootstrap 账号，
/// 把它的 id 记下来。**每次启动、两种模式都要先做这一步**（[`crate::run`]），这样升级后第一次启动就把它钉住，
/// 之后管理员标记怎么授予、取消，bootstrap 账号都不会变。已经记过、或者不是升级前的 `single` 库就什么都不做。
pub async fn pin_bootstrap_admin(store: &dyn Store) -> Result<(), StoreError> {
    if store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await?.is_some() || store.kv_get(KV_ADMIN_TOKEN_HASH).await?.is_none() {
        return Ok(());
    }
    if let Some(account) = store.first_admin().await? {
        store.kv_set(KV_BOOTSTRAP_ADMIN_ID, &account.id).await?;
        tracing::info!("recorded the first admin account of this database as the bootstrap admin");
    }
    Ok(())
}

/// bootstrap 管理员账号：记着的那个；没记过就新建（升级前的库已经在 [`pin_bootstrap_admin`] 里记过了）；记着的被删了也新建。
/// 账号在库里不是管理员了（`multi` 期间被取消）就把标记补回来：`single` 里取消不了它。
async fn resolve_admin_account(store: &dyn Store) -> Result<Account, StoreError> {
    pin_bootstrap_admin(store).await?;
    let create = || async { store.create_account_and_record_id("admin", true, KV_BOOTSTRAP_ADMIN_ID, now_ms()).await };
    let Some(id) = store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await? else { return create().await };
    match store.get_account(&id).await? {
        Some(mut account) => {
            if !account.is_admin {
                store.set_account_admin(&account.id, true).await?;
                account.is_admin = true;
                tracing::info!("the bootstrap admin account had lost its admin flag (multi mode); restored");
            }
            Ok(account)
        }
        None => {
            tracing::warn!("the recorded bootstrap admin account no longer exists; creating a new one");
            create().await
        }
    }
}

pub async fn bootstrap_admin(
    store: &dyn Store,
    data_dir: &Path,
    env_token: Option<&str>,
) -> Result<Bootstrapped, BootstrapError> {
    let token_file = data_dir.join("admin-token");
    let admin = resolve_admin_account(store).await?;

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
    let known = store.kv_get(KV_ADMIN_TOKEN_HASH).await?;
    if known.as_deref() != Some(token_hash.as_str()) {
        if known.is_some() {
            let n = store.delete_web_sessions_of_account(&admin.id).await?;
            tracing::info!(sessions_revoked = n, "admin token changed, admin sessions reset");
        }
        store.kv_set(KV_ADMIN_TOKEN_HASH, &token_hash).await?;
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

    #[tokio::test]
    async fn the_bootstrap_account_is_recorded_and_not_confused_with_other_admins() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        // 一个库在 multi 下跑过：有人被授予了管理员，没有 bootstrap 账号，也没启动过 single
        let user = store.create_account_with_email("U", Some("u@example.com"), 1).await.unwrap();
        let crate::store::CreateAccountOutcome::Created(user) = user else { panic!("created") };
        store.set_account_admin(&user.id, true).await.unwrap();
        assert_eq!(store.first_admin().await.unwrap().unwrap().id, user.id);

        let boot = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_ne!(boot.admin.account_id, user.id, "the bootstrap token must not log into an unrelated admin");
        assert_eq!(store.get_account(&boot.admin.account_id).await.unwrap().unwrap().display_name, "admin");
        assert_eq!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(), Some(boot.admin.account_id.as_str()));
        // 再来一遍，还是同一个；U 是更早的管理员也没用
        let again = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_eq!(again.admin.account_id, boot.admin.account_id);
        assert_eq!(store.list_accounts(10, None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_database_from_before_the_record_adopts_its_first_admin() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        // 升级前的 single 库：bootstrap 账号最早，token 哈希在 kv 里；之后有别的账号被授予管理员
        let old_admin = store.create_account("admin", true, 10).await.unwrap();
        let later = store.create_account("later", true, 20).await.unwrap();
        store.kv_set(KV_ADMIN_TOKEN_HASH, &hash_token("a-long-enough-token")).await.unwrap();
        assert!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().is_none());

        let boot = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_eq!(boot.admin.account_id, old_admin.id);
        assert_ne!(boot.admin.account_id, later.id);
        assert_eq!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(), Some(old_admin.id.as_str()));
        assert_eq!(store.list_accounts(10, None).await.unwrap().len(), 2, "nothing new was created");
    }

    #[tokio::test]
    async fn an_upgraded_database_pins_its_bootstrap_admin_at_the_first_start_whatever_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        // 升级前的 single 库：bootstrap 账号最早，token 哈希在 kv 里，没有 bootstrap_admin_id
        let original = store.create_account("admin", true, 10).await.unwrap();
        store.kv_set(KV_ADMIN_TOKEN_HASH, &hash_token("a-long-enough-token")).await.unwrap();

        // 升级后第一次启动是 multi：它不跑 bootstrap_admin，只做 pin
        pin_bootstrap_admin(&store).await.unwrap();
        assert_eq!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(), Some(original.id.as_str()));
        // multi 里原来的管理员被取消，管理员标记授予了别人
        store.set_account_admin(&original.id, false).await.unwrap();
        let later = store.create_account("later", true, 20).await.unwrap();
        assert_eq!(store.first_admin().await.unwrap().unwrap().id, later.id, "the setup: first_admin() moved");
        // pin 不会覆盖已经记下的
        pin_bootstrap_admin(&store).await.unwrap();
        assert_eq!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(), Some(original.id.as_str()));

        // 之后切回 single：还是原来的账号（标记补回来），不是 later
        let boot = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_eq!(boot.admin.account_id, original.id);
        assert!(store.get_account(&original.id).await.unwrap().unwrap().is_admin);
        assert_eq!(store.list_accounts(10, None).await.unwrap().len(), 2, "nothing new was created");
    }

    #[tokio::test]
    async fn pinning_does_nothing_for_a_database_that_never_ran_single_or_already_has_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        // 全新的库 / multi 跑出来的库：没有 admin_token_hash，不钉任何账号（哪怕有被授予的管理员）
        let user = store.create_account("granted", true, 10).await.unwrap();
        pin_bootstrap_admin(&store).await.unwrap();
        assert!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().is_none());
        // 有 admin_token_hash 但一个管理员也没有：没有可钉的，等 single 启动时新建
        store.set_account_admin(&user.id, false).await.unwrap();
        store.kv_set(KV_ADMIN_TOKEN_HASH, "h").await.unwrap();
        pin_bootstrap_admin(&store).await.unwrap();
        assert!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().is_none());
        // 已经记过的不动
        store.kv_set(KV_BOOTSTRAP_ADMIN_ID, "recorded").await.unwrap();
        store.set_account_admin(&user.id, true).await.unwrap();
        pin_bootstrap_admin(&store).await.unwrap();
        assert_eq!(store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(), Some("recorded"));
    }

    #[tokio::test]
    async fn a_recorded_account_that_was_deleted_is_replaced_and_one_that_lost_its_flag_gets_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(None, dir.path()).await.unwrap();
        let first = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        let id = first.admin.account_id.clone();

        // 在 multi 下被取消了管理员：再进 single，标记补回来，还是同一个账号
        store.set_account_admin(&id, false).await.unwrap();
        let second = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_eq!(second.admin.account_id, id);
        assert!(store.get_account(&id).await.unwrap().unwrap().is_admin);

        // 被删了：新建一个并记下来
        let tombstone = crate::store::NewAudit { event: "account_deleted", ..Default::default() };
        assert!(store.delete_account(&id, tombstone, 5).await.unwrap());
        let third = bootstrap_admin(&store, dir.path(), Some("a-long-enough-token")).await.unwrap();
        assert_ne!(third.admin.account_id, id);
        assert!(store.get_account(&third.admin.account_id).await.unwrap().unwrap().is_admin);
        assert_eq!(
            store.kv_get(KV_BOOTSTRAP_ADMIN_ID).await.unwrap().as_deref(),
            Some(third.admin.account_id.as_str())
        );
    }

    // 靠 `/proc` 不可写；Windows 上这个路径落在当前盘上，建得出来
    #[cfg(unix)]
    #[test]
    fn unwritable_data_dir_fails_clearly() {
        let err = prepare_data_dir(Path::new("/proc/definitely/not/writable")).unwrap_err();
        assert!(err.to_string().contains("cannot use data directory"));
    }
}
