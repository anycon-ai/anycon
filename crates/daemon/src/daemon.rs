//! daemon 的装配与生命周期：单实例锁、配置、存储、环境、各组件、后台任务、优雅退出。

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_agent_api::Adapter;
use tokio::sync::{watch, Notify};

use crate::agents::Agents;
use crate::audit::Audit;
use crate::blobs::Blobs;
use crate::clients::Clients;
use crate::config::{ConfigError, ConfigStore, Paths};
use crate::enroll::{Enroll, EnrollDeps};
use crate::env::DaemonEnv;
use crate::hub::{HubLink, HubOutbox, HubState};
use crate::identity::{Identity, IdentityError};
use crate::notify::Notifier;
use crate::peers::Peers;
use crate::rpc::Rpc;
use crate::sessions::{SessionDeps, Sessions};
use crate::store::{Store, StoreError};
use crate::util::now_ms;
use crate::workspace::{WatchGuard, Workspace};
use crate::worktree::WorktreeMgr;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another anycon daemon is already running (pid {0})")]
    AlreadyRunning(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("attachments: {0}")]
    Blobs(#[from] crate::blobs::BlobError),
    #[error("{0}")]
    Other(String),
}

impl DaemonError {
    /// 没初始化（缺配置 / 密钥）。
    pub fn is_not_initialized(&self) -> bool {
        matches!(self, DaemonError::Identity(IdentityError::NotInitialized(_)))
    }
}

pub struct DaemonOptions {
    /// `None` = `anycon_adapters::all()`。
    pub adapters: Option<Vec<Arc<dyn Adapter>>>,
    /// 连接 hub（默认 true）。
    pub hub: bool,
    /// 开本机 socket（默认 true）。
    pub socket: bool,
    /// 覆盖 `agents.env_from_login_shell`（测试用）。
    pub login_shell: Option<bool>,
    /// 取单实例锁（默认 true）。
    pub lock: bool,
    /// 叠加到 daemon 环境上的变量（测试用：给 git 一份不读开发机 `~/.gitconfig` 的配置、在 `PATH` 前面放一个假的 `gh`）。
    pub env_extra: Vec<(String, String)>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self { adapters: None, hub: true, socket: true, login_shell: None, lock: true, env_extra: Vec::new() }
    }
}

/// 共享上下文：RPC 调度器和后台任务都持有它。
pub struct Ctx {
    pub paths: Paths,
    pub config: Arc<ConfigStore>,
    pub store: Store,
    pub env: Arc<DaemonEnv>,
    pub clients: Arc<Clients>,
    pub agents: Arc<Agents>,
    pub sessions: Arc<Sessions>,
    pub blobs: Arc<Blobs>,
    pub workspace: Arc<Workspace>,
    pub worktrees: Arc<WorktreeMgr>,
    pub notifier: Arc<Notifier>,
    pub audit: Audit,
    pub peers: Arc<Peers>,
    /// 账号内接入引擎（ADR-36）；`peers.enroll` 是同一个。
    pub enroll: Arc<Enroll>,
    pub hub_state: Arc<HubState>,
    pub outbox: Arc<HubOutbox>,
    pub identity: Arc<Identity>,
    pub started_at_ms: u64,
    pub started: Instant,
    pub shutdown_tx: watch::Sender<bool>,
}

/// 单实例锁：`daemon.lock` 上的 flock。进程退出（含崩溃）自动释放。
pub struct DaemonLock {
    _file: std::fs::File,
}

impl DaemonLock {
    pub fn acquire(paths: &Paths) -> Result<DaemonLock, DaemonError> {
        use std::io::{Read, Seek, Write};
        let mut f =
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(paths.lock())?;
        // SAFETY: flock 只操作我们刚打开的 fd。
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let mut pid = String::new();
            let _ = f.read_to_string(&mut pid);
            return Err(DaemonError::AlreadyRunning(pid.trim().to_string()));
        }
        f.set_len(0)?;
        f.rewind()?;
        write!(f, "{}", std::process::id())?;
        Ok(DaemonLock { _file: f })
    }
}

pub struct Daemon {
    pub ctx: Arc<Ctx>,
    pub rpc: Arc<Rpc>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _lock: Option<DaemonLock>,
    _watch: Option<WatchGuard>,
    socket_path: Option<std::path::PathBuf>,
}

