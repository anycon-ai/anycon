//! daemon 的装配与生命周期：单实例锁、配置、存储、环境、各组件、后台任务、优雅退出。

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
use crate::preview::{Preview, PreviewDeps};
use crate::rpc::Rpc;
use crate::sessions::{SessionDeps, Sessions};
use crate::store::{Store, StoreError};
use crate::util::now_ms;
use crate::workspace::{WatchGuard, Workspace};
use crate::worktree::WorktreeMgr;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// 持锁进程的 pid；Windows 的文件锁是强制的，读不到时为空。
    #[error("another anycon daemon is already running{}", if .0.is_empty() { String::new() } else { format!(" (pid {})", .0) })]
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
    /// 到 hub 的连接多久收不到任何帧就重连（默认 [`crate::hub::HUB_SILENCE_TIMEOUT`]，测试里调短）。
    pub hub_silence_timeout: Duration,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            adapters: None,
            hub: true,
            socket: true,
            login_shell: None,
            lock: true,
            env_extra: Vec::new(),
            hub_silence_timeout: crate::hub::HUB_SILENCE_TIMEOUT,
        }
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
    /// dev server 预览（ADR-39）：票据、端口探测、隧道。
    pub preview: Arc<Preview>,
    /// 账号内接入引擎（ADR-36）；`peers.enroll` 是同一个。
    pub enroll: Arc<Enroll>,
    pub hub_state: Arc<HubState>,
    pub outbox: Arc<HubOutbox>,
    pub identity: Arc<Identity>,
    pub started_at_ms: u64,
    pub started: Instant,
    pub shutdown_tx: watch::Sender<bool>,
}

/// 单实例锁：`daemon.lock` 上的文件锁（unix `flock`，Windows `LockFileEx`）。进程退出（含崩溃）自动释放。
pub struct DaemonLock {
    _file: std::fs::File,
}

impl DaemonLock {
    pub fn acquire(paths: &Paths) -> Result<DaemonLock, DaemonError> {
        use std::io::{Read, Seek, Write};
        let mut f =
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(paths.lock())?;
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // Windows 的锁是强制的，读不到持锁进程写的 pid，这时 pid 为空
                let mut pid = String::new();
                let _ = f.read_to_string(&mut pid);
                return Err(DaemonError::AlreadyRunning(pid.trim().to_string()));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
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
        crate::util::warn_err("restrict the home directory to its owner", crate::sys::restrict_to_owner(&paths.home));
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
            base.extend(crate::env::canonical_env(opts.env_extra.iter().cloned()));
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
        let preview = Preview::new(PreviewDeps {
            store: store.clone(),
            config: config.clone(),
            identity: identity.clone(),
            outbox: outbox.clone(),
            audit: audit.clone(),
            sessions: sessions.clone(),
            peers: peers.clone(),
            env: env.base.clone(),
        });
        peers.bind_preview(&preview);
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
            preview: preview.clone(),
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
        tasks.extend(preview.spawn_background());
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
                preview,
                agents,
                wake: Arc::new(Notify::new()),
                silence_timeout: opts.hub_silence_timeout,
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
        self.ctx.preview.shutdown().await;
        self.ctx.sessions.shutdown_all().await;
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
        if let Some(p) = &self.socket_path {
            crate::local::cleanup(p);
        }
        self.ctx.store.close().await;
    }

    /// 等 SIGINT / SIGTERM（Windows：Ctrl+C、Ctrl+Break、关闭控制台、注销、关机，以及 `anycon service stop` 发的停止事件），
    /// 然后优雅退出。
    pub async fn run_until_signal(self) {
        #[cfg(windows)]
        let stop = match stop_event::create(&self.ctx.paths) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "cannot create the stop event; `anycon service stop` will terminate the process");
                None
            }
        };
        #[cfg(windows)]
        tokio::select! {
            _ = wait_for_stop_signal() => {}
            _ = async { match &stop { Some(e) => e.wait().await, None => std::future::pending().await } } => {
                tracing::info!("stop requested");
            }
        }
        #[cfg(unix)]
        wait_for_stop_signal().await;
        self.shutdown().await;
    }
}

#[cfg(unix)]
async fn wait_for_stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async { if let Some(s) = term.as_mut() { s.recv().await } else { std::future::pending().await } } => {}
        _ = async { if let Some(s) = int.as_mut() { s.recv().await } else { std::future::pending().await } } => {}
    }
}

