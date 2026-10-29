//! `anycon init`：生成 `config.toml` 和 Noise 密钥，创建 workspace 根目录（含 `.trash/`、`.scratch/`）。

use std::path::PathBuf;

use crate::config::{default_config_toml, Paths};
use crate::store::Store;

#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    pub hub: Option<String>,
    pub workspace: Option<String>,
    pub name: Option<String>,
    pub force: bool,
}

#[derive(Debug)]
pub struct InitOutcome {
    pub config_path: PathBuf,
    pub workspace_root: PathBuf,
    pub node_name: String,
    pub reinitialized: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("already initialized at {0}; use --force to re-initialize (this invalidates all paired devices)")]
    AlreadyInitialized(String),
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
}

pub async fn init(paths: &Paths, opts: &InitOptions) -> Result<InitOutcome, InitError> {
    let initialized = paths.config_toml().exists() || paths.node_key().exists();
    if initialized && !opts.force {
        return Err(InitError::AlreadyInitialized(paths.home.display().to_string()));
    }
    if let Some(h) = &opts.hub {
        crate::hub::ws_url(h).map_err(InitError::Invalid)?;
    }
    std::fs::create_dir_all(&paths.home)?;
    crate::sys::restrict_to_owner(&paths.home)?;
    std::fs::create_dir_all(paths.keys_dir())?;
    crate::sys::restrict_to_owner(&paths.keys_dir())?;
    std::fs::create_dir_all(paths.logs_dir())?;

    let ws_text = opts.workspace.clone().unwrap_or_else(|| "~/anycon".into());
    let root = crate::util::expand_tilde(&ws_text);
    let root = if root.is_relative() { std::env::current_dir()?.join(root) } else { root };
    std::fs::create_dir_all(root.join(crate::workspace::TRASH_DIR))?;
    std::fs::create_dir_all(root.join(crate::workspace::SCRATCH_DIR))?;
    let ws_store = root.to_string_lossy().into_owned();

    let node_name = opts.name.clone().unwrap_or_else(crate::config::hostname);
    let text = default_config_toml(opts.hub.as_deref(), opts.name.as_deref(), &ws_store);
    crate::identity::write_private(&paths.config_toml(), &text)?;
    crate::identity::create_node_key(paths)?;

    if initialized {
        // 重新初始化：旧的配对关系和设备身份作废
        for p in [paths.device_token(), paths.device_id(), paths.peers_json()] {
            let _ = std::fs::remove_file(p);
        }
        if paths.db().exists() {
            let store = Store::open(&paths.db()).await?;
            sqlx::query("DELETE FROM peers").execute(store.pool()).await.map_err(crate::store::StoreError::from)?;
            store.close().await;
        }
    }
    Ok(InitOutcome { config_path: paths.config_toml(), workspace_root: root, node_name, reinitialized: initialized })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_creates_layout_and_refuses_twice() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path().join("home"));
        let ws = d.path().join("ws");
        let opts = InitOptions {
            hub: Some("http://127.0.0.1:8080".into()),
            workspace: Some(ws.to_string_lossy().into()),
            name: Some("box".into()),
            force: false,
        };
        let out = init(&paths, &opts).await.unwrap();
        assert!(!out.reinitialized);
        assert!(ws.join(".trash").is_dir() && ws.join(".scratch").is_dir());
        let mode = crate::sys::mode_bits(&std::fs::metadata(paths.node_key()).unwrap());
        assert!(mode.is_none_or(|m| m == 0o600), "{mode:?}");
        let cfg = crate::config::Config::load(&paths).unwrap();
        assert_eq!(cfg.hub.url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(cfg.node_name(), "box");
        let key1 = std::fs::read_to_string(paths.node_key()).unwrap();
        assert!(matches!(init(&paths, &opts).await, Err(InitError::AlreadyInitialized(_))));
        // --force：换新密钥，清掉身份
        std::fs::write(paths.device_token(), "tok").unwrap();
        let out = init(&paths, &InitOptions { force: true, ..opts.clone() }).await.unwrap();
        assert!(out.reinitialized);
        assert_ne!(std::fs::read_to_string(paths.node_key()).unwrap(), key1);
        assert!(!paths.device_token().exists());
        // 坏的 hub URL
        let paths2 = Paths::new(d.path().join("home2"));
        assert!(matches!(
            init(&paths2, &InitOptions { hub: Some("not a url".into()), ..Default::default() }).await,
            Err(InitError::Invalid(_))
        ));
    }
}