impl Daemon {
    pub async fn start(paths: Paths, opts: DaemonOptions) -> Result<Daemon, DaemonError> {
        std::fs::create_dir_all(&paths.home)?;
        crate::util::warn_err(
            "restrict the home directory to its owner",
            std::fs::set_permissions(&paths.home, std::os::unix::fs::PermissionsExt::from_mode(0o700)),
        );
        let lock = if opts.lock { Some(DaemonLock::acquire(&paths)?) } else { None };
        let identity = Arc::new(Identity::load(&paths)?);
        let config = ConfigStore::load(&paths)?;
        let cfg = config.get();
        let store = Store::open(&paths.db()).await?;
        crate::util::warn_err("mark interrupted jobs as failed", store.jobs_fail_running(now_ms()).await);
        let login_shell = opts.login_shell.unwrap_or(cfg.agents.env_from_login_shell);
        let mut resolved = crate::env::resolve(login_shell, std::time::Duration::from_secs(15)).await;
        if !opts.env_extra.is_empty() {
            let mut base = (*resolved.base).clone();
            base.extend(opts.env_extra.iter().cloned());
            resolved.base = Arc::new(base);
        }
        let env = Arc::new(resolved);
        if let Some(w) = &env.warning {
            tracing::warn!("{w}");
        }
        tracing::info!(source = env.source.as_str(), "daemon environment resolved");

        let clients = Clients::new();
        let audit = Audit::new(store.clone());
        let notifier = Notifier::new(config.clone(), clients.clone());
        let workspace =
            Workspace::new(&cfg.workspace_root(), config.clone(), store.clone(), clients.clone(), env.base.clone())
                .await?;
        let worktrees = WorktreeMgr::new(
            crate::gitops::Git::new(env.base.clone()),
            store.clone(),
            workspace.clone(),
            config.clone(),
        )
        .await;
        let blobs = Blobs::open(&paths, store.clone(), config.clone()).await?;
        let adapters = opts.adapters.unwrap_or_else(anycon_adapters::all);
        let agents =
            Agents::new(adapters, config.clone(), env.clone(), store.clone(), clients.clone(), paths.clone()).await;
        let sessions = Sessions::new(SessionDeps {
            store: store.clone(),
            clients: clients.clone(),
            agents: agents.clone(),
            workspace: workspace.clone(),
            config: config.clone(),
            env: env.clone(),
            notifier: notifier.clone(),
            worktrees: worktrees.clone(),
            audit: audit.clone(),
            blobs: blobs.clone(),
        })
        .await;
        // 发现关联项目被换成符号链接 / 不再允许时（workspace.list、fs.*、开 turn 前的复核…），收掉它所有会话的进程
        {
            let sessions = Arc::downgrade(&sessions);
            workspace.set_problem_hook(Arc::new(move |project| {
                if let Some(s) = sessions.upgrade() {
                    s.release_project_soon(project);
                }
            }));
        }
        let outbox = HubOutbox::new();
        let hub_state = HubState::new();
        let enroll = Enroll::new(EnrollDeps {
            store: store.clone(),
            config: config.clone(),
            outbox: outbox.clone(),
            clients: clients.clone(),
            audit: audit.clone(),
            notifier: notifier.clone(),
            identity: identity.clone(),
        });
        let peers = Peers::new(
            store.clone(),
            paths.clone(),
            identity.clone(),
            outbox.clone(),
            clients.clone(),
            audit.clone(),
            config.clone(),
            enroll.clone(),
        )
        .await;
        notifier.set_out(peers.clone());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ctx = Arc::new(Ctx {
            paths: paths.clone(),
            config: config.clone(),
            store,
            env,
            clients,
            agents: agents.clone(),
            sessions: sessions.clone(),
            blobs,
            workspace: workspace.clone(),
            worktrees,
            notifier,
            audit,
            peers: peers.clone(),
            enroll: enroll.clone(),
            hub_state: hub_state.clone(),
            outbox: outbox.clone(),
            identity: identity.clone(),
            started_at_ms: now_ms(),
            started: Instant::now(),
            shutdown_tx,
        });
        let rpc = Rpc::new(ctx.clone());

        let mut tasks = vec![
            config.spawn_watcher(),
            agents.spawn_scheduler(),
            sessions.spawn_sweeper(),
            enroll.spawn_sweeper(),
            enroll.spawn_config_watcher(),
        ];
        // 出站保留缓冲里陈旧的信封（hub 早就不会再退回它们了）定期放掉，空闲的对端不占内存
        {
            let outbox = outbox.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    outbox.trim();
                }
            }));
        }
        // log.level 热加载
        {
            let mut rx = config.subscribe();
            tasks.push(tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    crate::logging::set_level(&rx.borrow().log.level);
                }
            }));
        }
        let watch_guard = match workspace.spawn_watcher() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(error = %e, "workspace watcher unavailable");
                None
            }
        };
        // 启动时清一次过期的回收站 / 会话
        {
            let (s, w) = (sessions.clone(), workspace.clone());
            tasks.push(tokio::spawn(async move {
                w.cleanup_trash().await;
                s.cleanup().await;
            }));
        }
        let mut socket_path = None;
        if opts.socket {
            let listener = crate::local::bind(&paths.sock())?;
            socket_path = Some(paths.sock());
            let (c, r, sd) = (ctx.clone(), rpc.clone(), shutdown_rx.clone());
            tasks.push(tokio::spawn(async move { crate::local::serve(listener, c, r, sd).await }));
        }
        if opts.hub {
            let link = Arc::new(HubLink {
                config: config.clone(),
                identity,
                outbox,
                state: hub_state,
                peers,
                agents,
                wake: Arc::new(Notify::new()),
            });
            tasks.extend(link.spawn(rpc.clone(), shutdown_rx));
        }
        tracing::info!(home = %paths.home.display(), workspace = %workspace.root().display(), version = env!("CARGO_PKG_VERSION"), "daemon started");
        Ok(Daemon { ctx, rpc, tasks, _lock: lock, _watch: watch_guard, socket_path })
    }

    /// 优雅退出：停止接受新连接，先让 agent 收尾（SIGINT → 5 秒 → SIGKILL 由 adapter 负责），再关库。
    pub async fn shutdown(mut self) {
        tracing::info!("daemon shutting down");
        let _ = self.ctx.shutdown_tx.send(true);
        self.ctx.sessions.shutdown_all().await;
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
        if let Some(p) = &self.socket_path {
            let _ = std::fs::remove_file(p);
        }
        self.ctx.store.close().await;
    }

    /// 等 SIGINT / SIGTERM，然后优雅退出。
    pub async fn run_until_signal(self) {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        tokio::select! {
            _ = async { if let Some(s) = term.as_mut() { s.recv().await } else { std::future::pending().await } } => {}
            _ = async { if let Some(s) = int.as_mut() { s.recv().await } else { std::future::pending().await } } => {}
        }
        self.shutdown().await;
    }
}

/// `anycon daemon`：启动并运行到收到信号。
pub async fn run(paths: Paths) -> Result<(), DaemonError> {
    let d = Daemon::start(paths, DaemonOptions::default()).await?;
    d.run_until_signal().await;
    Ok(())
}