/// 控制台的关闭 / 注销 / 关机事件只给几秒钟（之后系统强制结束进程）；agent 都在 job 里，daemon 被强制结束时也会被一起清掉。
#[cfg(windows)]
async fn wait_for_stop_signal() {
    use tokio::signal::windows;
    let mut c = windows::ctrl_c().ok();
    let mut brk = windows::ctrl_break().ok();
    let mut close = windows::ctrl_close().ok();
    let mut logoff = windows::ctrl_logoff().ok();
    let mut shutdown = windows::ctrl_shutdown().ok();
    macro_rules! recv {
        ($s:expr) => {
            async {
                match $s.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }
        };
    }
    tokio::select! {
        _ = recv!(c) => {}
        _ = recv!(brk) => {}
        _ = recv!(close) => {}
        _ = recv!(logoff) => {}
        _ = recv!(shutdown) => {}
    }
}

/// Windows 上给 `anycon service stop` 用的停止事件：命名事件 `Local\anycon-stop-<hash>`（和命名管道同一个 hash），DACL 只允许
/// 当前用户。计划任务拉起的 daemon 没有可见的控制台，收不到 Ctrl+C，所以用它请求优雅退出；`Local\` 是登录会话内的命名空间，
/// 别的用户看不到也抢不了这个名字。
#[cfg(windows)]
pub mod stop_event {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
    };

    use crate::config::Paths;
    use crate::sys::win::{current_user_sid, wide, SecurityDescriptor};

    pub fn name(paths: &Paths) -> String {
        let pipe = paths.sock();
        let hash = pipe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!(r"Local\{}", hash.replace("anycon-", "anycon-stop-"))
    }

    pub struct StopEvent(std::sync::Arc<OwnedHandle>);

    pub fn create(paths: &Paths) -> std::io::Result<StopEvent> {
        let sid = current_user_sid()?;
        let sd = SecurityDescriptor::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})"))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_ptr(),
            bInheritHandle: 0,
        };
        let w = wide(std::ffi::OsStr::new(&name(paths)));
        // SAFETY: `sa`、`sd`、`w` 在调用期间有效；手动复位，初始未触发。
        let h = unsafe { CreateEventW(&sa, 1, 0, w.as_ptr()) };
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: 刚创建的有效句柄。
        Ok(StopEvent(std::sync::Arc::new(unsafe { OwnedHandle::from_raw_handle(h) })))
    }

    impl StopEvent {
        /// 等事件被触发（在阻塞线程里等）。
        pub async fn wait(&self) {
            let h = self.0.clone();
            let _ = tokio::task::spawn_blocking(move || {
                // SAFETY: 句柄由 Arc 持有，等待期间有效。
                unsafe { WaitForSingleObject(h.as_raw_handle(), INFINITE) == WAIT_OBJECT_0 }
            })
            .await;
        }
    }

    /// 请求 daemon 优雅退出。事件不存在（daemon 没在跑，或者是不支持它的旧版本）返回 `Ok(false)`。
    pub fn request(paths: &Paths) -> std::io::Result<bool> {
        let w = wide(std::ffi::OsStr::new(&name(paths)));
        // SAFETY: `w` 以 NUL 结尾。
        let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) };
        if h.is_null() {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Ok(false);
            }
            return Err(e);
        }
        // SAFETY: 刚打开的有效句柄。
        let h = unsafe { OwnedHandle::from_raw_handle(h) };
        // SAFETY: 句柄有效且带 EVENT_MODIFY_STATE。
        if unsafe { SetEvent(h.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn request_wakes_the_waiter() {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path());
            assert!(name(&paths).starts_with(r"Local\anycon-stop-"), "{}", name(&paths));
            assert!(!request(&paths).unwrap(), "nobody listens yet");
            let ev = create(&paths).unwrap();
            let waiter = tokio::spawn(async move { ev.wait().await });
            assert!(request(&paths).unwrap());
            tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await.unwrap().unwrap();
        }
    }
}

/// `anycon daemon`：启动并运行到收到信号。
pub async fn run(paths: Paths) -> Result<(), DaemonError> {
    let d = Daemon::start(paths, DaemonOptions::default()).await?;
    d.run_until_signal().await;
    Ok(())
}
