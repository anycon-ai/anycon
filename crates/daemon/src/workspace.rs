//! Workspace：文件夹即项目（docs/spec/daemon.md § 10、architecture.md § 6），外加关联的本机已有目录（ADR-33）。
//!
//! 根目录下每个一级子文件夹是一个项目，元数据全部推导。所有路径操作先 `canonicalize` 再做前缀检查，
//! 符号链接指向根外的一律拒绝。回收站 `.trash/<name>-<timestamp>/`，临时会话目录 `.scratch/<session-id>/`。
//!
//! **多个根**（ADR-45）：主根（`workspace.root`）放 `.scratch/`，新项目缺省建在这里；`workspace.extra_roots` 里的每个额外根下的一级
//! 子文件夹同样是项目。项目名在所有根之间全局唯一，同名时 关联项目 > 主根 > 额外根（按配置顺序），输的那个不出现在列表里；
//! 所以 `fs.*` 的 `<项目名>/<相对路径>` 不需要知道项目在哪个根。每个根有自己的 `.trash/`（`rename` 不跨文件系统），改名只在同一个根内。
//! 根的集合在启动时定下来（[`crate::wsroots`]），之后不变。
//!
//! **锁顺序**：命名空间锁 `ns_lock`（link / unlink / create / clone / rename / restore / promote_scratch）在前，
//! 会话锁在后——`unlink` 持着 `ns_lock` 调用方传进来的 `busy` 回调，回调会逐个锁会话；所以任何代码都不能持有会话锁再来取
//! `ns_lock`（`Sessions::promote` 为此拆成三步）。`ns_lock` 之内不要等会话的事件。
//!
//! 另一类项目是 `workspace.link` 关联的目录（`kind = linked`）：只在 SQLite `linked_projects` 里记「名字 → canonical 路径」，
//! 内存里留一份（`project_path` / `resolve_rel` 是同步函数）。它和根目录下的项目共用一个命名空间（关联优先），
//! 项目的边界是关联目录本身；anycon 永远不移动、不删除关联目录里的任何东西。可关联 / 可浏览的范围是
//! `workspace.link_roots`（每次调用读最新配置）。「能关联哪些目录」和「使用已有关联时的复核」的规则在
//! [`crate::linkrules`]（隐藏目录、内置拒绝表、绝对拒绝；canonicalize 之后必须等于登记的路径）。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anycon_protocol::{
    methods, BrowseEntry, ErrorCode, FsEntry, FsListResult, FsReadEncoding, FsReadResult, GitMeta, JobDone,
    JobProgress, LinkBlock, Project, ProjectKind, RenamedProject, RpcError, TrashEntry, WorkspaceBrowseResult,
    WorkspaceChanged, WorkspaceListResult, WorkspaceRootInfo,
};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{oneshot, watch};

use crate::clients::Clients;
use crate::config::ConfigStore;
use crate::fsopen::OpenError;
use crate::linkrules::{
    check_path_text, deny_table_with, expand_with_home, fold_path, lexical_clean, name_key, nfc, permission_message,
    tcc_table, LinkCtx, LinkRoots, LinkedProblem, PathAliases,
};
use crate::store::{JobRow, LinkedRow, Store};
use crate::util::{fmt_compact, now_ms, parse_compact};

pub const TRASH_DIR: &str = ".trash";
pub const SCRATCH_DIR: &str = ".scratch";
/// 有缓存（哪怕过期）时 `workspace.list` 前台最多等多久，超了先返回缓存的旧值。
const GIT_FRONT_BUDGET: Duration = Duration::from_millis(50);
/// 没有任何缓存（冷启动）时前台最多等多久（每个项目各自算，互相并行）；超了先不带 `git` 返回，算完之后发
/// `workspace.changed { updated }`。
const GIT_COLD_BUDGET: Duration = Duration::from_secs(3);
/// 冷缓存超时的项目算完之后，`workspace.changed { updated }` 等这么久，把同一时段算完的合并成一条。
const GIT_UPDATED_DEBOUNCE: Duration = Duration::from_millis(200);
const GIT_CACHE_TTL: Duration = Duration::from_secs(5);
const GIT_HARD_TIMEOUT: Duration = Duration::from_secs(5);
const DEBOUNCE: Duration = Duration::from_millis(500);
pub const FS_READ_DEFAULT: u64 = 1024 * 1024;
pub const FS_READ_MAX: u64 = 10 * 1024 * 1024;
/// `fs.read` 的 base64 模式一次最多回多少原始字节（和 `blob.get` 一样 512 KiB）：设备到 daemon 是一条有序的传输，
/// 一个大响应会让别的消息（事件、审批、「停止」的回复）排在它后面，所以大文件分块读（`offset`）。
pub const FS_READ_CHUNK: u64 = 512 * 1024;
/// `workspace.browse` 最多返回的子目录数。
pub const BROWSE_MAX_ENTRIES: usize = 1000;
/// `workspace.browse` 最多检查多少个候选条目（一个塞满文件的目录不能拖住 daemon）；超过就当作被截断。
const BROWSE_MAX_EXAMINED: usize = 20_000;

type RpcResult<T> = Result<T, RpcError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

/// [`Workspace::new`] 的错误：建主根的目录失败，或 `workspace.extra_roots` 的配置错误（daemon 拒绝启动）。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Roots(#[from] crate::wsroots::RootsError),
}

/// 一个 workspace 根（ADR-45）。`Workspace` 里下标 0 是主根。
#[derive(Debug, Clone)]
pub struct WsRoot {
    /// 可用的根是 canonical 路径；不可用的根是 `~` 展开后的配置值。
    pub path: PathBuf,
    /// 主根：放 `.scratch/`，新项目缺省建在这里。
    pub primary: bool,
    /// 启动时是一个存在的目录。不可用的根不扫描、不监听、没有 `.trash`、不能新建项目。
    pub active: bool,
    /// 配置里的原样。
    pub configured: String,
}

/// 项目名校验：不含 `/` 和 NUL、不以 `.` 开头、≤ 255 字节、不是 `.trash` / `.scratch`。
pub fn validate_name(name: &str) -> RpcResult<()> {
    if name.is_empty() {
        return Err(err(ErrorCode::Invalid, "project name is empty"));
    }
    if name.len() > 255 {
        return Err(err(ErrorCode::Invalid, "project name is longer than 255 bytes"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(err(ErrorCode::Invalid, "project name must not contain '/' or NUL"));
    }
    if name.starts_with('.') {
        return Err(err(ErrorCode::Invalid, "project name must not start with '.'"));
    }
    if name == TRASH_DIR || name == SCRATCH_DIR {
        return Err(err(ErrorCode::Invalid, "reserved project name"));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(err(ErrorCode::Invalid, "project name contains control characters"));
    }
    Ok(())
}

/// 克隆 URL 只允许 `https://`、`ssh://`、`git@host:path`。
pub fn validate_clone_url(url: &str) -> RpcResult<()> {
    let bad = |m: &str| Err(err(ErrorCode::Invalid, format!("unsupported clone url: {m}")));
    if url.is_empty() || url.len() > 2048 {
        return bad("empty or too long");
    }
    if url.starts_with('-') || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("illegal characters");
    }
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("ssh://")) {
        if rest.is_empty() || rest.starts_with('/') {
            return bad("missing host");
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            if !host.is_empty() && !path.is_empty() && !host.contains('/') {
                return Ok(());
            }
        }
        return bad("expected git@host:path");
    }
    bad("only https://, ssh:// and git@host:path are allowed")
}

/// 从克隆 URL 推出默认项目名。
pub fn name_from_url(url: &str) -> String {
    let tail = url.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or("");
    tail.strip_suffix(".git").unwrap_or(tail).to_string()
}

/// 发现关联项目 `Moved` / `NotAllowed` 时的回调（参数是项目名）。
pub type ProblemHook = Arc<dyn Fn(String) + Send + Sync>;

/// `link_roots` 一项的判定。
enum RootJudgement {
    /// 可用（canonical 路径；路径里没有任何一段是符号链接，大小写 / NFC 写法和盘上不同不算）。
    Usable(PathBuf),
    /// 目录不存在（或不是目录）：不 warn，但要让客户端看得到原因。
    Missing,
    /// 经过符号链接 / 展开不了 / HOME 变了：整项忽略，附原因（warn 和 `config set` 的输出用）和不带真实路径的简短说法（给配对设备的错误消息用）。
    Ignored { reason: String, brief: &'static str },
}

/// `link_roots` 里一个不能用的项（被忽略的，或者目录不存在的）和原因。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IgnoredRoot {
    /// 配置里的原样。
    pub entry: String,
    /// 完整的原因（本机管理员看：`config set` 的输出、daemon 日志）。
    pub reason: String,
    /// 不带路径的简短说法（`workspace.link` / `browse` 的错误消息里给配对设备看）。
    pub brief: String,
    /// 只是目录不存在（不算「被忽略」，不 warn）。
    pub missing: bool,
}

/// HOME：daemon 启动时解析并固定下来。进程的 HOME 环境变量不会变，但它可能是一个链接，链接事后被改指向别处时，
/// `~`、拒绝表、TCC 表仍然用启动时的 canonical 路径。
#[derive(Debug, Clone)]
struct HomeDir {
    raw: PathBuf,
    canon: PathBuf,
}

/// 会话记着的位置和现在的位置比较的结果（[`Workspace::check_loc`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocCheck {
    /// 同一个目录。
    Same,
    /// 同一个项目、只是身份信息要重新记（整个 workspace 根搬了盘，或旧版本记的位置没有 inode）：接受，并把这个位置记下来。
    Refresh(String),
    /// 另一个目录：拒绝续接。
    Different,
}

/// `folder:<名字>#<dev>:<ino>`（旧版本只有 `folder:<名字>`）。名字里可以有 `#`，所以只认最后一个 `#` 后面是 `数字:数字` 的情形。
fn parse_folder_loc(loc: &str) -> Option<(&str, Option<(u64, u64)>)> {
    let rest = loc.strip_prefix("folder:")?;
    if let Some((name, id)) = rest.rsplit_once('#') {
        if let Some((dev, ino)) = id.split_once(':') {
            if let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) {
                return Some((name, Some((dev, ino))));
            }
        }
    }
    Some((rest, None))
}

struct JobHandle {
    cancel: oneshot::Sender<()>,
}

/// 守护 watcher 的 RAII。drop 即停止。
pub struct WatchGuard {
    _watcher: RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Workspace {
    /// 所有根：下标 0 是主根，后面是额外根（配置顺序）。启动时定下来，之后不变。
    roots: Vec<WsRoot>,
    config: Arc<ConfigStore>,
    store: Store,
    clients: Arc<Clients>,
    env: Arc<HashMap<String, String>>,
    /// 启动时固定下来的 HOME（词法 + canonical），没有可用的 HOME 时为 `None`。
    home: Option<HomeDir>,
    /// 测试用：额外的 firmlink（数据卷前缀, 系统卷前缀），模拟 macOS 的 `/System/Volumes/Data`。
    #[cfg(any(test, feature = "testing"))]
    test_firmlinks: RwLock<Vec<(PathBuf, PathBuf)>>,
    git: Arc<GitMetaSvc>,
    jobs: Mutex<HashMap<String, JobHandle>>,
    /// 目录名 → 目录身份 `(dev, ino)`，用来在重新扫描时识别「改名」（含所有可用根下的项目，不含被盖住的同名文件夹）。
    /// 身份带设备号：不同根可能在不同的盘上，只比 inode 会把别的根里碰巧同号的目录当成改名。
    snapshot: Mutex<HashMap<String, (u64, u64)>>,
    /// 关联的项目：名字（NFC）→ 登记的目录（link 时 canonicalize 过）。和 `linked_projects` 表保持一致。
    linked: RwLock<BTreeMap<String, PathBuf>>,
    /// 任何一个可用根所在的文件系统不区分大小写（APFS 默认）：项目名（全局一个命名空间）按小写比较。
    case_insensitive: std::sync::atomic::AtomicBool,
    /// 已经为「被更高优先级的项目盖住的同名文件夹」记过 warn 的名字（每个名字每次进程只记一次）。
    shadow_warned: Mutex<HashSet<String>>,
    /// 已经为某个 `link_roots` 项记过 warn 的原因（每个原因每个进程只记一次，免得每次请求都刷屏；判定本身每次请求都重做）。
    roots_warned: Mutex<HashSet<String>>,
    /// 发现关联项目 `Moved` / `NotAllowed` 时的回调（daemon 接线到 `Sessions::release_project_soon`；
    /// 那边把同一个项目的重复通知合并）。
    problem_hook: RwLock<Option<ProblemHook>>,
    /// 串行化「占用项目名」的操作（link / create / clone / rename / restore / promote），让命名空间检查和落盘是一个整体。
    ns_lock: tokio::sync::Mutex<()>,
}

impl Workspace {
    /// `root` 是主根（调用方已展开 `~`）；额外根取自 `config` 的 `workspace.extra_roots`，按 [`crate::wsroots`] 的规则校验
    /// （配置错误 → [`WorkspaceError::Roots`]），不存在的跳过并 warn。
    pub async fn new(
        root: &Path,
        config: Arc<ConfigStore>,
        store: Store,
        clients: Arc<Clients>,
        env: Arc<HashMap<String, String>>,
    ) -> Result<Arc<Self>, WorkspaceError> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join(TRASH_DIR))?;
        std::fs::create_dir_all(root.join(SCRATCH_DIR))?;
        let root = crate::sys::canonicalize(root)?;
        let roots = {
            let cfg = config.get();
            let planned = crate::wsroots::plan_with(
                &cfg.workspace.root,
                &root,
                &cfg.workspace.extra_roots,
                &config.paths().home,
            )?;
            let mut roots = Vec::with_capacity(planned.len());
            for p in planned {
                if let Some(why) = &p.unavailable {
                    tracing::warn!(
                        root = %p.path.display(),
                        "workspace.extra_roots: skipping {:?}: {why}; its projects are not listed. Restart the daemon after fixing it",
                        p.configured
                    );
                } else if !p.primary {
                    // 每个根有自己的 `.trash/`（rename 不跨文件系统）；建不了（只读的盘）不拦：项目照常列出，trash 时才报错
                    if let Err(e) = std::fs::create_dir_all(p.path.join(TRASH_DIR)) {
                        tracing::warn!(root = %p.path.display(), error = %e, "cannot create .trash in this workspace root");
                    }
                }
                roots.push(WsRoot {
                    path: if p.primary { root.clone() } else { p.path },
                    primary: p.primary,
                    active: p.unavailable.is_none(),
                    configured: p.configured,
                });
            }
            roots
        };
        let linked: BTreeMap<String, PathBuf> = match store.list_linked().await {
            Ok(rows) => rows.into_iter().map(|r| (r.name, PathBuf::from(r.path))).collect(),
            Err(e) => {
                tracing::error!(error = %e, "cannot load the linked projects");
                BTreeMap::new()
            }
        };
        let case_insensitive = roots.iter().filter(|r| r.active).any(|r| probe_case_insensitive(&r.path));
        let home = anycon_agent_api::env_home(&env).map(|h| {
            let raw = PathBuf::from(h);
            let canon = crate::sys::canonicalize(&raw).unwrap_or_else(|_| raw.clone());
            HomeDir { raw, canon }
        });
        let git = Arc::new(GitMetaSvc::new(env.clone(), clients.clone()));
        let ws = Arc::new(Workspace {
            roots,
            config,
            store,
            clients,
            env,
            home,
            #[cfg(any(test, feature = "testing"))]
            test_firmlinks: RwLock::new(Vec::new()),
            git,
            jobs: Mutex::new(HashMap::new()),
            snapshot: Mutex::new(HashMap::new()),
            linked: RwLock::new(linked),
            case_insensitive: std::sync::atomic::AtomicBool::new(case_insensitive),
            shadow_warned: Mutex::new(HashSet::new()),
            roots_warned: Mutex::new(HashSet::new()),
            problem_hook: RwLock::new(None),
            ns_lock: tokio::sync::Mutex::new(()),
        });
        *ws.snapshot.lock().unwrap() = ws.scan_names();
        // 启动时和配置变化（热加载 / `config set`）时对被忽略的 link_roots 项 warn（每个原因一次）
        ws.warn_ignored_roots();
        ws.spawn_roots_watcher();
        Ok(ws)
    }

    /// 主根。
    pub fn root(&self) -> &Path {
        &self.roots[0].path
    }

    /// 所有根（主根在最前）。
    pub fn roots(&self) -> &[WsRoot] {
        &self.roots
    }

    /// `node.info.workspace_roots`：`available` = 启动时可用并且现在目录还在（外接盘中途拔掉会变成 false）。
    pub fn roots_info(&self) -> Vec<WorkspaceRootInfo> {
        self.roots
            .iter()
            .map(|r| WorkspaceRootInfo {
                path: path_string(&r.path),
                primary: r.primary,
                available: r.active && r.path.is_dir(),
            })
            .collect()
    }

    /// 可用的根（启动时可用的，含现在目录已经不在的），带下标，主根在最前。
    fn active_roots(&self) -> impl Iterator<Item = (usize, &WsRoot)> {
        self.roots.iter().enumerate().filter(|(_, r)| r.active)
    }

    /// workspace 根所在的卷不区分大小写（项目名按小写比较）。
    fn ci(&self) -> bool {
        self.case_insensitive.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 测试用：强制打开 / 关闭「不区分大小写」（和这台机器的卷无关，让规则本身在任何卷上都被覆盖）。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_case_insensitive_for_test(&self, on: bool) {
        self.case_insensitive.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：冷缓存时 `workspace.list` 对每个项目最多等多久（缺省 3 秒）。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_git_cold_budget_for_test(&self, d: Duration) {
        self.git.cold_budget_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：算完的项目合并成一条 `workspace.changed { updated }` 的防抖时长（缺省 200 毫秒）。机器忙时几个项目算完的时刻
    /// 会拉开，测试把它加长，才不会因为调度抖动把一批拆成两条。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_git_updated_debounce_for_test(&self, d: Duration) {
        self.git.updated_debounce_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：忘掉所有项目的 git 元数据缓存（再次 `workspace.list` 就是冷缓存）。
    #[cfg(any(test, feature = "testing"))]
    pub fn clear_git_cache_for_test(&self) {
        self.git.cache.lock().unwrap().clear();
    }

    /// 测试用：命名空间锁本身（锁顺序的回归用例要自己持有它）。
    #[cfg(any(test, feature = "testing"))]
    pub fn ns_lock_for_test(&self) -> &tokio::sync::Mutex<()> {
        &self.ns_lock
    }

    // ---------------- 路径安全 ----------------

    /// 项目目录（必须存在）。校验名字、canonicalize、前缀检查。关联的项目优先：返回关联的目录
    /// （目录不在了返回 `not_found`）；否则是某个根下的文件夹（主根优先，再按配置顺序找额外根），必须仍在那个根之内。
    pub fn project_path(&self, name: &str) -> RpcResult<PathBuf> {
        self.locate(name).map(|(_, canon)| canon)
    }

    /// 项目所在的根（下标；关联项目没有，是 `None`）和 canonical 目录。规则同 [`project_path`](Self::project_path)。
    fn locate(&self, name: &str) -> RpcResult<(Option<usize>, PathBuf)> {
        validate_name(name)?;
        if let Some(stored) = self.find_linked(name) {
            return self.linked_dir(&stored).map(|d| (None, d));
        }
        let Some((idx, p)) = self.find_entry(name) else {
            return Err(err(ErrorCode::NotFound, format!("project {name} not found")));
        };
        let root = &self.roots[idx].path;
        let canon =
            crate::sys::canonicalize(&p).map_err(|_| err(ErrorCode::NotFound, format!("project {name} not found")))?;
        if !canon.starts_with(root) {
            return Err(err(ErrorCode::Forbidden, "project path escapes the workspace root"));
        }
        if !canon.is_dir() {
            return Err(err(ErrorCode::NotFound, format!("project {name} is not a directory")));
        }
        Ok((Some(idx), canon))
    }

    /// 根下项目所在的根（下标）；关联项目 `None`。项目不存在 / 不可用时报错。
    fn root_of_project(&self, name: &str) -> RpcResult<usize> {
        match self.locate(name)? {
            (Some(idx), _) => Ok(idx),
            (None, _) => {
                Err(err(ErrorCode::Invalid, format!("{name} is a linked directory, not a folder in a workspace root")))
            }
        }
    }

    /// `fs.*` 的相对路径解析。拒绝绝对路径、`..`、NUL；canonicalize 后必须仍在所属的边界之内（符号链接逃逸被拒绝）。
    /// 第一段是关联项目名时，其余部分相对关联目录解析、边界是关联目录；否则相对项目所在的 workspace 根、边界是那个根
    /// （第一段不是任何根下的项目时——空路径、`.trash`、`.scratch`、不存在的名字——是主根）。
    pub fn resolve_rel(&self, rel: &str) -> RpcResult<PathBuf> {
        self.resolve_in(rel).map(|(canon, _)| canon)
    }

    /// [`resolve_rel`](Self::resolve_rel)，同时返回边界目录（列目录时用它过滤指向边界之外的符号链接）。
    fn resolve_in(&self, rel: &str) -> RpcResult<(PathBuf, PathBuf)> {
        if rel.contains('\0') {
            return Err(err(ErrorCode::Invalid, "path contains NUL"));
        }
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(err(ErrorCode::Forbidden, "absolute paths are not allowed"));
        }
        let mut segments: Vec<&OsStr> = Vec::new();
        for c in rel_path.components() {
            match c {
                Component::Normal(s) => segments.push(s),
                Component::CurDir => {}
                _ => return Err(err(ErrorCode::Forbidden, "path escapes the workspace root")),
            }
        }
        let first = segments.first().and_then(|s| s.to_str());
        let (boundary, rest) = match first.and_then(|n| self.find_linked(n)) {
            Some(name) => (self.linked_dir(&name)?, &segments[1..]),
            None => (self.boundary_root(first), &segments[..]),
        };
        let mut joined = boundary.clone();
        joined.extend(rest);
        let canon =
            crate::sys::canonicalize(&joined).map_err(|_| err(ErrorCode::NotFound, format!("{rel} not found")))?;
        if !canon.starts_with(&boundary) {
            return Err(err(ErrorCode::Forbidden, "path escapes the project directory"));
        }
        Ok((canon, boundary))
    }

    /// 第一段名字所属的 workspace 根（路径）：它是某个根下的项目就是那个根，否则（含以 `.` 开头的名字）是主根。
    fn boundary_root(&self, first: Option<&str>) -> PathBuf {
        let idx = first.filter(|n| !n.starts_with('.')).and_then(|n| self.find_entry(n)).map_or(0, |(i, _)| i);
        self.roots[idx].path.clone()
    }

    /// `.scratch/<session-id>`（主根下；scratch 只在主根），不存在则创建。
    pub fn scratch_dir(&self, session_id: &str) -> std::io::Result<PathBuf> {
        let p = self.scratch_path(session_id);
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    pub fn scratch_path(&self, session_id: &str) -> PathBuf {
        self.root().join(SCRATCH_DIR).join(session_id)
    }

    /// 删除 scratch 目录（会话删除 / 过期清理）。
    pub fn remove_scratch(&self, session_id: &str) {
        if session_id.contains('/') || session_id.contains("..") || session_id.is_empty() {
            return;
        }
        let p = self.scratch_path(session_id);
        if p.starts_with(self.root().join(SCRATCH_DIR)) {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    /// scratch 会话转项目：`mv .scratch/<id> <主根>/<name>`（scratch 在主根，转成的项目也在主根，同一个文件系统）。
    pub async fn promote_scratch(&self, session_id: &str, name: &str) -> RpcResult<PathBuf> {
        let name = nfc(name);
        let name = name.as_str();
        validate_name(name)?;
        let src = self.scratch_path(session_id);
        if !src.is_dir() {
            return Err(err(ErrorCode::NotFound, "scratch directory not found"));
        }
        let dst = self.root().join(name);
        {
            let _ns = self.ns_lock.lock().await;
            self.check_name_free(name)?;
            std::fs::rename(&src, &dst).map_err(|e| err(ErrorCode::Internal, format!("promote: {e}")))?;
        }
        self.rescan_and_notify().await;
        Ok(dst)
    }

    // ---------------- 扫描与元数据 ----------------

    /// 所有可用根下的项目。名字全局唯一，同名时 关联项目 > 主根 > 额外根（按配置顺序）：输的那个文件夹不算项目，
    /// 记在第二个返回值里（调用方 warn）。同一个根里按字节不同、只差大小写 / NFC 的两个文件夹不互相盖住（沿用 v0.1）。
    fn scan(&self) -> (Vec<Scanned>, Vec<Shadowed>) {
        let mut out = Vec::new();
        let mut shadowed = Vec::new();
        let linked_keys: HashSet<String> = self.linked.read().unwrap().keys().map(|n| name_key(n, self.ci())).collect();
        // 更高优先级的根已经占了的名字（键 → 根的下标）
        let mut taken: HashMap<String, usize> = HashMap::new();
        for (idx, root) in self.active_roots() {
            let Ok(rd) = std::fs::read_dir(&root.path) else { continue };
            let mut mine = Vec::new();
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                // metadata() 跟随符号链接：指向根外的目录不算项目
                let Some(md) = project_dir_metadata(&root.path, &e.path()) else { continue };
                let key = name_key(&name, self.ci());
                if linked_keys.contains(&key) {
                    shadowed.push(Shadowed { name, root: idx, by: ShadowedBy::Linked });
                    continue;
                }
                if let Some(&winner) = taken.get(&key) {
                    shadowed.push(Shadowed { name, root: idx, by: ShadowedBy::Root(winner) });
                    continue;
                }
                mine.push(key);
                out.push(Scanned {
                    id: crate::sys::file_identity(&md, &e.path(), true).unwrap_or((0, 0)),
                    name,
                    root: idx,
                });
            }
            for key in mine {
                taken.insert(key, idx);
            }
        }
        (out, shadowed)
    }

    fn scan_names(&self) -> HashMap<String, (u64, u64)> {
        self.scan().0.into_iter().map(|e| (e.name, e.id)).collect()
    }

    /// 被盖住的同名文件夹：warn 一次（每个名字每次进程只记一次）。
    fn warn_shadowed(&self, shadowed: Vec<Shadowed>) {
        for s in shadowed {
            if !self.shadow_warned.lock().unwrap().insert(s.name.clone()) {
                continue;
            }
            let hidden_root = path_string(&self.roots[s.root].path);
            match s.by {
                ShadowedBy::Linked => tracing::warn!(
                    project = %s.name,
                    root = %hidden_root,
                    "a folder in a workspace root has the same name as a linked project; the linked project wins and the folder is hidden"
                ),
                ShadowedBy::Root(winner) => tracing::warn!(
                    project = %s.name,
                    root = %hidden_root,
                    winner = %path_string(&self.roots[winner].path),
                    "a folder in this workspace root has the same name as a project in a workspace root listed before it; the earlier one wins and this folder is hidden"
                ),
            }
        }
    }

    /// `workspace.list`：所有可用根下的项目 + 关联的项目。`active` = 项目名 → 活跃会话数（由 sessions 提供）。
    pub async fn list(&self, active: &HashMap<String, u32>) -> WorkspaceListResult {
        let (entries, shadowed) = self.scan();
        self.warn_shadowed(shadowed);
        // 会话里存的项目名可能和盘上的写法不同（NFC / NFD、大小写）：按规范化的键汇总活跃会话数
        let mut active_by_key: HashMap<String, u32> = HashMap::new();
        for (n, c) in active {
            *active_by_key.entry(name_key(n, self.ci())).or_default() += c;
        }
        let active_of = |n: &str| active_by_key.get(&name_key(n, self.ci())).copied().unwrap_or(0);
        let mut tasks = Vec::new();
        for e in entries {
            let root = path_string(&self.roots[e.root].path);
            let path = self.roots[e.root].path.join(&e.name);
            let name = e.name;
            let active_sessions = active_of(&name);
            let git_svc = self.git.clone();
            tasks.push(tokio::spawn(async move {
                let md = std::fs::metadata(&path).ok();
                let modified_at_ms = md.as_ref().map(mtime_ms).unwrap_or(0);
                let (git, git_pending) = git_svc.get(&name, &path).await;
                (
                    Project {
                        name,
                        path: path.to_string_lossy().into_owned(),
                        root: Some(root),
                        git,
                        modified_at_ms,
                        active_sessions,
                        kind: ProjectKind::Folder,
                        missing: false,
                        git_pending,
                    },
                    None,
                )
            }));
        }
        let ctx = Arc::new(self.link_ctx_lenient());
        for (name, path) in ctx.linked.clone() {
            let active_sessions = active_of(&name);
            let git_svc = self.git.clone();
            let ctx = ctx.clone();
            tasks.push(tokio::spawn(async move {
                linked_project_state(&ctx, &git_svc, name, &path, active_sessions).await
            }));
        }
        let mut projects = Vec::new();
        for t in tasks {
            if let Ok((p, problem)) = t.await {
                if let Some(problem) = problem {
                    // 目录被换成了符号链接 / 按当前规则不再允许：这个项目所有会话的进程都要收掉
                    self.report_problem(&p.name, &problem);
                }
                projects.push(p);
            }
        }
        projects.sort_by(|a, b| b.modified_at_ms.cmp(&a.modified_at_ms).then(a.name.cmp(&b.name)));
        WorkspaceListResult { root: path_string(self.root()), projects }
    }

    /// 单个项目的当前信息。关联项目的目录不在了不算错误（`missing: true`）；根目录下不存在的项目是 `not_found`。
    pub async fn project(&self, name: &str, active: u32) -> RpcResult<Project> {
        validate_name(name)?;
        if let Some(stored) = self.find_linked(name) {
            let path = self.linked_path(&stored).unwrap_or_default();
            let ctx = self.link_ctx_lenient();
            let (project, problem) = linked_project_state(&ctx, &self.git, stored, &path, active).await;
            if let Some(problem) = problem {
                self.report_problem(&project.name, &problem);
            }
            return Ok(project);
        }
        let (idx, path) = self.locate(name)?;
        let modified_at_ms = std::fs::metadata(&path).ok().as_ref().map(mtime_ms).unwrap_or(0);
        let canonical_name = self.canonical_project_name(name);
        let (git, git_pending) = self.git.get(&canonical_name, &path).await;
        Ok(Project {
            name: canonical_name,
            path: path.to_string_lossy().into_owned(),
            root: idx.map(|i| path_string(&self.roots[i].path)),
            git,
            modified_at_ms,
            active_sessions: active,
            kind: ProjectKind::Folder,
            missing: false,
            git_pending,
        })
    }

    // ---------------- 变更通知 ----------------

    /// 重新扫描所有根，和快照做 diff，有变化就发 `workspace.changed`。改名靠目录身份识别（只认同一个根内的改名：
    /// 跨根的移动要拷贝，身份会变，是一次删除加一次新增）。同名项目换了落脚的目录（更高优先级的根里的同名文件夹没了、
    /// 露出另一个根里的）进 `updated`，客户端重新拉列表。
    pub async fn rescan_and_notify(&self) {
        let new = self.scan_names();
        let change = {
            let mut snap = self.snapshot.lock().unwrap();
            let old = std::mem::replace(&mut *snap, new.clone());
            let mut added: Vec<String> = new.keys().filter(|k| !old.contains_key(*k)).cloned().collect();
            let mut removed: Vec<String> = old.keys().filter(|k| !new.contains_key(*k)).cloned().collect();
            let mut renamed = Vec::new();
            for r in removed.clone() {
                let id = old[&r];
                if let Some(a) = added.iter().find(|a| new[*a] == id).cloned() {
                    renamed.push(RenamedProject { from: r.clone(), to: a.clone() });
                    removed.retain(|x| x != &r);
                    added.retain(|x| x != &a);
                }
            }
            let mut updated: Vec<String> =
                new.iter().filter(|(k, id)| old.get(*k).is_some_and(|o| o != *id)).map(|(k, _)| k.clone()).collect();
            added.sort();
            removed.sort();
            updated.sort();
            WorkspaceChanged { added, removed, renamed, updated }
        };
        if !change.added.is_empty()
            || !change.removed.is_empty()
            || !change.renamed.is_empty()
            || !change.updated.is_empty()
        {
            self.clients.broadcast(methods::WORKSPACE_CHANGED, &change);
        }
    }

    /// 只 watch 每个可用根的一层，500 ms 防抖。返回的 guard drop 时停止。额外根 watch 不上只 warn（主根不行才报错）。
    pub fn spawn_watcher(self: &Arc<Self>) -> notify::Result<WatchGuard> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let roots: Vec<PathBuf> = self.active_roots().map(|(_, r)| r.path.clone()).collect();
        let event_roots = roots.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if ev.paths.iter().any(|p| event_is_relevant(&event_roots, p)) {
                let _ = tx.send(());
            }
        })?;
        for (i, root) in roots.iter().enumerate() {
            match watcher.watch(root, RecursiveMode::NonRecursive) {
                Ok(()) => {}
                Err(e) if i > 0 => {
                    tracing::warn!(root = %root.display(), error = %e, "cannot watch this workspace root; changes in it show up on the next list");
                }
                Err(e) => return Err(e),
            }
        }
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // 防抖：安静 500 ms 后再扫
                loop {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(())) => continue,
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                this.rescan_and_notify().await;
            }
        });
        Ok(WatchGuard { _watcher: watcher, task })
    }

    // ---------------- 关联已有目录（ADR-33） ----------------

    /// 按 NFC（以及不区分大小写的文件系统上的大小写）找已关联的项目，返回登记的名字。
    pub fn find_linked(&self, name: &str) -> Option<String> {
        let linked = self.linked.read().unwrap();
        if linked.contains_key(name) {
            return Some(name.to_string());
        }
        let key = name_key(name, self.ci());
        linked.keys().find(|n| name_key(n, self.ci()) == key).cloned()
    }

    pub fn is_linked(&self, name: &str) -> bool {
        self.find_linked(name).is_some()
    }

    /// 关联项目登记的目录（link 时的 canonical 路径，不检查现在还在不在）。
    pub fn linked_path(&self, name: &str) -> Option<PathBuf> {
        let stored = self.find_linked(name)?;
        self.linked.read().unwrap().get(&stored).cloned()
    }

    /// 关联项目的目录。**使用前复核**（ADR-33 第 5 条）：现在 canonicalize 一次，结果必须等于登记的路径，
    /// 并且按当前配置重跑隐藏目录 / 拒绝表 / 绝对拒绝；不满足就当 `missing`（`not_found`）。
    fn linked_dir(&self, name: &str) -> RpcResult<PathBuf> {
        let stored =
            self.find_linked(name).ok_or_else(|| err(ErrorCode::NotFound, format!("project {name} not found")))?;
        let registered = self.linked_path(&stored).unwrap_or_default();
        self.link_ctx_lenient().resolve_linked(&registered).map_err(|problem: LinkedProblem| {
            self.report_problem(&stored, &problem);
            err(ErrorCode::NotFound, problem.describe(&stored))
        })
    }

    /// daemon 启动时接线：关联项目被发现 `Moved` / `NotAllowed` 时调用（回调里把这个项目所有会话的进程收掉）。
    pub fn set_problem_hook(&self, hook: ProblemHook) {
        *self.problem_hook.write().unwrap() = Some(hook);
    }

    /// 关联项目的目录被换成指向别处的符号链接 / 按当前规则不再允许：通知回调。
    /// 目录只是不在了（`Missing`）不通知：agent 的工作目录是个已经删掉的目录，不会读到别处的东西。
    fn report_problem(&self, name: &str, problem: &LinkedProblem) {
        if matches!(problem, LinkedProblem::Missing) {
            return;
        }
        let Some(hook) = self.problem_hook.read().unwrap().clone() else { return };
        hook(name.to_string());
    }

    /// 名字已被占用（某个关联项目，或任何一个根里的任何条目，NFC / 不区分大小写地比较）-> `exists`。
    /// 全局一个命名空间：别的根里的同名文件夹也算占用（它会被列在列表里，新的这个反而会被盖住）。
    fn check_name_free(&self, name: &str) -> RpcResult<()> {
        if self.is_linked(name) {
            return Err(err(ErrorCode::Exists, format!("project {name} already exists (a linked directory)")));
        }
        if self.root_name_taken(name) {
            return Err(err(ErrorCode::Exists, format!("project {name} already exists")));
        }
        Ok(())
    }

    /// 可用的根里有没有占着这个名字的条目：文件系统自己认的（含符号链接），或者按 NFC / 大小写规则等价的。
    fn root_name_taken(&self, name: &str) -> bool {
        let key = name_key(name, self.ci());
        self.active_roots().any(|(_, r)| {
            if std::fs::symlink_metadata(r.path.join(name)).is_ok() {
                return true;
            }
            match std::fs::read_dir(&r.path) {
                Ok(rd) => rd.flatten().any(|e| name_key(&e.file_name().to_string_lossy(), self.ci()) == key),
                Err(_) => false,
            }
        })
    }

    /// 两个项目名是不是同一个名字（NFC；不区分大小写的文件系统上再忽略大小写）。
    pub fn same_project_name(&self, a: &str, b: &str) -> bool {
        a == b || name_key(a, self.ci()) == name_key(b, self.ci())
    }

    /// 会话里存的项目名、对外显示的项目名：已关联的用登记的名字；根下的项目用**盘上那一项的原样名字**
    /// （按规范化的键在根目录里找：Finder 建的 NFD 名字和 `workspace.list` 给的是同一个字符串）；
    /// 都找不到（项目还不存在）就是 NFC。
    pub fn canonical_project_name(&self, name: &str) -> String {
        if let Some(stored) = self.find_linked(name) {
            return stored;
        }
        self.find_entry(name)
            .and_then(|(idx, _)| self.disk_name_in(&self.roots[idx].path, name))
            .unwrap_or_else(|| nfc(name))
    }

    /// `root` 里和 `name` 按规范化的键等价的那一项（不含点开头的），返回盘上原样的名字。
    fn disk_name_in(&self, root: &Path, name: &str) -> Option<String> {
        let key = name_key(name, self.ci());
        std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|on_disk| !on_disk.starts_with('.') && name_key(on_disk, self.ci()) == key)
    }

    /// 根 `root` 下项目 `name` 在盘上的路径：原样的名字在就用它，否则用按规范化的键找到的那一项。
    /// APFS 不区分 NFC / NFD，ext4 之类按字节比较名字：NFC 写法找不到 NFD 名字的文件夹，要靠后一步。
    fn root_entry_in(&self, root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        if std::fs::symlink_metadata(&p).is_ok() {
            return p;
        }
        self.disk_name_in(root, name).map_or(p, |on_disk| root.join(on_disk))
    }

    /// 按项目优先级（主根，再按配置顺序的额外根）找名字对应的盘上条目：根的下标和条目路径。优先返回「确实是项目」的条目
    /// （目录，且 canonical 路径在所属的根之内），所有根里都没有这样的条目时退回第一个同名条目，让调用方按它报具体的错
    /// （不是目录、指向根外……）。不含关联项目。
    fn find_entry(&self, name: &str) -> Option<(usize, PathBuf)> {
        let mut fallback = None;
        for (idx, root) in self.active_roots() {
            let p = self.root_entry_in(&root.path, name);
            if std::fs::symlink_metadata(&p).is_err() {
                continue;
            }
            if project_dir_metadata(&root.path, &p).is_some() {
                return Some((idx, p));
            }
            fallback.get_or_insert((idx, p));
        }
        fallback
    }

    /// 项目现在指向哪里，用来给会话记「创建时的 cwd」。
    /// - 关联项目：`linked:<canonical 目录>`（会复核）；
    /// - 根目录下的项目：`folder:<盘上的名字>#<dev>:<ino>`——不记绝对路径（workspace 根整体搬家后会话照常能用），
    ///   记目录自己的身份：同一个设备上 inode 不同 = 换了一个目录（进了回收站再新建同名文件夹），旧会话不能续接；
    ///   从回收站恢复 inode 不变，可以续接；设备号不同 = 整个 workspace 根搬到了别的盘，接受并重新记下。
    pub fn project_loc(&self, name: &str) -> RpcResult<String> {
        validate_name(name)?;
        if let Some(stored) = self.find_linked(name) {
            return Ok(format!("linked:{}", self.linked_dir(&stored)?.display()));
        }
        let dir = self.project_path(name)?;
        let md = std::fs::metadata(&dir).map_err(|_| err(ErrorCode::NotFound, format!("project {name} not found")))?;
        let (dev, ino) = crate::sys::file_identity(&md, &dir, true)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("project {name} not found")))?;
        Ok(format!("folder:{}#{}:{}", self.canonical_project_name(name), dev, ino))
    }

    /// 记下的位置和现在的位置比较。
    pub fn check_loc(&self, recorded: &str, now: &str) -> LocCheck {
        match (parse_folder_loc(recorded), parse_folder_loc(now)) {
            (Some((rec_name, rec_id)), Some((now_name, now_id))) => {
                if name_key(rec_name, self.ci()) != name_key(now_name, self.ci()) {
                    return LocCheck::Different;
                }
                match (rec_id, now_id) {
                    (Some((rd, ri)), Some((nd, ni))) if rd == nd => {
                        if ri == ni {
                            LocCheck::Same
                        } else {
                            LocCheck::Different
                        }
                    }
                    // 设备号不同，但记下的那个设备现在还是某个 workspace 根所在的盘：不是「整个根搬了盘」，而是这个名字现在
                    // 指向了另一个根里的另一个目录（比如原来那个进了回收站、别的根里建了同名的）：拒绝续接
                    (Some((rd, _)), Some(_)) if self.is_root_device(rd) => LocCheck::Different,
                    // 设备号不同（整个根搬了盘），或旧版本只记了名字：接受并重新记下
                    _ => LocCheck::Refresh(now.to_string()),
                }
            }
            _ if recorded == now => LocCheck::Same,
            _ => LocCheck::Different,
        }
    }

    /// 有没有可用的 workspace 根在设备 `dev` 上。
    fn is_root_device(&self, dev: u64) -> bool {
        self.active_roots().any(|(_, r)| crate::sys::identity_of(&r.path).is_some_and(|(d, _)| d == dev))
    }

    /// `project_loc` 的人读形式（拒绝续接时写进消息）。
    pub fn describe_loc(recorded: &str) -> String {
        match (recorded.strip_prefix("linked:"), parse_folder_loc(recorded)) {
            (Some(path), _) => path.to_string(),
            (_, Some((name, _))) => format!("the folder {name} in a workspace root"),
            _ => recorded.to_string(),
        }
    }

    /// `workspace.trash` 对关联项目是 `invalid`：anycon 永远不移动、不删除关联目录里的文件。
    pub fn ensure_not_linked_for_trash(&self, name: &str) -> RpcResult<()> {
        if self.is_linked(name) {
            Err(err(
                ErrorCode::Invalid,
                format!("{name} is a linked directory and is never moved or deleted; use workspace.unlink to remove it from the list"),
            ))
        } else {
            Ok(())
        }
    }

    /// 启动时固定下来的 canonical HOME。
    fn home_canonical(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.canon.clone())
    }

    /// HOME 现在的 canonical 路径和启动时不一样了（运行中 HOME 这个链接被改指向别处）：返回现在的路径。
    fn home_moved(&self) -> Option<PathBuf> {
        let h = self.home.as_ref()?;
        let now = crate::sys::canonicalize(&h.raw).ok()?;
        (now != h.canon).then_some(now)
    }

    /// 测试用：加一个 firmlink（`数据卷前缀`, `系统卷前缀`），`数据卷前缀/X…` 和 `系统卷前缀/X…` 身份相同时视为同一个目录。
    #[cfg(any(test, feature = "testing"))]
    pub fn add_firmlink_for_test(&self, data: &Path, sys: &Path) {
        self.test_firmlinks.write().unwrap().push((data.to_path_buf(), sys.to_path_buf()));
    }

    /// 词法比较前要换掉的写法：系统别名和 firmlink，加上「HOME 的原样路径 → 启动时固定的 canonical 路径」（HOME 在符号链接后面时）。
    fn path_aliases(&self) -> PathAliases {
        let mut aliases = PathAliases::system();
        #[cfg(any(test, feature = "testing"))]
        aliases.firmlinks.extend(self.test_firmlinks.read().unwrap().iter().cloned());
        if let Some(h) = &self.home {
            let raw = lexical_clean(&h.raw);
            if raw != h.canon {
                aliases.prefixes.push((raw, h.canon.clone()));
            }
        }
        aliases
    }

    /// 用户给的路径（`workspace.link` / `workspace.browse`）：`~` 按固定下来的 canonical HOME 展开（和 `link_roots` 同一套）。
    fn expand_user_path(&self, raw: &str) -> Result<PathBuf, String> {
        expand_with_home(raw, self.home_canonical().as_deref())
    }

    /// 判定 `link_roots` 的一项（ADR-33 第 1 条，**每次请求都重判**，不依赖任何快照）：`~` 按启动时固定的 canonical HOME 展开
    /// （HOME 的 canonical 路径之后变了，`~` 开头的项整项忽略），系统别名 / firmlink 规范化，然后**逐段 `lstat`**：
    /// 任何一段是符号链接就整项忽略；大小写或 NFC / NFD 写法和盘上不同不算链接，以 canonical 路径作为这个 root。
    /// 返回规范化的词法路径（展开失败为 `None`）和判定。
    fn judge_root(&self, configured: &str, aliases: &PathAliases) -> (Option<PathBuf>, RootJudgement) {
        let Ok(expanded) = self.expand_user_path(configured) else {
            return (
                None,
                RootJudgement::Ignored {
                    reason: "it cannot be expanded (the daemon environment has no usable HOME, or the entry is not an absolute path / `~` / `~/…`)".into(),
                    brief: "cannot be expanded",
                },
            );
        };
        if configured == "~" || crate::util::strip_tilde_slash(configured).is_some() {
            if let Some(now) = self.home_moved() {
                let was = self.home.as_ref().map(|h| h.canon.display().to_string()).unwrap_or_default();
                return (
                    None,
                    RootJudgement::Ignored {
                        reason: format!(
                            "HOME now resolves to {} but it was {was} when the daemon started; restart the daemon (or write the real path)",
                            now.display()
                        ),
                        brief: "HOME was changed after the daemon started (restart the daemon)",
                    },
                );
            }
        }
        let normalized = aliases.normalize(&expanded);
        // 逐段 lstat：任何一段是符号链接 -> 忽略；任何一段不存在 -> 目录不存在
        let mut cur = PathBuf::new();
        for c in normalized.components() {
            cur.push(c.as_os_str());
            if matches!(c, Component::RootDir | Component::Prefix(_)) {
                continue;
            }
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => {
                    let target = crate::sys::canonicalize(&cur).map(|t| t.display().to_string()).unwrap_or_default();
                    return (
                        Some(normalized.clone()),
                        RootJudgement::Ignored {
                            reason: format!(
                                "{} goes through a symbolic link at {} (its real path is {target}); write the real path instead",
                                normalized.display(),
                                cur.display()
                            ),
                            brief: "goes through a symbolic link (write the real path)",
                        },
                    );
                }
                Ok(_) => {}
                Err(_) => return (Some(normalized), RootJudgement::Missing),
            }
        }
        match crate::sys::canonicalize(&normalized) {
            Ok(c) if c.is_dir() => {
                // 没有链接，canonical 路径和写法最多差在大小写 / Unicode 规范化；差得更多说明有我们没认出来的别名，宁可忽略
                if fold_path(&c, true) == fold_path(&normalized, true) {
                    (Some(normalized), RootJudgement::Usable(c))
                } else {
                    (
                        Some(normalized.clone()),
                        RootJudgement::Ignored {
                            reason: format!(
                                "{} resolves to {}, which is a different path; write the real path instead",
                                normalized.display(),
                                c.display()
                            ),
                            brief: "resolves to a different path (write the real path)",
                        },
                    )
                }
            }
            _ => (Some(normalized), RootJudgement::Missing),
        }
    }

    /// 把配置里的 `link_roots` 逐项判定成规则引擎用的 [`LinkRoots`]，同时返回不能用的项和原因（被忽略的和不存在的）。
    fn evaluate_roots(&self, aliases: &PathAliases) -> (LinkRoots, Vec<IgnoredRoot>) {
        let cfg = self.config.get();
        let mut roots = LinkRoots::default();
        let mut unusable = Vec::new();
        for configured in &cfg.workspace.link_roots {
            match self.judge_root(configured, aliases).1 {
                RootJudgement::Usable(c) => {
                    if !roots.canon.contains(&c) {
                        roots.raw.push(c.clone());
                        roots.canon.push(c);
                    }
                }
                RootJudgement::Missing => unusable.push(IgnoredRoot {
                    entry: configured.clone(),
                    reason: "the directory does not exist (or is not a directory)".into(),
                    brief: "does not exist".into(),
                    missing: true,
                }),
                RootJudgement::Ignored { reason, brief } => unusable.push(IgnoredRoot {
                    entry: configured.clone(),
                    reason,
                    brief: brief.into(),
                    missing: false,
                }),
            }
        }
        (roots, unusable)
    }

    /// 现在被忽略的 `link_roots` 项和原因（经过符号链接的、展开不了的、HOME 变了的）。目录只是不存在的项不算被忽略。
    pub fn ignored_link_roots(&self) -> Vec<(String, String)> {
        self.link_roots_report().into_iter().filter(|r| !r.missing).map(|r| (r.entry, r.reason)).collect()
    }

    /// `link_roots` 里所有不能用的项（被忽略的和目录不存在的）和原因：`config set` 之后打印给用户看。
    pub fn link_roots_report(&self) -> Vec<IgnoredRoot> {
        self.evaluate_roots(&self.path_aliases()).1
    }

    /// 对被忽略的 `link_roots` 项 warn（每个原因每个进程一次）。
    fn warn_ignored_roots(&self) {
        for (configured, why) in self.ignored_link_roots() {
            if self.roots_warned.lock().unwrap().insert(format!("{configured}\0{why}")) {
                tracing::warn!(entry = %configured, "workspace.link_roots: this entry is ignored: {why}");
            }
        }
    }

    /// 配置变了（热加载 / `config set`）就马上判一遍、对新出现的被忽略项 warn。
    fn spawn_roots_watcher(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut rx = self.config.subscribe();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(ws) = weak.upgrade() else { return };
                ws.warn_ignored_roots();
            }
        });
    }

    /// 规则快照（使用已有关联时用）：`workspace.link_roots` 可以是空的、里面的项可以都不存在 / 被忽略。
    /// `~` 按启动时固定的 canonical HOME 展开，拒绝表和 TCC 表按固定的 HOME 展开（词法 + canonical 两种写法），数据目录取
    /// `ConfigStore::paths()`。root 每次都重新判定（不经过符号链接）。
    fn link_ctx_lenient(&self) -> LinkCtx {
        self.build_link_ctx().0
    }

    /// [`link_ctx_lenient`](Self::link_ctx_lenient)，同时返回 `link_roots` 里不能用的项（被忽略的和目录不存在的）。
    fn build_link_ctx(&self) -> (LinkCtx, Vec<IgnoredRoot>) {
        let aliases = self.path_aliases();
        let (roots, unusable) = self.evaluate_roots(&aliases);
        for r in unusable.iter().filter(|r| !r.missing) {
            if self.roots_warned.lock().unwrap().insert(format!("{}\0{}", r.entry, r.reason)) {
                tracing::warn!(entry = %r.entry, "workspace.link_roots: this entry is ignored: {}", r.reason);
            }
        }
        let home = self.home.as_ref();
        let data = &self.config.paths().home;
        let mut ctx = LinkCtx {
            roots,
            workspace_roots: self.roots.iter().map(|r| r.path.clone()).collect(),
            data: crate::sys::canonicalize(data).unwrap_or_else(|_| data.clone()),
            data_lexical: lexical_clean(data),
            root_ids: Vec::new(),
            data_ids: Default::default(),
            deny: deny_table_with(home.map(|h| h.raw.as_path()), home.map(|h| h.canon.as_path())),
            aliases,
            tcc: tcc_table(home.map(|h| h.canon.as_path())),
            linked: self.linked.read().unwrap().iter().map(|(n, p)| (n.clone(), p.clone())).collect(),
            case_insensitive: self.ci(),
        };
        ctx.refresh_identities();
        (ctx, unusable)
    }

    /// `link` / `browse` 用的规则快照：列表为空，或一项都不可用：`forbidden`（整个功能不可用）；
    /// 一项都不可用时，错误消息列出每一项和原因（配置里写了、目录也在，却只看到「都不存在」，用户没法排查）。
    fn link_ctx(&self) -> RpcResult<LinkCtx> {
        if self.config.get().workspace.link_roots.is_empty() {
            return Err(err(
                ErrorCode::Forbidden,
                "linking existing directories is turned off (workspace.link_roots is empty)",
            ));
        }
        let (ctx, unusable) = self.build_link_ctx();
        if ctx.roots.canon.is_empty() {
            let list = unusable.iter().map(|r| format!("{} ({})", r.entry, r.brief)).collect::<Vec<_>>().join("; ");
            return Err(err(
                ErrorCode::Forbidden,
                format!("none of the directories in workspace.link_roots can be used: {list}"),
            ));
        }
        Ok(ctx)
    }

    /// `workspace.link { path, name? }`：把本机已有目录关联为项目。规则见 daemon.md § 10 / ADR-33。审计由 RPC 层记。
    pub async fn link(&self, path: &str, name: Option<&str>) -> RpcResult<Project> {
        let _ns = self.ns_lock.lock().await;
        let ctx = self.link_ctx()?;
        check_path_text(path)?;
        let expanded = self.expand_user_path(path).map_err(|m| err(ErrorCode::Invalid, m))?;
        let canon = ctx.canonicalize_within(&expanded)?;
        if !canon.is_dir() {
            // 和「不存在」同一个错误和消息：不让 link 的结果区分「没有这个路径」和「是个文件」
            return Err(err(ErrorCode::NotFound, crate::linkrules::NOT_A_DIRECTORY));
        }
        let Some(canon_str) = canon.to_str() else {
            return Err(err(ErrorCode::Invalid, "the path is not valid UTF-8"));
        };
        ctx.verdict(&canon)?;
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => nfc(n),
            None => {
                let dir_name = nfc(canon.file_name().and_then(|n| n.to_str()).unwrap_or_default());
                validate_name(&dir_name).map_err(|e| {
                    err(
                        ErrorCode::Invalid,
                        format!(
                            "the directory name {dir_name:?} cannot be used as a project name ({}); pass a name",
                            e.message
                        ),
                    )
                })?;
                dir_name
            }
        };
        validate_name(&name)?;
        self.check_name_free(&name)?;
        let row = LinkedRow { name: name.clone(), path: canon_str.to_string(), created_ms: now_ms() };
        self.store.insert_linked(&row).await.map_err(|e| err(ErrorCode::Internal, format!("link: {e}")))?;
        self.linked.write().unwrap().insert(name.clone(), canon.clone());
        tracing::info!(project = %name, path = %canon.display(), "linked an existing directory as a project");
        self.clients.broadcast(
            methods::WORKSPACE_CHANGED,
            &WorkspaceChanged { added: vec![name.clone()], ..Default::default() },
        );
        let ctx = self.link_ctx_lenient();
        Ok(linked_project_state(&ctx, &self.git, name, &canon, 0).await.0)
    }

    /// `workspace.unlink { name }`：从列表去掉，目录和文件不动。返回原来登记的路径（给审计用）。
    /// 不是关联项目（含根目录下的项目）：`invalid`。`busy` 是「这个项目上有没有运行中的会话」的检查：
    /// 它在**持有命名空间锁的状态下**调用，紧接着就删登记（检查和删除之间不会有 link / rename 等插进来；
    /// 之后才开始的 turn 因为项目已经解析不到而起不来，已经起来的由 RPC 层 `release_project` 取消）。
    pub async fn unlink<F, Fut>(&self, name: &str, busy: F) -> RpcResult<(String, PathBuf)>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let _ns = self.ns_lock.lock().await;
        let Some(stored) = self.find_linked(name) else {
            return Err(err(ErrorCode::Invalid, format!("{name} is not a linked project")));
        };
        if busy(stored.clone()).await {
            return Err(err(ErrorCode::Busy, "the project has running sessions"));
        }
        let path = self.linked_path(&stored).unwrap_or_default();
        match self.store.delete_linked(&stored).await {
            Ok(_) => {}
            Err(e) => return Err(err(ErrorCode::Internal, format!("unlink: {e}"))),
        }
        self.linked.write().unwrap().remove(&stored);
        self.git.forget(&path);
        tracing::info!(project = %stored, path = %path.display(), "unlinked a project (the directory is untouched)");
        self.clients.broadcast(
            methods::WORKSPACE_CHANGED,
            &WorkspaceChanged { removed: vec![stored.clone()], ..Default::default() },
        );
        // 原来被盖住的同名文件夹现在露出来了
        self.rescan_and_notify().await;
        Ok((stored, path))
    }

    /// `workspace.browse { path?, show_hidden? }`：只列 `link_roots` 之内的子目录（目录名，不读文件内容，不审计）。
    /// 拒绝表里的目录（以及数据目录）不能浏览进去。
    pub async fn browse(&self, path: Option<&str>, show_hidden: bool) -> RpcResult<WorkspaceBrowseResult> {
        let ctx = self.link_ctx()?;
        let target = match path.filter(|p| !p.is_empty()) {
            None => ctx.roots.canon[0].clone(),
            Some(p) => {
                check_path_text(p)?;
                let expanded = self.expand_user_path(p).map_err(|m| err(ErrorCode::Invalid, m))?;
                ctx.canonicalize_within(&expanded)?
            }
        };
        if !target.is_dir() {
            // 和「不存在」同一个错误和消息：浏览的结果不能区分「没有这个路径」和「是个文件」
            return Err(err(ErrorCode::NotFound, crate::linkrules::NOT_A_DIRECTORY));
        }
        if let Some(b) = ctx.block_of(&target) {
            if !b.enterable {
                return Err(err(ErrorCode::Forbidden, format!("this directory cannot be browsed: {}", b.message)));
            }
        }
        tokio::task::spawn_blocking(move || browse_blocking(&ctx, target, show_hidden))
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("browse: {e}")))?
    }

    // ---------------- 项目操作 ----------------

    /// 新建空项目。`root` 缺省 = 主根；否则必须是 `node.info` 列出的某个根（见 [`Self::resolve_root`]）。
    pub async fn create(&self, name: &str, git_init: bool, root: Option<&str>) -> RpcResult<PathBuf> {
        let name = nfc(name);
        let name = name.as_str();
        validate_name(name)?;
        let root_idx = self.resolve_root(root)?;
        let p = self.roots[root_idx].path.join(name);
        // `create_dir` 本身是原子的：并发的 create / clone 抢同一个名字，只有一个能成功；
        // 关联的项目和根目录下的项目共用命名空间，所以名字不能已被某个关联项目占用
        {
            let _ns = self.ns_lock.lock().await;
            self.check_name_free(name)?;
            self.claim_dir(&p, name)?;
        }
        if git_init {
            match run_git(&self.env, &["init", "-q"], Some(&p), Duration::from_secs(15)).await {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let _ = std::fs::remove_dir_all(&p);
                    return Err(err(
                        ErrorCode::Internal,
                        format!("git init failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
                    ));
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&p);
                    return Err(err(ErrorCode::Internal, format!("git init failed: {e}")));
                }
            }
        }
        self.rescan_and_notify().await;
        Ok(p)
    }

    /// `workspace.create` / `clone` 的 `root` 参数：缺省 = 主根；给了就必须是 `node.info.workspace_roots` 里的某个根
    /// （`~` 按固定的 HOME 展开，词法规范化后或 canonicalize 后等于某个根的路径；其他值，包括根下面的子目录，`invalid`）。
    /// 匹配上了但这个根现在不可用（启动时就不存在、或之后目录没了）→ `not_found`。
    fn resolve_root(&self, requested: Option<&str>) -> RpcResult<usize> {
        let idx = match requested {
            None => 0,
            Some(raw) => {
                check_path_text(raw)?;
                let expanded = self.expand_user_path(raw).map_err(|m| err(ErrorCode::Invalid, m))?;
                let lexical = lexical_clean(&expanded);
                let canon = crate::sys::canonicalize(&lexical).ok();
                self.roots
                    .iter()
                    .position(|r| r.path == lexical || canon.as_ref().is_some_and(|c| *c == r.path))
                    .ok_or_else(|| {
                        err(
                            ErrorCode::Invalid,
                            "root is not one of this node's workspace roots (workspace.root or an entry of workspace.extra_roots; node.info lists them)",
                        )
                    })?
            }
        };
        let root = &self.roots[idx];
        if !root.active || !root.path.is_dir() {
            return Err(err(
                ErrorCode::NotFound,
                format!(
                    "the workspace root {} is not available (is the disk mounted?); the node reads workspace.extra_roots when it starts",
                    root.path.display()
                ),
            ));
        }
        Ok(idx)
    }

    /// 原子地占住一个新目录：已存在（含符号链接）返回 `exists`。
    fn claim_dir(&self, path: &Path, name: &str) -> RpcResult<()> {
        match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(err(ErrorCode::Exists, format!("project {name} already exists")))
            }
            Err(e) => Err(err(ErrorCode::Internal, format!("mkdir: {e}"))),
        }
    }

    /// 改名。根目录下的项目：移动目录；关联的项目：只改登记的名字，目录不动。
    pub async fn rename(&self, from: &str, to: &str) -> RpcResult<()> {
        let to = nfc(to);
        let to = to.as_str();
        validate_name(from)?;
        validate_name(to)?;
        {
            let _ns = self.ns_lock.lock().await;
            if let Some(stored) = self.find_linked(from) {
                return self.rename_linked(&stored, to).await;
            }
            // 只在同一个根内改名：新名字落在原来所在的根里（跨根要拷贝，不是改名）
            let root_idx = self.root_of_project(from)?;
            let src = self.project_path(from)?;
            self.check_name_free(to)?;
            let dst = self.roots[root_idx].path.join(to);
            std::fs::rename(&src, &dst).map_err(|e| err(ErrorCode::Internal, format!("rename: {e}")))?;
            self.git.forget(&src);
        }
        self.rescan_and_notify().await;
        Ok(())
    }

    /// 关联项目改名（调用方已持有 `ns_lock`）。
    async fn rename_linked(&self, from: &str, to: &str) -> RpcResult<()> {
        self.check_name_free(to)?;
        match self.store.rename_linked(from, to).await {
            Ok(true) => {}
            Ok(false) => return Err(err(ErrorCode::NotFound, format!("project {from} not found"))),
            Err(e) => return Err(err(ErrorCode::Internal, format!("rename linked project: {e}"))),
        }
        {
            let mut linked = self.linked.write().unwrap();
            if let Some(path) = linked.remove(from) {
                linked.insert(to.to_string(), path);
            }
        }
        self.clients.broadcast(
            methods::WORKSPACE_CHANGED,
            &WorkspaceChanged {
                renamed: vec![RenamedProject { from: from.to_string(), to: to.to_string() }],
                ..Default::default()
            },
        );
        // 原来被盖住的同名文件夹现在露出来了
        self.rescan_and_notify().await;
        Ok(())
    }

    /// 移到所在根的 `.trash/<name>-<timestamp>`（每个根有自己的回收站，`rename` 不跨文件系统）。
    pub async fn trash(&self, name: &str) -> RpcResult<TrashEntry> {
        validate_name(name)?;
        self.ensure_not_linked_for_trash(name)?;
        let root_idx = self.root_of_project(name)?;
        let root = &self.roots[root_idx].path;
        let ts = now_ms();
        let id = format!("{name}-{}", fmt_compact(ts));
        // id 在所有根之间唯一：`restore` / `purge` 只带 id，靠它找到是哪个根的回收站
        if self.active_roots().any(|(_, r)| r.path.join(TRASH_DIR).join(&id).exists()) {
            return Err(err(ErrorCode::Exists, "trash entry already exists; retry in a second"));
        }
        let dst = root.join(TRASH_DIR).join(&id);
        std::fs::create_dir_all(root.join(TRASH_DIR)).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        // 如果 name 本身是符号链接，rename 的是链接而不是目标
        let src_for_move = self.root_entry_in(root, name);
        std::fs::rename(&src_for_move, &dst).map_err(|e| err(ErrorCode::Internal, format!("move to trash: {e}")))?;
        self.rescan_and_notify().await;
        let bytes = dir_size(&dst).await;
        Ok(TrashEntry { id, original_name: name.to_string(), trashed_at_ms: ts, bytes, root: Some(path_string(root)) })
    }

    /// 所有可用根的回收站（主根在前），按删除时间从新到旧；每项标出来自哪个根。
    pub async fn trash_list(&self) -> Vec<TrashEntry> {
        let mut entries = Vec::new();
        for (_, root) in self.active_roots() {
            let Ok(rd) = std::fs::read_dir(root.path.join(TRASH_DIR)) else { continue };
            let root_str = path_string(&root.path);
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                let Some((original, ts)) = parse_trash_id(&id) else { continue };
                let bytes = dir_size(&e.path()).await;
                entries.push(TrashEntry {
                    id,
                    original_name: original,
                    trashed_at_ms: ts,
                    bytes,
                    root: Some(root_str.clone()),
                });
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.trashed_at_ms));
        entries
    }

    /// 回收站条目 `id` 所在的根（下标）和路径：在可用根的 `.trash/` 里按顺序找。
    fn trash_path(&self, id: &str) -> RpcResult<(usize, PathBuf)> {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains('\0') || id == "." || id == ".." {
            return Err(err(ErrorCode::Invalid, "invalid trash id"));
        }
        for (idx, root) in self.active_roots() {
            let p = root.path.join(TRASH_DIR).join(id);
            if std::fs::symlink_metadata(&p).is_ok() {
                return Ok((idx, p));
            }
        }
        Err(err(ErrorCode::NotFound, format!("trash entry {id} not found")))
    }

    /// 还原到原名，回到原来的根（回收站在哪个根，就还原到那个根）。返回还原后的项目名。
    pub async fn restore(&self, id: &str) -> RpcResult<String> {
        let (root_idx, src) = self.trash_path(id)?;
        let (original, _) = parse_trash_id(id).ok_or_else(|| err(ErrorCode::Invalid, "malformed trash id"))?;
        validate_name(&original)?;
        let dst = self.roots[root_idx].path.join(&original);
        {
            let _ns = self.ns_lock.lock().await;
            self.check_name_free(&original)?;
            std::fs::rename(&src, &dst).map_err(|e| err(ErrorCode::Internal, format!("restore: {e}")))?;
        }
        self.rescan_and_notify().await;
        Ok(original)
    }

    /// 永久删除一个回收站条目。
    pub async fn purge(&self, id: &str) -> RpcResult<()> {
        let (_, p) = self.trash_path(id)?;
        let md = std::fs::symlink_metadata(&p).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        let r = if md.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        r.map_err(|e| err(ErrorCode::Internal, format!("purge: {e}")))
    }

    /// 回收站超期条目自动删除（所有可用根的回收站）。返回删除数。`trash_retention_days = 0` 表示不自动清理。
    pub async fn cleanup_trash(&self) -> usize {
        let days = self.config.get().workspace.trash_retention_days;
        if days == 0 {
            return 0;
        }
        let cutoff = now_ms().saturating_sub(days as u64 * 86_400_000);
        let mut n = 0;
        for e in self.trash_list().await {
            if e.trashed_at_ms < cutoff && self.purge(&e.id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    // ---------------- 克隆 ----------------

    /// 开始克隆，返回 job id。进度走 `job.progress`，结束走 `job.done`。`root` 缺省 = 主根，规则同 [`Self::create`]。
    pub async fn clone_start(self: &Arc<Self>, url: &str, name: Option<&str>, root: Option<&str>) -> RpcResult<String> {
        validate_clone_url(url)?;
        let name = match name {
            Some(n) => nfc(n),
            None => nfc(&name_from_url(url)),
        };
        validate_name(&name)?;
        let root_idx = self.resolve_root(root)?;
        let dest = self.roots[root_idx].path.join(&name);
        // 先原子地占住目标目录（空目录），`git clone` 往这个已占好的空目录里克隆；失败时只删这个 job 自己建的目录，
        // 不会误删并发的 create / clone 在同名处建起来的东西
        {
            let _ns = self.ns_lock.lock().await;
            self.check_name_free(&name)?;
            self.claim_dir(&dest, &name)?;
        }
        let job_id = crate::util::new_uuid();
        let started = now_ms();
        if let Err(e) = self
            .store
            .upsert_job(&JobRow {
                id: job_id.clone(),
                kind: "clone".into(),
                target: Some(name.clone()),
                status: "running".into(),
                error: None,
                started_ms: started,
                ended_ms: None,
            })
            .await
        {
            tracing::warn!(error = %e, "cannot record the clone job");
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.jobs.lock().unwrap().insert(job_id.clone(), JobHandle { cancel: cancel_tx });
        let this = Arc::clone(self);
        let url = url.to_string();
        let jid = job_id.clone();
        tokio::spawn(async move {
            let result = this.run_clone(&jid, &url, &dest, cancel_rx).await;
            this.jobs.lock().unwrap().remove(&jid);
            let (ok, error) = match result {
                Ok(()) => (true, None),
                Err(e) => {
                    // 这个目录是本 job 自己建的（claim_dir），失败 / 取消时删掉半成品
                    if let Err(re) = tokio::fs::remove_dir_all(&dest).await {
                        tracing::warn!(error = %re, "cannot remove the half-cloned directory");
                    }
                    (false, Some(e))
                }
            };
            if let Err(e) = this
                .store
                .upsert_job(&JobRow {
                    id: jid.clone(),
                    kind: "clone".into(),
                    target: Some(name.clone()),
                    status: if ok { "done".into() } else { "failed".into() },
                    error: error.clone(),
                    started_ms: started,
                    ended_ms: Some(now_ms()),
                })
                .await
            {
                tracing::warn!(error = %e, "cannot record the clone job result");
            }
            this.rescan_and_notify().await;
            this.clients.broadcast(methods::JOB_DONE, JobDone { job_id: jid, ok, error });
        });
        Ok(job_id)
    }

    async fn run_clone(
        &self,
        job_id: &str,
        url: &str,
        dest: &Path,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let mut cmd = Command::new(crate::gitops::command_program("git", &self.env));
        cmd.args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.file.allow=never",
            "clone",
            "--progress",
            "--",
            url,
        ])
        .arg(dest)
        .env_clear()
        .envs(self.env.iter().filter(|(k, _)| !crate::gitops::GIT_ENV_STRIP.contains(&k.as_str())))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", ":")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("failed to run git: {e}"))?;
        let mut stderr = child.stderr.take().ok_or("no stderr")?;
        let mut buf = [0u8; 4096];
        let mut line = String::new();
        let mut last_lines: Vec<String> = Vec::new();
        self.clients.broadcast(
            methods::JOB_PROGRESS,
            JobProgress { job_id: job_id.to_string(), stage: "starting".into(), percent: Some(0.0), log_line: None },
        );
        loop {
            tokio::select! {
                _ = &mut cancel => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err("cancelled".into());
                }
                n = stderr.read(&mut buf) => {
                    let n = n.map_err(|e| e.to_string())?;
                    if n == 0 { break; }
                    for ch in String::from_utf8_lossy(&buf[..n]).chars() {
                        if ch == '\r' || ch == '\n' {
                            if !line.trim().is_empty() {
                                let l = std::mem::take(&mut line);
                                let l = l.trim().to_string();
                                if let Some(p) = parse_progress_line(&l) {
                                    self.clients.broadcast(methods::JOB_PROGRESS, JobProgress { job_id: job_id.to_string(), stage: p.0, percent: p.1, log_line: Some(l.chars().take(200).collect()) });
                                }
                                last_lines.push(l);
                                if last_lines.len() > 8 { last_lines.remove(0); }
                            } else {
                                line.clear();
                            }
                        } else {
                            line.push(ch);
                        }
                    }
                }
            }
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            let msg = last_lines
                .iter()
                .rev()
                .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
                .cloned()
                .unwrap_or_else(|| last_lines.last().cloned().unwrap_or_else(|| format!("git exited with {status}")));
            Err(msg)
        }
    }

    pub fn cancel_job(&self, job_id: &str) -> RpcResult<()> {
        match self.jobs.lock().unwrap().remove(job_id) {
            Some(h) => {
                let _ = h.cancel.send(());
                Ok(())
            }
            None => Err(err(ErrorCode::NotFound, format!("job {job_id} not found or already finished"))),
        }
    }

    // ---------------- fs（只读） ----------------

    /// 打开之后的复核失败 / 打开失败 -> RPC 错误。
    fn open_error(rel: &str, e: OpenError) -> RpcError {
        match e {
            OpenError::Escaped => err(ErrorCode::Forbidden, "path escapes the project directory"),
            OpenError::WrongKind => err(ErrorCode::Invalid, format!("{rel} is not the expected kind of file")),
            OpenError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                err(ErrorCode::NotFound, format!("{rel} not found"))
            }
            OpenError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                err(ErrorCode::Forbidden, permission_message())
            }
            OpenError::Io(e) => err(ErrorCode::Internal, e.to_string()),
        }
    }

    pub fn fs_list(&self, rel: &str) -> RpcResult<FsListResult> {
        let (dir, boundary) = self.resolve_in(rel)?;
        if !dir.is_dir() {
            return Err(err(ErrorCode::Invalid, format!("{rel} is not a directory")));
        }
        // 按路径打开之后再按句柄的真实路径复核，枚举用打开的 fd（检查之后、打开之前目录被换掉也列不到别处）
        let (real_dir, names) =
            crate::fsopen::list_dir_verified(&dir, &boundary).map_err(|e| Self::open_error(rel, e))?;
        let mut entries = Vec::new();
        for name in names {
            let path = real_dir.join(&name);
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            // 指向边界（根目录，或关联项目自己的目录）之外的符号链接不列出
            let md = if md.file_type().is_symlink() {
                match crate::sys::canonicalize(&path) {
                    Ok(c) if c.starts_with(&boundary) => std::fs::metadata(&c).unwrap_or(md),
                    _ => continue,
                }
            } else {
                md
            };
            let modified_at_ms = mtime_ms(&md);
            entries.push(FsEntry {
                name: name.to_string_lossy().into_owned(),
                is_dir: md.is_dir(),
                bytes: if md.is_dir() { 0 } else { md.len() },
                modified_at_ms,
            });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
        Ok(FsListResult { entries })
    }

    /// `fs.read` 的文本模式（`encoding` 缺省）。
    pub async fn fs_read(&self, rel: &str, max_bytes: Option<u64>) -> RpcResult<FsReadResult> {
        self.fs_read_as(rel, max_bytes, FsReadEncoding::Text, 0).await
    }

    /// 读文件。
    /// - `Text`：前 `max_bytes`（默认 1 MiB，上限 10 MiB）个字节，UTF-8 文本放 `text`，二进制不返回内容。`offset > 0` 是 `invalid`：
    ///   从中间读起会落在半个字符上，而文本模式的「截断在字符中间就保留有效前缀」只管结尾。
    /// - `Base64`：从 `offset` 起至多 `max_bytes`（默认和上限都是一块，[`FS_READ_CHUNK`]）个原始字节，做 base64 放 `data_base64`，
    ///   `text` 为空，并回显 `offset`。`offset` 在文件末尾或之后不是错误：回一个空块（`bytes` 仍是总长，`truncated = false`），
    ///   这样文件在分块读的过程中变短，客户端从 `bytes` 的变化就能看出来。
    ///
    /// 两种模式用同一份路径检查和打开方式。
    pub async fn fs_read_as(
        &self,
        rel: &str,
        max_bytes: Option<u64>,
        encoding: FsReadEncoding,
        offset: u64,
    ) -> RpcResult<FsReadResult> {
        let base64 = encoding == FsReadEncoding::Base64;
        if !base64 && offset > 0 {
            return Err(err(ErrorCode::Invalid, "offset needs encoding \"base64\""));
        }
        let (path, boundary) = self.resolve_in(rel)?;
        crate::fsopen::after_resolve(&path);
        let md = std::fs::metadata(&path).map_err(|e| err(ErrorCode::NotFound, e.to_string()))?;
        if !md.is_file() {
            return Err(err(ErrorCode::Invalid, format!("{rel} is not a regular file")));
        }
        let max = if base64 {
            max_bytes.unwrap_or(FS_READ_CHUNK).min(FS_READ_CHUNK)
        } else {
            max_bytes.unwrap_or(FS_READ_DEFAULT).min(FS_READ_MAX)
        };
        // 从边界目录开始逐段 `openat`（每段 `O_NOFOLLOW`）：检查之后、打开之前目录被换成符号链接读不到边界外；
        // 不再拿「打开之前按路径 stat 的身份」去比（那个身份可能是换目录之后才取的）
        let rel_owned = rel.to_string();
        let (mut buf, total) = tokio::task::spawn_blocking(move || -> RpcResult<(Vec<u8>, u64)> {
            let mut f = crate::fsopen::open_file_in(&path, &boundary).map_err(|e| Self::open_error(&rel_owned, e))?;
            let total = f.metadata().map_err(|e| err(ErrorCode::Internal, e.to_string()))?.len();
            let mut buf = Vec::new();
            // 文件末尾或之后：空块。不去 seek（`u64::MAX` 这样的偏移转成 `off_t` 会溢出，seek 报 `EINVAL`）
            if offset < total {
                if offset > 0 {
                    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset))
                        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                }
                std::io::Read::read_to_end(&mut std::io::Read::take(&mut f, max), &mut buf)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
            }
            Ok((buf, total))
        })
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("fs.read: {e}")))??;
        let truncated = total > offset.saturating_add(buf.len() as u64);
        let text_len = text_prefix_len(&buf, truncated);
        if base64 {
            return Ok(FsReadResult {
                text: String::new(),
                binary: text_len.is_none(),
                truncated,
                bytes: total,
                data_base64: Some(B64.encode(&buf)),
                offset: Some(offset),
            });
        }
        let Some(n) = text_len else {
            return Ok(FsReadResult {
                text: String::new(),
                binary: true,
                truncated,
                bytes: total,
                data_base64: None,
                offset: None,
            });
        };
        buf.truncate(n);
        // `text_prefix_len` 已经保证 `buf[..n]` 是合法的 UTF-8
        let text = String::from_utf8(buf).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        Ok(FsReadResult { text, binary: false, truncated, bytes: total, data_base64: None, offset: None })
    }
}

/// 读到的字节里可以当文本的前缀长度；`None` = 二进制。前 8 KiB 里有 NUL 就是二进制；不是合法 UTF-8 也是二进制，
/// 唯一的例外是读被截断、并且只是最后一个多字节字符被切成了两半（不足 4 字节）：保留有效前缀。
fn text_prefix_len(buf: &[u8], truncated: bool) -> Option<usize> {
    if buf[..buf.len().min(8192)].contains(&0) {
        return None;
    }
    match std::str::from_utf8(buf) {
        Ok(_) => Some(buf.len()),
        Err(e) => {
            let valid = e.valid_up_to();
            (truncated && buf.len() - valid < 4).then_some(valid)
        }
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// 能不能进入并列出目录（[`crate::sys::can_read_dir`]：unix 上只看权限位，不打开目录）。
fn can_read_dir(p: &Path) -> bool {
    crate::sys::can_read_dir(p)
}

/// 浏览时一个目录的状态：能不能关联；不能又不是因为已经是项目时，原因。
fn link_state(ctx: &LinkCtx, canon: &Path, is_project: bool) -> (bool, Option<LinkBlock>) {
    if is_project {
        return (false, None);
    }
    match ctx.verdict(canon) {
        Ok(()) => (true, None),
        Err(_) => (false, ctx.block_of(canon).map(|b| b.block)),
    }
}

fn browse_blocking(ctx: &LinkCtx, target: PathBuf, show_hidden: bool) -> RpcResult<WorkspaceBrowseResult> {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let rd = std::fs::read_dir(&target).map_err(|e| match e.kind() {
        // EACCES / EPERM：macOS 的「文件与文件夹」隐私授权还没给 anycon（弹窗在那台机器的屏幕上）
        std::io::ErrorKind::PermissionDenied => err(ErrorCode::Forbidden, permission_message()),
        _ => err(ErrorCode::NotFound, "cannot read the directory"),
    })?;
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        candidates.push((name, e.path()));
    }
    candidates.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
    let mut entries = Vec::new();
    let mut truncated = false;
    for (i, (name, p)) in candidates.into_iter().enumerate() {
        if i >= BROWSE_MAX_EXAMINED {
            truncated = true;
            break;
        }
        // 跟随符号链接：指向目录的才列；解析后落在范围之外的、无法读的不列
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if !md.is_dir() {
            continue;
        }
        let Ok(canon) = crate::sys::canonicalize(&p) else { continue };
        if !ctx.roots.within(&canon) || !can_read_dir(&canon) {
            continue;
        }
        if entries.len() >= BROWSE_MAX_ENTRIES {
            truncated = true;
            break;
        }
        let project = ctx.project_name(&canon);
        let (linkable, blocked) = link_state(ctx, &canon, project.is_some());
        entries.push(BrowseEntry {
            name,
            // `~/Documents` 之类里面有没有 `.git` 不探测：stat 里面的条目就会触发 macOS 的隐私授权
            git: !ctx.skips_git_probe(&canon) && canon.join(".git").exists(),
            project,
            linkable,
            blocked,
            path: path_string(&canon),
        });
    }
    let parent = if ctx.roots.is_root(&target) { None } else { target.parent().map(path_string) };
    let project = ctx.project_name(&target);
    let (linkable, blocked) = link_state(ctx, &target, project.is_some());
    Ok(WorkspaceBrowseResult {
        path: path_string(&target),
        parent,
        roots: ctx.roots.canon.iter().map(|r| path_string(r)).collect(),
        linkable,
        project,
        blocked,
        entries,
        truncated,
    })
}

/// workspace 根所在的卷是不是不区分大小写：`.trash` 和 `.TRASH` 是不是同一个目录（不写任何东西）。
fn probe_case_insensitive(root: &Path) -> bool {
    let (lower, upper) = (root.join(TRASH_DIR), root.join(TRASH_DIR.to_uppercase()));
    match (crate::sys::identity_of(&lower), crate::sys::identity_of(&upper)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn mtime_ms(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 关联项目的元数据（现算）。目录不在了 / 不是目录 / 无法访问 / 被换成指向别处的符号链接 / 按当前规则已经不允许：
/// `missing`，`git` 为空，`modified_at_ms = 0`（规则 5：使用时复核）。
async fn linked_project_state(
    ctx: &LinkCtx,
    git_svc: &Arc<GitMetaSvc>,
    name: String,
    registered: &Path,
    active_sessions: u32,
) -> (Project, Option<LinkedProblem>) {
    let resolved = ctx.resolve_linked(registered);
    let ((git, git_pending), modified_at_ms, missing) = match &resolved {
        Ok(dir) => {
            let md = std::fs::metadata(dir).ok();
            (git_svc.get(&name, dir).await, md.as_ref().map(mtime_ms).unwrap_or(0), false)
        }
        Err(_) => ((None, false), 0, true),
    };
    let project = Project {
        name,
        path: registered.to_string_lossy().into_owned(),
        root: None,
        git,
        modified_at_ms,
        active_sessions,
        kind: ProjectKind::Linked,
        missing,
        git_pending,
    };
    (project, resolved.err())
}

fn parse_trash_id(id: &str) -> Option<(String, u64)> {
    if id.len() < 18 {
        return None;
    }
    let (head, ts) = id.split_at(id.len() - 16);
    let name = head.strip_suffix('-')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), parse_compact(ts)?))
}

/// 递归统计目录大小（阻塞 IO 放到 blocking 线程；超过 200k 个条目就停止，避免巨型目录拖住）。
async fn dir_size(p: &Path) -> u64 {
    let p = p.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut total = 0u64;
        let mut count = 0usize;
        let mut stack = vec![p];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                count += 1;
                if count > 200_000 {
                    return total;
                }
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    stack.push(e.path());
                } else {
                    total += md.len();
                }
            }
        }
        total
    })
    .await
    .unwrap_or(0)
}

/// 解析 `git clone --progress` 的 stderr 行，返回 (stage, percent)。
fn parse_progress_line(line: &str) -> Option<(String, Option<f32>)> {
    let stages = [
        ("Counting objects", "counting"),
        ("Compressing objects", "compressing"),
        ("Receiving objects", "receiving"),
        ("Resolving deltas", "resolving"),
        ("Updating files", "updating"),
        ("Checking out files", "updating"),
    ];
    for (prefix, stage) in stages {
        if line.starts_with(prefix) {
            let pct = line
                .split('%')
                .next()
                .and_then(|h| h.rsplit(|c: char| !(c.is_ascii_digit() || c == '.')).next())
                .and_then(|n| n.parse::<f32>().ok());
            return Some((stage.to_string(), pct));
        }
    }
    if line.starts_with("Cloning into") {
        return Some(("cloning".into(), Some(0.0)));
    }
    None
}

/// daemon 自己跑的每一条 `git` 都带的 `-c`：不执行仓库自己配置的 `core.fsmonitor`，也不跑它的 hook。
/// 关联功能让 `~` 下任何目录（比如解压出来的仓库）都能被选成项目，`workspace.list` 会在里面跑 `git status`。
/// clean filter 没法通配关闭，是已知限制（daemon.md § 10）。
pub const GIT_SAFE_CONFIG: [&str; 4] = ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"];

/// 完整的 `git` 参数（安全配置在前，子命令在后）。
pub fn git_invocation(args: &[&str]) -> Vec<String> {
    GIT_SAFE_CONFIG.iter().chain(args.iter()).map(|a| a.to_string()).collect()
}

async fn run_git(
    env: &HashMap<String, String>,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new(crate::gitops::command_program("git", env));
    cmd.args(git_invocation(args))
        .env_clear()
        .envs(env.iter().filter(|(k, _)| !crate::gitops::GIT_ENV_STRIP.contains(&k.as_str())))
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", ":")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let child = cmd.output();
    match tokio::time::timeout(timeout, child).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "git timed out")),
    }
}

/// `GitMeta.worktree_ready`（ADR-34）：项目目录（canonical）就是 `git rev-parse --show-toplevel`（canonical）——不是仓库里的
/// 子目录——并且 `git rev-parse --verify HEAD` 成功（至少有一个提交）。每次现算，不走缓存（建 worktree 前的同步校验用它）。
pub async fn worktree_ready_fresh(env: &HashMap<String, String>, path: &Path) -> bool {
    let (top, head) = tokio::join!(
        run_git(env, &["rev-parse", "--show-toplevel"], Some(path), GIT_HARD_TIMEOUT),
        run_git(env, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], Some(path), GIT_HARD_TIMEOUT),
    );
    let head_ok = matches!(&head, Ok(o) if o.status.success());
    let Ok(top) = top else { return false };
    if !top.status.success() || !head_ok {
        return false;
    }
    let top = String::from_utf8_lossy(&top.stdout).trim().to_string();
    match (crate::sys::canonicalize(&top), crate::sys::canonicalize(path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

async fn git_meta_fresh(env: &HashMap<String, String>, path: &Path) -> Option<GitMeta> {
    if !path.join(".git").exists() {
        return None;
    }
    let (branch, status, remote, worktree_ready) = tokio::join!(
        run_git(env, &["symbolic-ref", "--short", "-q", "HEAD"], Some(path), GIT_HARD_TIMEOUT),
        run_git(env, &["status", "--porcelain"], Some(path), GIT_HARD_TIMEOUT),
        run_git(env, &["config", "--get", "remote.origin.url"], Some(path), GIT_HARD_TIMEOUT),
        worktree_ready_fresh(env, path),
    );
    let branch = match branch {
        Ok(o) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
        }
        _ => {
            // 游离 HEAD：给短 hash
            match run_git(env, &["rev-parse", "--short", "HEAD"], Some(path), GIT_HARD_TIMEOUT).await {
                Ok(o) if o.status.success() => {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
                }
                _ => None,
            }
        }
    };
    let dirty = matches!(&status, Ok(o) if o.status.success() && !o.stdout.is_empty());
    let remote = match remote {
        Ok(o) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
        }
        _ => None,
    };
    Some(GitMeta { branch, dirty, remote, worktree_ready })
}

/// 一次正在跑的 git 元数据计算。同一个目录的并发请求共用它。
struct Inflight {
    /// `None` = 还在算；`Some(结果)` = 算完了。
    done: watch::Sender<Option<Option<GitMeta>>>,
    /// 冷缓存时前台等超时、先返回了没有 `git` 的项目名：算完之后发 `workspace.changed { updated }` 通知这些项目（同名去重）。
    late: Mutex<BTreeSet<String>>,
}

/// 项目的 git 元数据：5 秒缓存、前台预算和「冷缓存」处理（daemon.md § 10「元数据」）。
///
/// - 有缓存（哪怕过期）：前台最多等 [`GIT_FRONT_BUDGET`]，超了先返回旧值，后台算完更新缓存；
/// - **没有缓存**（daemon 刚（重）启动、新项目）：前台最多等 [`GIT_COLD_BUDGET`]（`workspace.list` 对各项目并行，所以整个
///   list 也是这个量级）。超时的先不带 `git` 返回，后台算完之后发 `workspace.changed { updated: [名字] }`，客户端重新拉。
///   以前冷缓存也只等 50 ms，每个项目要 5–6 个 git 子进程，第一次 list 几乎必然拿不到 `git`，Web 因此悄悄把新会话建成
///   「当前目录」。
struct GitMetaSvc {
    cache: Mutex<HashMap<PathBuf, (Instant, GitMeta)>>,
    env: Arc<HashMap<String, String>>,
    clients: Arc<Clients>,
    /// 正在算的（按目录）。
    inflight: Mutex<HashMap<PathBuf, Arc<Inflight>>>,
    /// 冷缓存时前台最多等多久（毫秒）；测试里可以缩短。
    cold_budget_ms: std::sync::atomic::AtomicU64,
    /// `workspace.changed { updated }` 的防抖时长（毫秒）；测试里可以加长。
    updated_debounce_ms: std::sync::atomic::AtomicU64,
    /// 算完了、等着合并成一条 `workspace.changed { updated }` 发出去的项目名（200 ms 防抖）。
    notify_queue: Mutex<BTreeSet<String>>,
}

impl GitMetaSvc {
    fn new(env: Arc<HashMap<String, String>>, clients: Arc<Clients>) -> GitMetaSvc {
        GitMetaSvc {
            cache: Mutex::new(HashMap::new()),
            env,
            clients,
            inflight: Mutex::new(HashMap::new()),
            cold_budget_ms: std::sync::atomic::AtomicU64::new(GIT_COLD_BUDGET.as_millis() as u64),
            updated_debounce_ms: std::sync::atomic::AtomicU64::new(GIT_UPDATED_DEBOUNCE.as_millis() as u64),
            notify_queue: Mutex::new(BTreeSet::new()),
        }
    }

    /// 把算完的项目名排进队列；200 ms 内算完的合并成一条 `workspace.changed { updated }`（N 个慢项目不会变成 N 次整表重拉）。
    /// 缓存早在入队之前就写好了，所以收到通知后的 `workspace.list` 一定带 `git`。
    fn queue_updated(self: &Arc<Self>, names: BTreeSet<String>) {
        let first = {
            let mut q = self.notify_queue.lock().unwrap();
            let was_empty = q.is_empty();
            q.extend(names);
            was_empty
        };
        if !first {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(
                this.updated_debounce_ms.load(std::sync::atomic::Ordering::Relaxed),
            ))
            .await;
            let updated: Vec<String> = std::mem::take(&mut *this.notify_queue.lock().unwrap()).into_iter().collect();
            if !updated.is_empty() {
                this.clients.broadcast(methods::WORKSPACE_CHANGED, &WorkspaceChanged { updated, ..Default::default() });
            }
        });
    }

    fn forget(&self, path: &Path) {
        self.cache.lock().unwrap().remove(path);
    }

    fn cached(&self, path: &Path) -> Option<GitMeta> {
        self.cache.lock().unwrap().get(path).map(|(_, m)| m.clone())
    }

    /// 同一个目录正在算就加入它，否则起一次计算。算完后：写缓存 → 摘掉 inflight（同时取走 `late`）→ 放行等待者 → 通知。
    fn start_or_join(self: &Arc<Self>, path: &Path) -> (Arc<Inflight>, watch::Receiver<Option<Option<GitMeta>>>) {
        let mut map = self.inflight.lock().unwrap();
        if let Some(f) = map.get(path) {
            return (f.clone(), f.done.subscribe());
        }
        let (tx, rx) = watch::channel(None);
        let flight = Arc::new(Inflight { done: tx, late: Mutex::new(BTreeSet::new()) });
        map.insert(path.to_path_buf(), flight.clone());
        drop(map);
        let (this, path, f) = (self.clone(), path.to_path_buf(), flight.clone());
        tokio::spawn(async move {
            // 里面再起一个任务：就算计算 panic，inflight 也一定会被摘掉，等待者不会永远等下去
            let env = this.env.clone();
            let p = path.clone();
            let meta = tokio::spawn(async move { git_meta_fresh(&env, &p).await }).await.ok().flatten();
            if let Some(m) = &meta {
                this.cache.lock().unwrap().insert(path.clone(), (Instant::now(), m.clone()));
            }
            let late = {
                let mut map = this.inflight.lock().unwrap();
                map.remove(&path);
                std::mem::take(&mut *f.late.lock().unwrap())
            };
            f.done.send_replace(Some(meta));
            // 算出来没有（目录里的 `.git` 中途没了）也要通知：客户端重新拉，看到的就是「不是 git 项目、不在算」
            if !late.is_empty() {
                this.queue_updated(late);
            }
        });
        (flight, rx)
    }

    /// 前台超时之后登记「算完要通知 `name`」。返回 `false` = 登记时这次计算刚好算完了（结果已经在缓存里）。
    fn mark_late(&self, path: &Path, flight: &Arc<Inflight>, name: &str) -> bool {
        let map = self.inflight.lock().unwrap();
        match map.get(path) {
            Some(f) if Arc::ptr_eq(f, flight) => {
                flight.late.lock().unwrap().insert(name.to_string());
                true
            }
            _ => false,
        }
    }

    /// (元数据, 还在算)。第二项为真 = 冷缓存、前台等待超时、后台还没算完：`git` 为空**不代表不是 git 项目**（`Project.git_pending`）。
    /// 后台没算完之前再来的请求同样给这个结果（它们加入同一次计算，各自再等一个预算）。
    async fn get(self: &Arc<Self>, name: &str, path: &Path) -> (Option<GitMeta>, bool) {
        if !path.join(".git").exists() {
            return (None, false);
        }
        let stale = {
            let c = self.cache.lock().unwrap();
            match c.get(path) {
                Some((at, meta)) if at.elapsed() < GIT_CACHE_TTL => return (Some(meta.clone()), false),
                Some((_, meta)) => Some(meta.clone()),
                None => None,
            }
        };
        let (flight, mut rx) = self.start_or_join(path);
        let budget = if stale.is_some() {
            GIT_FRONT_BUDGET
        } else {
            Duration::from_millis(self.cold_budget_ms.load(std::sync::atomic::Ordering::Relaxed))
        };
        let wait = async {
            loop {
                if let Some(v) = rx.borrow_and_update().clone() {
                    return v;
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        };
        match tokio::time::timeout(budget, wait).await {
            Ok(meta) => (meta.or(stale), false),
            Err(_) if stale.is_some() => (stale, false),
            Err(_) => {
                if self.mark_late(path, &flight, name) {
                    (None, true)
                } else {
                    // 登记时这次计算刚好算完：结果已经在缓存里
                    (self.cached(path), false)
                }
            }
        }
    }
}

/// 扫描到的一个根下项目。
struct Scanned {
    /// 盘上的原样名字。
    name: String,
    /// 所在的根（`Workspace::roots` 的下标）。
    root: usize,
    /// 目录身份 `(dev, ino)`。
    id: (u64, u64),
}

/// 谁盖住了一个同名文件夹。
enum ShadowedBy {
    /// 关联项目（最高优先级）。
    Linked,
    /// 更靠前的根（下标）里的同名项目。
    Root(usize),
}

/// 被盖住、不出现在列表里的同名文件夹。
struct Shadowed {
    name: String,
    /// 被盖住的文件夹所在的根。
    root: usize,
    by: ShadowedBy,
}

/// 根 `root` 下的 `p` 是不是项目目录：跟随符号链接是目录，且 canonical 路径还在 `root` 之内（指向根外的不算）。
/// 是就返回它的元数据（跟随符号链接）。
fn project_dir_metadata(root: &Path, p: &Path) -> Option<std::fs::Metadata> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_dir() {
        return None;
    }
    match crate::sys::canonicalize(p) {
        Ok(canon) if !canon.starts_with(root) => None,
        _ => Some(md),
    }
}

/// 文件系统事件值不值得重新扫描：只关心某个根下一级的非隐藏条目（`.trash` / `.scratch` 里的变化不是项目增删）和根本身；
/// 不在任何根之下的路径（平台报出的奇怪路径）保守地算相关。根之间不嵌套，所以一个路径至多在一个根之下。
fn event_is_relevant(roots: &[PathBuf], p: &Path) -> bool {
    for root in roots {
        if let Ok(rel) = p.strip_prefix(root) {
            let mut comps = rel.components();
            return match (comps.next(), comps.next()) {
                (Some(Component::Normal(first)), None) => !first.to_string_lossy().starts_with('.'),
                (None, _) => true,
                _ => false,
            };
        }
    }
    true
}

/// 项目名集合（测试用）。
pub fn names_of(list: &WorkspaceListResult) -> HashSet<String> {
    list.projects.iter().map(|p| p.name.clone()).collect()
}

// 这些测试大量用到符号链接、权限位和 `/var` 这类 unix 路径；Windows 上的路径规则由 `linkrules` 的 Windows 测试和集成测试覆盖
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::clients::testing::ChanSink;
    use crate::clients::Caller;
    use crate::config::{Config, Paths};
    use crate::linkrules::{expand_home, lexical_clean, system_aliases};
    use anycon_protocol::RpcNotification;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    struct Fixture {
        _d: tempfile::TempDir,
        ws: Arc<Workspace>,
        rx: mpsc::UnboundedReceiver<RpcNotification>,
        root: PathBuf,
    }

    async fn fixture() -> Fixture {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("ws");
        let paths = Paths::new(d.path().join("home"));
        std::fs::create_dir_all(&paths.home).unwrap();
        let store = Store::open(&paths.db()).await.unwrap();
        let config = ConfigStore::with_config(paths, Config::default());
        let clients = Clients::new();
        let (tx, rx) = mpsc::unbounded_channel();
        clients.register(Caller::local_cli(), Box::new(ChanSink(tx)));
        let env: HashMap<String, String> = std::env::vars().collect();
        let ws = Workspace::new(&root, config, store, clients, Arc::new(env)).await.unwrap();
        let root = ws.root().to_path_buf();
        Fixture { _d: d, ws, rx, root }
    }

    async fn next_changed(
        rx: &mut mpsc::UnboundedReceiver<RpcNotification>,
        method: &str,
        timeout: Duration,
    ) -> Option<RpcNotification> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(n)) if n.method == method => return Some(n),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    #[test]
    fn name_validation() {
        for ok in ["my-app", "a b", "项目", "x.y", "a_b"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "a\0b", ".trash", ".scratch", "..", "a\\b", &"x".repeat(256)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"x".repeat(255)).is_ok());
    }

    #[test]
    fn clone_urls() {
        for ok in ["https://github.com/a/b.git", "ssh://git@github.com/a/b", "git@github.com:a/b.git"] {
            assert!(validate_clone_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "file:///tmp/x",
            "ext::sh -c id",
            "http://x/y",
            "-oProxyCommand=x",
            "https://",
            "git@host",
            "https://a b",
            "../x",
            "ftp://x/y",
        ] {
            assert!(validate_clone_url(bad).is_err(), "{bad:?}");
        }
        assert_eq!(name_from_url("https://github.com/a/b.git"), "b");
        assert_eq!(name_from_url("git@github.com:a/my-repo.git"), "my-repo");
        assert_eq!(name_from_url("https://x/y/z/"), "z");
    }

    #[test]
    fn progress_parsing() {
        assert_eq!(
            parse_progress_line("Receiving objects:  45% (45/100), 1.2 MiB | 2 MiB/s").unwrap(),
            ("receiving".to_string(), Some(45.0))
        );
        assert_eq!(parse_progress_line("Resolving deltas: 100% (3/3), done.").unwrap().1, Some(100.0));
        assert!(parse_progress_line("remote: Enumerating objects").is_none());
    }

    #[tokio::test]
    async fn create_list_rename_trash_restore_purge() {
        let mut f = fixture().await;
        let ws = f.ws.clone();
        assert!(f.root.join(TRASH_DIR).is_dir() && f.root.join(SCRATCH_DIR).is_dir());

        ws.create("alpha", false, None).await.unwrap();
        let n = next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(2)).await.expect("added event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.added, vec!["alpha".to_string()]);
        assert_eq!(ws.create("alpha", false, None).await.unwrap_err().stable_code(), Some(ErrorCode::Exists));
        assert_eq!(ws.create("../evil", false, None).await.unwrap_err().stable_code(), Some(ErrorCode::Invalid));
        assert_eq!(ws.create(".trash", false, None).await.unwrap_err().stable_code(), Some(ErrorCode::Invalid));

        let mut active = HashMap::new();
        active.insert("alpha".to_string(), 2);
        let list = ws.list(&active).await;
        assert_eq!(list.projects.len(), 1);
        assert_eq!(list.projects[0].name, "alpha");
        assert_eq!(list.projects[0].active_sessions, 2);
        assert!(list.projects[0].git.is_none());

        // 改名：inode 相同 -> renamed
        ws.rename("alpha", "beta").await.unwrap();
        let n =
            next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(2)).await.expect("renamed event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.renamed, vec![RenamedProject { from: "alpha".into(), to: "beta".into() }]);
        assert!(ch.added.is_empty() && ch.removed.is_empty());
        assert_eq!(ws.rename("alpha", "gamma").await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
        ws.create("gamma", false, None).await.unwrap();
        assert_eq!(ws.rename("beta", "gamma").await.unwrap_err().stable_code(), Some(ErrorCode::Exists));

        // 回收站
        std::fs::write(f.root.join("beta").join("f.txt"), "hello").unwrap();
        let entry = ws.trash("beta").await.unwrap();
        assert_eq!(entry.original_name, "beta");
        assert!(entry.id.starts_with("beta-") && entry.bytes == 5, "{entry:?}");
        assert!(!f.root.join("beta").exists());
        let tl = ws.trash_list().await;
        assert_eq!(tl.len(), 1);
        assert_eq!(tl[0].id, entry.id);
        // 同名占用时还原失败
        ws.create("beta", false, None).await.unwrap();
        assert_eq!(ws.restore(&entry.id).await.unwrap_err().stable_code(), Some(ErrorCode::Exists));
        ws.trash("beta").await.unwrap_err(); // 同一秒内第二次回收同名：Exists（时间戳相同）或成功都可接受
        let _ = std::fs::remove_dir_all(f.root.join("beta"));
        assert_eq!(ws.restore(&entry.id).await.unwrap(), "beta");
        assert_eq!(std::fs::read_to_string(f.root.join("beta/f.txt")).unwrap(), "hello");
        // 清除
        let entry2 = ws.trash("beta").await.unwrap();
        assert_eq!(ws.restore("../etc").await.unwrap_err().stable_code(), Some(ErrorCode::Invalid));
        ws.purge(&entry2.id).await.unwrap();
        assert!(ws.trash_list().await.is_empty());
        assert_eq!(ws.purge(&entry2.id).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn path_escape_is_rejected() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("proj", false, None).await.unwrap();
        std::fs::write(f.root.join("proj/a.txt"), "x").unwrap();
        let outside = f._d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top secret").unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join("proj/link")).unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join("linked-project")).unwrap();

        for bad in [
            "../outside/secret.txt",
            "proj/../../outside",
            "/etc/passwd",
            "proj/link/secret.txt",
            "linked-project/secret.txt",
            "a\0b",
        ] {
            let e = ws.fs_read(bad, None).await.unwrap_err();
            assert!(
                matches!(
                    e.stable_code(),
                    Some(ErrorCode::Forbidden) | Some(ErrorCode::Invalid) | Some(ErrorCode::NotFound)
                ),
                "{bad}: {e:?}"
            );
        }
        assert_eq!(
            ws.fs_read("proj/link/secret.txt", None).await.unwrap_err().stable_code(),
            Some(ErrorCode::Forbidden)
        );
        assert_eq!(
            ws.fs_read("../outside/secret.txt", None).await.unwrap_err().stable_code(),
            Some(ErrorCode::Forbidden)
        );
        assert_eq!(ws.fs_read("/etc/passwd", None).await.unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
        assert_eq!(ws.project_path("linked-project").unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
        assert!(ws.fs_list("proj/link").is_err());
        // 符号链接目标在根外：list 不列出，且不当作项目
        let list = ws.list(&HashMap::new()).await;
        assert_eq!(names_of(&list), HashSet::from(["proj".to_string()]));
        let entries = ws.fs_list("proj").unwrap().entries;
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["a.txt"]);

        // 正常读
        let r = ws.fs_read("proj/a.txt", None).await.unwrap();
        assert_eq!((r.text.as_str(), r.binary, r.truncated, r.bytes), ("x", false, false, 1));
        let root_list = ws.fs_list("").unwrap();
        assert!(root_list.entries.iter().any(|e| e.name == "proj" && e.is_dir));
        assert!(ws.fs_list("proj/a.txt").is_err());
    }

    #[tokio::test]
    async fn fs_read_limits_and_binary() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        std::fs::write(f.root.join("p/big.txt"), "a".repeat(5000)).unwrap();
        std::fs::write(f.root.join("p/bin.dat"), [0u8, 1, 2, 255, 0]).unwrap();
        std::fs::write(f.root.join("p/cjk.txt"), "你好世界").unwrap();
        let r = ws.fs_read("p/big.txt", Some(100)).await.unwrap();
        assert!(r.truncated && r.text.len() == 100 && r.bytes == 5000);
        let r = ws.fs_read("p/bin.dat", None).await.unwrap();
        assert!(r.binary && r.text.is_empty());
        // 截断在多字节字符中间：保留有效前缀
        let r = ws.fs_read("p/cjk.txt", Some(4)).await.unwrap();
        assert!(!r.binary && r.truncated && r.text == "你");
        let r = ws.fs_read("p/cjk.txt", None).await.unwrap();
        assert_eq!(r.text, "你好世界");
    }

    /// base64 模式：读到的原始字节（二进制 / 文本都一样）放进 `data_base64`，`text` 为空，并回显 `offset`。
    #[tokio::test]
    async fn fs_read_base64_returns_the_raw_bytes() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        // PNG 文件头：含 NUL 和 0x89，不是合法的 UTF-8
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R'];
        std::fs::write(f.root.join("p/a.png"), &png).unwrap();
        std::fs::write(f.root.join("p/logo.svg"), "<svg/>").unwrap();
        std::fs::write(f.root.join("p/cjk.txt"), "你好世界").unwrap();

        let r = ws.fs_read_as("p/a.png", None, FsReadEncoding::Base64, 0).await.unwrap();
        assert_eq!(B64.decode(r.data_base64.as_deref().unwrap()).unwrap(), png);
        assert!(r.text.is_empty() && r.binary && !r.truncated && r.bytes == png.len() as u64);
        assert_eq!(r.offset, Some(0));
        // 文本文件：字节原样，`binary` 仍是 false，`text` 不填
        let r = ws.fs_read_as("p/logo.svg", None, FsReadEncoding::Base64, 0).await.unwrap();
        assert_eq!(B64.decode(r.data_base64.as_deref().unwrap()).unwrap(), b"<svg/>");
        assert!(r.text.is_empty() && !r.binary);
        // 截断：只给前 max_bytes 个字节（截在多字节字符中间也照样按字节切），`bytes` 是文件总长
        let r = ws.fs_read_as("p/cjk.txt", Some(4), FsReadEncoding::Base64, 0).await.unwrap();
        assert_eq!(B64.decode(r.data_base64.as_deref().unwrap()).unwrap(), "你好世界".as_bytes()[..4]);
        assert!(r.truncated && r.bytes == 12 && !r.binary, "{r:?}");
        // 文本模式不带 `data_base64` / `offset`，行为不变
        let r = ws.fs_read("p/a.png", None).await.unwrap();
        assert!(r.binary && r.data_base64.is_none() && r.offset.is_none());
        let r = ws.fs_read("p/logo.svg", None).await.unwrap();
        assert!(r.text == "<svg/>" && r.data_base64.is_none() && r.offset.is_none());
        // 路径检查和文本模式一样
        for bad in ["../x", "/etc/passwd", "p"] {
            assert!(ws.fs_read_as(bad, None, FsReadEncoding::Base64, 0).await.is_err(), "{bad}");
        }
    }

    /// base64 模式一次最多一块（512 KiB），`max_bytes` 更大的按一块算，缺省也是一块；`bytes` 一直是总长。
    #[tokio::test]
    async fn fs_read_base64_returns_at_most_one_chunk_per_call() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        std::fs::write(f.root.join("p/big.bin"), vec![7u8; FS_READ_MAX as usize + 1]).unwrap();
        let len =
            |r: &anycon_protocol::FsReadResult| B64.decode(r.data_base64.as_deref().unwrap()).unwrap().len() as u64;
        for max in [None, Some(u64::MAX), Some(FS_READ_MAX), Some(FS_READ_CHUNK + 1), Some(FS_READ_CHUNK)] {
            let r = ws.fs_read_as("p/big.bin", max, FsReadEncoding::Base64, 0).await.unwrap();
            assert_eq!(len(&r), FS_READ_CHUNK, "max_bytes {max:?}");
            assert!(r.truncated && r.bytes == FS_READ_MAX + 1);
        }
        // 小于一块的照给；0 只回大小
        let r = ws.fs_read_as("p/big.bin", Some(10), FsReadEncoding::Base64, 0).await.unwrap();
        assert_eq!(len(&r), 10);
        let r = ws.fs_read_as("p/big.bin", Some(0), FsReadEncoding::Base64, 0).await.unwrap();
        assert_eq!((len(&r), r.truncated, r.bytes), (0, true, FS_READ_MAX + 1));
        // 文本模式的上限不变：缺省 1 MiB，最大 10 MiB
        assert_eq!(ws.fs_read("p/big.bin", None).await.unwrap().bytes, FS_READ_MAX + 1);
        assert_eq!(ws.fs_read("p/big.bin", Some(u64::MAX)).await.unwrap().text.len() as u64, FS_READ_MAX);
        assert_eq!(ws.fs_read("p/big.bin", None).await.unwrap().text.len() as u64, FS_READ_DEFAULT);
    }

    /// 用 `offset` 一块一块读完整个文件，拼起来和原文件一样；`truncated` 只有最后一块是 false，每一块的 `bytes` 都是总长。
    #[tokio::test]
    async fn fs_read_base64_reads_a_file_in_chunks_with_offset() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        let chunk = FS_READ_CHUNK as usize;
        let data: Vec<u8> = (0..chunk * 2 + 1234).map(|i| (i % 251) as u8).collect();
        std::fs::write(f.root.join("p/img.bin"), &data).unwrap();
        let mut got = Vec::new();
        let mut calls = Vec::new();
        loop {
            let r = ws.fs_read_as("p/img.bin", None, FsReadEncoding::Base64, got.len() as u64).await.unwrap();
            assert_eq!((r.offset, r.bytes), (Some(got.len() as u64), data.len() as u64));
            got.extend(B64.decode(r.data_base64.unwrap()).unwrap());
            calls.push(r.truncated);
            if !r.truncated {
                break;
            }
            assert!(calls.len() < 10);
        }
        assert_eq!(calls, vec![true, true, false]);
        assert_eq!(got, data);
        // 不是块的整数倍的偏移也行，`max_bytes` 照样生效
        let r = ws.fs_read_as("p/img.bin", Some(100), FsReadEncoding::Base64, 1000).await.unwrap();
        assert_eq!(B64.decode(r.data_base64.unwrap()).unwrap(), data[1000..1100]);
        assert!(r.truncated);
        // 刚好读到末尾的那一块不算「还有」
        let r = ws.fs_read_as("p/img.bin", None, FsReadEncoding::Base64, data.len() as u64 - 10).await.unwrap();
        assert_eq!(B64.decode(r.data_base64.unwrap()).unwrap(), data[data.len() - 10..]);
        assert!(!r.truncated);
    }

    /// `offset` 在文件末尾或之后不是错误：回空块、`truncated = false`、`bytes` 是当前的总长（文件在分块读的时候变短，
    /// 客户端从 `bytes` 的变化看得出来）。
    #[tokio::test]
    async fn fs_read_base64_offset_at_or_past_the_end_gives_an_empty_chunk() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        std::fs::write(f.root.join("p/a.bin"), [1u8, 2, 3, 4, 5]).unwrap();
        std::fs::write(f.root.join("p/empty.bin"), []).unwrap();
        for (path, offset, total) in [
            ("p/a.bin", 5u64, 5u64),
            ("p/a.bin", 6, 5),
            ("p/a.bin", u64::MAX, 5),
            ("p/empty.bin", 0, 0),
            ("p/empty.bin", 7, 0),
        ] {
            let r = ws.fs_read_as(path, None, FsReadEncoding::Base64, offset).await.unwrap();
            assert_eq!(r.data_base64.as_deref(), Some(""), "{path} @{offset}");
            assert_eq!(
                (r.truncated, r.bytes, r.offset, r.text.as_str()),
                (false, total, Some(offset), ""),
                "{path} @{offset}"
            );
        }
    }

    /// 文本模式不支持 `offset`：从中间读起会落在半个字符上。`offset = 0` 照旧。
    #[tokio::test]
    async fn fs_read_text_rejects_a_nonzero_offset() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("p", false, None).await.unwrap();
        std::fs::write(f.root.join("p/a.txt"), "hello").unwrap();
        let e = ws.fs_read_as("p/a.txt", None, FsReadEncoding::Text, 1).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
        assert_eq!(ws.fs_read_as("p/a.txt", None, FsReadEncoding::Text, 0).await.unwrap().text, "hello");
        // 路径不存在时先报参数错误，不泄露存在性
        assert_eq!(
            ws.fs_read_as("p/none.txt", None, FsReadEncoding::Text, 1).await.unwrap_err().stable_code(),
            Some(ErrorCode::Invalid)
        );
    }

    /// Web 的文件浏览要能打开会话的工作目录：worktree 在 `<项目>/.anycon/worktrees/<名字>`，scratch 会话在 `.scratch/<id>`。
    /// 这两条路径都含以 `.` 开头的段，`fs.*` 不能因此拒绝（隐藏目录的限制只用于关联目录，见 daemon.md § 10）。
    #[tokio::test]
    async fn fs_opens_session_working_directories() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("proj", false, None).await.unwrap();
        let wt = f.root.join("proj/.anycon/worktrees/fix-login");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/main.rs"), "fn main() {}").unwrap();
        let names = |l: FsListResult| l.entries.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(ws.fs_list("proj/.anycon/worktrees/fix-login").unwrap()), vec!["src"]);
        assert_eq!(
            ws.fs_read("proj/.anycon/worktrees/fix-login/src/main.rs", None).await.unwrap().text,
            "fn main() {}"
        );
        // 项目目录里这些目录是可以列出来的
        assert_eq!(names(ws.fs_list("proj").unwrap()), vec![".anycon"]);
        assert_eq!(names(ws.fs_list("proj/.anycon").unwrap()), vec!["worktrees"]);

        let scratch = ws.scratch_dir("sess_abc").unwrap();
        std::fs::write(scratch.join("notes.md"), "# hi").unwrap();
        assert_eq!(names(ws.fs_list(".scratch/sess_abc").unwrap()), vec!["notes.md"]);
        assert_eq!(ws.fs_read(".scratch/sess_abc/notes.md", None).await.unwrap().text, "# hi");
        // 仍然出不了 workspace 根
        assert_eq!(code_of(ws.fs_list(".scratch/../../x")), ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn watcher_reports_external_changes_with_debounce() {
        let mut f = fixture().await;
        let _guard = f.ws.spawn_watcher().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 外部创建三个目录，应合并成一次事件
        for n in ["w1", "w2", "w3"] {
            std::fs::create_dir(f.root.join(n)).unwrap();
        }
        let n =
            next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("watcher event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.added, vec!["w1".to_string(), "w2".to_string(), "w3".to_string()]);
        // 外部删除
        std::fs::remove_dir(f.root.join("w2")).unwrap();
        let n =
            next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("watcher removal");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.removed, vec!["w2".to_string()]);
        // 外部改名（mv）：识别为 renamed
        std::fs::rename(f.root.join("w1"), f.root.join("w1-renamed")).unwrap();
        let n =
            next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("watcher rename");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.renamed, vec![RenamedProject { from: "w1".into(), to: "w1-renamed".into() }]);
        // .scratch 里的变化不产生事件
        std::fs::create_dir(f.root.join(SCRATCH_DIR).join("sess_x")).unwrap();
        assert!(next_changed(&mut f.rx, methods::WORKSPACE_CHANGED, Duration::from_millis(1200)).await.is_none());
    }

    #[tokio::test]
    async fn git_metadata_and_init() {
        let f = fixture().await;
        let ws = f.ws.clone();
        ws.create("repo", true, None).await.unwrap();
        assert!(f.root.join("repo/.git").exists());
        // 第一次可能超过 50ms 预算而返回 None；后台任务会填缓存，稍后再取
        let mut meta = None;
        for _ in 0..40 {
            let p = ws.project("repo", 0).await.unwrap();
            if let Some(g) = p.git {
                meta = Some(g);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let g = meta.expect("git meta eventually available");
        assert!(!g.dirty);
        assert!(g.remote.is_none());
        std::fs::write(f.root.join("repo/new.txt"), "x").unwrap();
        // 缓存 5 秒内仍是旧值；这里只验证非 git 目录没有 meta
        ws.create("plain", false, None).await.unwrap();
        assert!(ws.project("plain", 0).await.unwrap().git.is_none());
    }

    #[tokio::test]
    async fn clone_rejects_bad_urls_and_existing_names() {
        let f = fixture().await;
        let ws = f.ws.clone();
        assert_eq!(
            ws.clone_start("file:///etc", None, None).await.unwrap_err().stable_code(),
            Some(ErrorCode::Invalid)
        );
        ws.create("repo", false, None).await.unwrap();
        assert_eq!(
            ws.clone_start("https://example.invalid/x/repo.git", None, None).await.unwrap_err().stable_code(),
            Some(ErrorCode::Exists)
        );
        assert_eq!(ws.cancel_job("nope").unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn clone_claims_its_target_atomically_and_a_failure_removes_only_that_directory() {
        let f = fixture().await;
        // 克隆进行中，目标目录已经被占住：同名的 create / clone 立刻得到 exists，不会互相踩
        let job = f.ws.clone_start("ssh://127.0.0.1:1/x/claimed.git", Some("claimed"), None).await.unwrap();
        assert!(f.root.join("claimed").is_dir(), "the target directory is created up front");
        let e = f.ws.create("claimed", false, None).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Exists));
        let e = f.ws.clone_start("ssh://127.0.0.1:1/x/claimed.git", Some("claimed"), None).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Exists));
        // 已经存在的同名目录（别人的）永远不会被这个 job 删掉：clone_start 在占位时就失败了
        std::fs::create_dir(f.root.join("mine")).unwrap();
        std::fs::write(f.root.join("mine").join("keep.txt"), "x").unwrap();
        let e = f.ws.clone_start("ssh://127.0.0.1:1/x/mine.git", Some("mine"), None).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Exists));
        assert!(f.root.join("mine").join("keep.txt").is_file());
        // 克隆失败后，只有自己建的目录被清理
        let _ = job;
        let t = std::time::Instant::now();
        while f.root.join("claimed").exists() && t.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!f.root.join("claimed").exists(), "the half-made directory is gone");
        assert!(f.root.join("mine").join("keep.txt").is_file(), "someone else's directory is untouched");
    }

    #[tokio::test]
    async fn clone_local_via_ssh_url_fails_cleanly_and_can_cancel() {
        // 不依赖网络：指向一个必然连不上的本机端口，任务应以 job.done ok=false 结束且不留半成品目录
        let mut f = fixture().await;
        let ws = f.ws.clone();
        let id = ws.clone_start("ssh://git@127.0.0.1:1/x/y.git", Some("cloned"), None).await.unwrap();
        let done = next_changed(&mut f.rx, methods::JOB_DONE, Duration::from_secs(30)).await.expect("job.done");
        let d: JobDone = serde_json::from_value(done.params).unwrap();
        assert_eq!(d.job_id, id);
        assert!(!d.ok);
        assert!(!f.root.join("cloned").exists());
        assert_eq!(ws.store.get_job(&id).await.unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn trash_retention_cleanup() {
        let f = fixture().await;
        let ws = f.ws.clone();
        // 伪造一个 40 天前的回收站条目和一个刚刚的
        let old = now_ms() - 40 * 86_400_000;
        let old_id = format!("old-{}", fmt_compact(old));
        let new_id = format!("new-{}", fmt_compact(now_ms()));
        std::fs::create_dir_all(f.root.join(TRASH_DIR).join(&old_id)).unwrap();
        std::fs::create_dir_all(f.root.join(TRASH_DIR).join(&new_id)).unwrap();
        assert_eq!(ws.cleanup_trash().await, 1);
        let left: Vec<String> = ws.trash_list().await.into_iter().map(|e| e.id).collect();
        assert_eq!(left, vec![new_id]);
    }

    // ======================================================================= 关联已有目录（ADR-33）

    /// 一台模拟的机器：`HOME` 下有 anycon 数据目录、workspace 根目录和几个普通目录。
    ///
    /// ```text
    /// <base>/home/            HOME（daemon 环境里的 HOME）
    ///   .anycon/              anycon 数据目录（Paths.home，存 daemon.db / config.toml）
    ///   anycon/               workspace 根目录
    ///   work/proj-a/          普通项目目录（有 sub/、f.txt）
    ///   work/proj-b/.git/     另一个，有 .git
    ///   work/.hidden/         隐藏目录
    ///   work/notes.txt        文件
    ///   other/                link_roots 之外
    /// <base>/outside/         HOME 之外：secret.txt、dir/
    /// ```
    struct LinkFx {
        _d: tempfile::TempDir,
        ws: Arc<Workspace>,
        config: Arc<ConfigStore>,
        store: Store,
        paths: Paths,
        rx: mpsc::UnboundedReceiver<RpcNotification>,
        base: PathBuf,
        home: PathBuf,
        root: PathBuf,
        env: Arc<HashMap<String, String>>,
    }

    async fn link_fixture(link_roots: &str) -> LinkFx {
        link_fixture_with(link_roots, |_, _| {}).await
    }

    /// `prepare(home, base)` 在 `Workspace` 创建（= `link_roots` 拍快照）之前运行：需要「一开始就存在」的符号链接 root 时用。
    async fn link_fixture_with(link_roots: &str, prepare: impl FnOnce(&Path, &Path)) -> LinkFx {
        let cfg = format!("[workspace]\nlink_roots = {link_roots}\n");
        link_fixture_cfg(move |_| cfg, prepare).await
    }

    /// [`link_fixture_with`]，`config.toml` 的内容由 `config(base)` 给（要在里面写 `base` 之下的路径时用）。
    async fn link_fixture_cfg(config: impl FnOnce(&Path) -> String, prepare: impl FnOnce(&Path, &Path)) -> LinkFx {
        let d = tempfile::tempdir().unwrap();
        // macOS 的临时目录在符号链接（/var -> /private/var）后面：所有断言用 canonical 路径
        let base = crate::sys::canonicalize(d.path()).unwrap();
        let home = base.join("home");
        let data = home.join(".anycon");
        let root = home.join("anycon");
        for dir in [
            data.clone(),
            home.join("work/proj-a/sub"),
            home.join("work/proj-b/.git"),
            home.join("work/.hidden"),
            home.join("other"),
            base.join("outside/dir"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(home.join("work/proj-a/f.txt"), "hello").unwrap();
        std::fs::write(home.join("work/proj-a/sub/g.txt"), "deep").unwrap();
        std::fs::write(home.join("work/notes.txt"), "n").unwrap();
        std::fs::write(base.join("outside/secret.txt"), "top secret").unwrap();
        prepare(&home, &base);
        let paths = Paths::new(data);
        std::fs::write(paths.config_toml(), config(&base)).unwrap();
        let store = Store::open(&paths.db()).await.unwrap();
        let config = ConfigStore::load(&paths).unwrap();
        let clients = Clients::new();
        let (tx, rx) = mpsc::unbounded_channel();
        clients.register(Caller::local_cli(), Box::new(ChanSink(tx)));
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("HOME".into(), home.to_string_lossy().into_owned());
        let env = Arc::new(env);
        let ws = Workspace::new(&root, config.clone(), store.clone(), clients, env.clone()).await.unwrap();
        let root = ws.root().to_path_buf();
        LinkFx { _d: d, ws, config, store, paths, rx, base, home, root, env }
    }

    /// 没有运行中的会话的 unlink（测试里 Workspace 不知道会话）。
    async fn unlink(ws: &Workspace, name: &str) -> RpcResult<PathBuf> {
        ws.unlink(name, |_| async { false }).await.map(|(_, path)| path)
    }

    fn code_of<T: std::fmt::Debug>(r: RpcResult<T>) -> ErrorCode {
        r.unwrap_err().stable_code().expect("a stable error code")
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<RpcNotification>, method: &str) -> Vec<WorkspaceChanged> {
        let mut out = Vec::new();
        while let Ok(n) = rx.try_recv() {
            if n.method == method {
                out.push(serde_json::from_value(n.params).unwrap());
            }
        }
        out
    }

    #[tokio::test]
    async fn link_expands_tilde_registers_the_directory_and_lists_it() {
        let mut f = link_fixture("[\"~\"]").await;
        let p = f.ws.link("~/work/proj-a", None).await.unwrap();
        assert_eq!((p.name.as_str(), p.kind, p.missing), ("proj-a", ProjectKind::Linked, false));
        assert_eq!(p.path, f.home.join("work/proj-a").to_string_lossy());
        assert_eq!(
            drain(&mut f.rx, methods::WORKSPACE_CHANGED),
            vec![WorkspaceChanged { added: vec!["proj-a".into()], ..Default::default() }]
        );
        // 另起一个项目（带显式名字），根目录里再有一个普通项目
        let b = f.ws.link(&format!("{}/work/proj-b", f.home.display()), Some("bee")).await.unwrap();
        assert_eq!(b.name, "bee");
        f.ws.create("plain", false, None).await.unwrap();
        let list = f.ws.list(&HashMap::from([("proj-a".to_string(), 2)])).await;
        let kinds: HashMap<&str, ProjectKind> = list.projects.iter().map(|p| (p.name.as_str(), p.kind)).collect();
        assert_eq!(
            kinds,
            HashMap::from([
                ("proj-a", ProjectKind::Linked),
                ("bee", ProjectKind::Linked),
                ("plain", ProjectKind::Folder)
            ])
        );
        let a = list.projects.iter().find(|p| p.name == "proj-a").unwrap();
        assert_eq!(a.active_sessions, 2);
        assert!(a.modified_at_ms > 0 && !a.missing);
        // project_path 认关联项目；单个项目信息也一样
        assert_eq!(f.ws.project_path("proj-a").unwrap(), f.home.join("work/proj-a"));
        assert_eq!(f.ws.project("bee", 0).await.unwrap().kind, ProjectKind::Linked);
        // 表里存的是 canonical 路径，重启（新的 Workspace 读同一个库）之后还在
        let rows = f.store.list_linked().await.unwrap();
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["bee", "proj-a"]);
        let again =
            Workspace::new(&f.root, f.config.clone(), f.store.clone(), Clients::new(), f.env.clone()).await.unwrap();
        assert_eq!(again.project_path("bee").unwrap(), f.home.join("work/proj-b"));
        // link 之后 link_roots 收紧：已有的关联不受影响（范围只管新的 link / browse）
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~/other\"]").unwrap();
        f.config.reload().unwrap();
        assert_still_usable_after_the_roots_shrank(&f);
    }

    /// `link_roots` 收紧或清空之后，已有关联照常可用——除非它在内置拒绝表的目录里面（比如 macOS 的临时目录在
    /// `/private/var/folders` 下）：那种位置只有被 `link_roots` 的某一项罩着才放行，规则 5 按当前配置重新判断。
    fn assert_still_usable_after_the_roots_shrank(f: &LinkFx) {
        let in_denied_dir = f.ws.link_ctx_lenient().deny.iter().any(|d| f.home.starts_with(&d.canon));
        let r = f.ws.project_path("proj-a");
        if in_denied_dir {
            assert_eq!(code_of(r), ErrorCode::NotFound, "no root covers it any more");
        } else {
            assert!(r.is_ok(), "{r:?}");
        }
    }

    #[tokio::test]
    async fn link_roots_bound_what_can_be_linked_and_errors_do_not_leak_outside_paths() {
        let f = link_fixture("[\"~/work\"]").await;
        let home = f.home.display().to_string();
        // 在范围之外：存在的、`..` 绕出去的、绝对路径、HOME 之外的
        for bad in [
            format!("{home}/other"),
            "~/other".to_string(),
            "~/work/../other".to_string(),
            "/etc".to_string(),
            format!("{}/outside/dir", f.base.display()),
            // 不存在的也是 forbidden，不泄露范围之外有没有这个目录
            format!("{home}/other/nothing-here"),
            "/definitely/not/there".to_string(),
        ] {
            assert_eq!(code_of(f.ws.link(&bad, None).await), ErrorCode::Forbidden, "{bad}");
        }
        // 范围之内但不存在 / 是文件
        assert_eq!(code_of(f.ws.link("~/work/nothing", None).await), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.link("~/work/notes.txt", None).await), ErrorCode::NotFound);
        // 指向范围之外的符号链接：canonicalize 之后判断
        std::os::unix::fs::symlink(f.base.join("outside/dir"), f.home.join("work/escape")).unwrap();
        assert_eq!(code_of(f.ws.link("~/work/escape", None).await), ErrorCode::Forbidden);
        // 指向范围之内的符号链接：关联的是它指向的真实目录
        std::os::unix::fs::symlink(f.home.join("work/proj-a"), f.home.join("work/alias")).unwrap();
        let p = f.ws.link("~/work/alias", Some("via-alias")).await.unwrap();
        assert_eq!(p.path, f.home.join("work/proj-a").to_string_lossy());
        // 文本非法
        assert_eq!(code_of(f.ws.link("", None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link("~/work/a\0b", None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link("~/work/a\nb", None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link(&format!("~/work/{}", "x".repeat(5000)), None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link("work/proj-b", None).await), ErrorCode::Invalid, "relative paths are refused");
        assert_eq!(code_of(f.ws.link("~someone/work", None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.browse(Some("work"), false).await), ErrorCode::Invalid);
    }

    #[tokio::test]
    async fn an_empty_link_roots_turns_linking_and_browsing_off_and_changes_apply_without_restart() {
        let f = link_fixture("[]").await;
        assert_eq!(code_of(f.ws.link("~/work/proj-a", None).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.browse(None, false).await), ErrorCode::Forbidden);
        // 热加载：改配置文件、reload 之后下一次调用就按新值
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~/work\"]").unwrap();
        assert!(f.config.reload().unwrap());
        f.ws.link("~/work/proj-a", None).await.unwrap();
        assert_eq!(f.ws.browse(None, false).await.unwrap().roots, vec![f.home.join("work").to_string_lossy()]);
        crate::config::set_value(&f.paths, "workspace.link_roots", "[]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(code_of(f.ws.link("~/work/proj-b", None).await), ErrorCode::Forbidden);
        // 配置了但一项都不存在：同样不可用，也不会 panic
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"/definitely/not/a/dir\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(code_of(f.ws.browse(None, false).await), ErrorCode::Forbidden);
        // 已有的关联不受影响
        assert_still_usable_after_the_roots_shrank(&f);
    }

    #[tokio::test]
    async fn the_workspace_root_the_data_dir_and_their_parents_cannot_be_linked() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.create("mine", false, None).await.unwrap();
        // HOME 本身包含数据目录和 workspace 根目录
        assert_eq!(code_of(f.ws.link("~", Some("home")).await), ErrorCode::Forbidden);
        // 数据目录本身 / 里面
        assert_eq!(code_of(f.ws.link("~/.anycon", Some("data")).await), ErrorCode::Forbidden);
        std::fs::create_dir_all(f.paths.home.join("keys")).unwrap();
        assert_eq!(code_of(f.ws.link("~/.anycon/keys", Some("keys")).await), ErrorCode::Forbidden);
        // workspace 根目录本身：它包含自己
        assert_eq!(code_of(f.ws.link("~/anycon", Some("ws")).await), ErrorCode::Forbidden);
        // 在 workspace 根目录里面：已经是项目 / 在 .trash、.scratch 里 -> invalid
        assert_eq!(code_of(f.ws.link("~/anycon/mine", None).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link("~/anycon/.trash", Some("t")).await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.link("~/anycon/.scratch", Some("s")).await), ErrorCode::Invalid);
        // 数据目录通过符号链接进来也一样（canonicalize 之后比较）
        std::os::unix::fs::symlink(&f.paths.home, f.home.join("work/sneaky")).unwrap();
        assert_eq!(code_of(f.ws.link("~/work/sneaky", None).await), ErrorCode::Forbidden);
        // 文件系统根
        let g = link_fixture("[\"/\"]").await;
        assert_eq!(code_of(g.ws.link("/", Some("everything")).await), ErrorCode::Forbidden);
        // 一个普通目录没问题
        f.ws.link("~/work/proj-a", None).await.unwrap();
    }

    #[tokio::test]
    async fn names_share_one_namespace_with_the_root_projects() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.create("taken", false, None).await.unwrap();
        f.ws.link("~/work/proj-a", None).await.unwrap();
        // 同一个 canonical 路径（包括经过符号链接的另一条路径）不能关联两次，消息里带已有的名字
        let e = f.ws.link("~/work/proj-a", Some("again")).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Exists));
        assert!(e.message.contains("proj-a"), "{}", e.message);
        std::os::unix::fs::symlink(f.home.join("work/proj-a"), f.home.join("work/alias")).unwrap();
        assert_eq!(code_of(f.ws.link("~/work/alias", Some("alias")).await), ErrorCode::Exists);
        // 名字被另一个关联项目 / 根目录下的项目占了
        std::fs::create_dir_all(f.home.join("other/proj-a")).unwrap();
        assert_eq!(code_of(f.ws.link("~/other/proj-a", None).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.link("~/work/proj-b", Some("taken")).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.link("~/work/proj-b", Some("proj-a")).await), ErrorCode::Exists);
        // 名字规则同 workspace.create；目录名不合法时要客户端给名字
        for bad in [".x", "a/b", "..", ".trash", ".scratch"] {
            assert_eq!(code_of(f.ws.link("~/work/proj-b", Some(bad)).await), ErrorCode::Invalid, "{bad}");
        }
        // 隐藏目录要运维显式列进 link_roots 才能关联；目录名以 `.` 开头不能当项目名，要客户端给名字
        assert_eq!(code_of(f.ws.link("~/work/.hidden", Some("hidden")).await), ErrorCode::Forbidden);
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/work/.hidden\"]").unwrap();
        f.config.reload().unwrap();
        let e = f.ws.link("~/work/.hidden", None).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
        assert!(e.message.contains("pass a name"), "{}", e.message);
        assert_eq!(f.ws.link("~/work/.hidden", Some("hidden")).await.unwrap().name, "hidden");
        // 反过来：新建 / 克隆 / 改名 / 恢复 / scratch 转项目遇到关联项目的名字都是 exists
        assert_eq!(code_of(f.ws.create("proj-a", false, None).await), ErrorCode::Exists);
        assert_eq!(
            code_of(f.ws.clone_start("https://example.invalid/x/y.git", Some("proj-a"), None).await),
            ErrorCode::Exists
        );
        assert!(!f.root.join("proj-a").exists(), "nothing was created in the root");
        assert_eq!(code_of(f.ws.rename("taken", "proj-a").await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.rename("proj-a", "taken").await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.rename("proj-a", "hidden").await), ErrorCode::Exists);
        let trashed = f.ws.trash("taken").await.unwrap();
        f.ws.link("~/work/proj-b", Some("taken")).await.unwrap(); // 名字腾出来了，关联可以用
        assert_eq!(code_of(f.ws.restore(&trashed.id).await), ErrorCode::Exists);
        let scratch = f.ws.scratch_dir("sess_1").unwrap();
        assert!(scratch.is_dir());
        assert_eq!(code_of(f.ws.promote_scratch("sess_1", "proj-a").await), ErrorCode::Exists);
        assert!(scratch.is_dir(), "the scratch directory stays where it was");
    }

    #[tokio::test]
    async fn a_root_folder_created_later_with_a_linked_name_is_hidden_and_warned_once() {
        let mut f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        drain(&mut f.rx, methods::WORKSPACE_CHANGED);
        // 事后在根目录里手工建出同名文件夹
        std::fs::create_dir(f.root.join("proj-a")).unwrap();
        std::fs::write(f.root.join("proj-a/mine.txt"), "x").unwrap();
        for _ in 0..3 {
            let list = f.ws.list(&HashMap::new()).await;
            let same: Vec<&Project> = list.projects.iter().filter(|p| p.name == "proj-a").collect();
            assert_eq!(same.len(), 1, "{:?}", list.projects);
            assert_eq!(same[0].kind, ProjectKind::Linked, "the linked project wins");
        }
        assert_eq!(f.ws.shadow_warned.lock().unwrap().len(), 1, "warned once per name");
        // 项目名解析到关联目录；fs.* 也是（根目录里被盖住的文件夹看不到）
        assert_eq!(f.ws.project_path("proj-a").unwrap(), f.home.join("work/proj-a"));
        assert_eq!(f.ws.fs_read("proj-a/f.txt", None).await.unwrap().text, "hello");
        assert_eq!(code_of(f.ws.fs_read("proj-a/mine.txt", None).await), ErrorCode::NotFound);
        // 重新扫描不把它当作新增（它不是可见的项目）
        f.ws.rescan_and_notify().await;
        assert!(drain(&mut f.rx, methods::WORKSPACE_CHANGED).is_empty());
        // 解除关联之后，根目录里的文件夹露出来
        unlink(&f.ws, "proj-a").await.unwrap();
        let events = drain(&mut f.rx, methods::WORKSPACE_CHANGED);
        assert!(events.iter().any(|e| e.removed == vec!["proj-a".to_string()]), "{events:?}");
        assert!(events.iter().any(|e| e.added == vec!["proj-a".to_string()]), "{events:?}");
        let list = f.ws.list(&HashMap::new()).await;
        let p = list.projects.iter().find(|p| p.name == "proj-a").unwrap();
        assert_eq!(p.kind, ProjectKind::Folder);
        assert_eq!(f.ws.project_path("proj-a").unwrap(), f.root.join("proj-a"));
    }

    /// 关联项目里的 worktree（`<关联目录>/.anycon/worktrees/<名字>`）也能经 `<项目名>/.anycon/worktrees/<名字>` 浏览。
    #[tokio::test]
    async fn fs_opens_a_worktree_inside_a_linked_project() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        let wt = f.home.join("work/proj-a/.anycon/worktrees/fix");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("x.txt"), "in worktree").unwrap();
        let names = |l: FsListResult| l.entries.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(f.ws.fs_list("proj-a/.anycon/worktrees/fix").unwrap()), vec!["x.txt"]);
        assert_eq!(f.ws.fs_read("proj-a/.anycon/worktrees/fix/x.txt", None).await.unwrap().text, "in worktree");
    }

    #[tokio::test]
    async fn fs_resolves_inside_the_linked_directory_and_cannot_leave_it() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        // 第一段是关联项目名：其余部分相对关联目录
        let names = |l: FsListResult| l.entries.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(f.ws.fs_list("proj-a").unwrap()), vec!["sub", "f.txt"]);
        assert_eq!(names(f.ws.fs_list("proj-a/sub").unwrap()), vec!["g.txt"]);
        assert_eq!(names(f.ws.fs_list("./proj-a/./sub").unwrap()), vec!["g.txt"]);
        assert_eq!(f.ws.fs_read("proj-a/sub/g.txt", None).await.unwrap().text, "deep");
        // 根目录的列表不含关联项目
        assert!(!names(f.ws.fs_list("").unwrap()).contains(&"proj-a".to_string()));
        // 走不出去：`..`、绝对路径、符号链接（指向关联目录之外，不论落在 HOME 里还是外面）
        for bad in ["proj-a/..", "proj-a/../proj-b", "proj-a/sub/../../proj-b/x", "/etc/passwd"] {
            assert_eq!(code_of(f.ws.fs_read(bad, None).await), ErrorCode::Forbidden, "{bad}");
        }
        std::os::unix::fs::symlink(f.base.join("outside"), f.home.join("work/proj-a/out")).unwrap();
        std::os::unix::fs::symlink(f.home.join("work/proj-b"), f.home.join("work/proj-a/sibling")).unwrap();
        std::os::unix::fs::symlink(f.home.join("work/proj-a/sub"), f.home.join("work/proj-a/inner")).unwrap();
        std::os::unix::fs::symlink(f.base.join("outside/secret.txt"), f.home.join("work/proj-a/leak.txt")).unwrap();
        assert_eq!(code_of(f.ws.fs_read("proj-a/out/secret.txt", None).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.fs_read("proj-a/leak.txt", None).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.fs_list("proj-a/sibling")), ErrorCode::Forbidden);
        // 列表里不出现指向边界之外的符号链接，指向里面的还在
        let listed = names(f.ws.fs_list("proj-a").unwrap());
        assert!(listed.contains(&"inner".to_string()), "{listed:?}");
        for hidden in ["out", "sibling", "leak.txt"] {
            assert!(!listed.contains(&hidden.to_string()), "{hidden} leaks out of the project: {listed:?}");
        }
        assert_eq!(f.ws.fs_read("proj-a/inner/g.txt", None).await.unwrap().text, "deep");
        // 不是项目名的第一段仍然相对根目录：HOME 里的其它目录不可达
        assert_eq!(code_of(f.ws.fs_read("work/proj-a/f.txt", None).await), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.fs_list("proj-b")), ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_linked_directory_that_disappears_is_missing_until_unlinked() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-b", Some("bee")).await.unwrap();
        std::fs::remove_dir_all(f.home.join("work/proj-b")).unwrap();
        let list = f.ws.list(&HashMap::new()).await;
        let p = list.projects.iter().find(|p| p.name == "bee").unwrap();
        assert!(p.missing && p.git.is_none() && p.modified_at_ms == 0, "{p:?}");
        assert_eq!(p.kind, ProjectKind::Linked);
        assert_eq!(p.path, f.home.join("work/proj-b").to_string_lossy(), "the path is still shown");
        let one = f.ws.project("bee", 0).await.unwrap();
        assert!(one.missing);
        assert_eq!(code_of(f.ws.project_path("bee")), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.fs_list("bee")), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.fs_read("bee/x", None).await), ErrorCode::NotFound);
        // 路径被换成了文件：同样是 missing
        std::fs::write(f.home.join("work/proj-b"), "now a file").unwrap();
        assert!(f.ws.list(&HashMap::new()).await.projects.iter().find(|p| p.name == "bee").unwrap().missing);
        assert_eq!(code_of(f.ws.project_path("bee")), ErrorCode::NotFound);
        // missing 的项目可以解除关联、可以改名；目录回来后又好了
        std::fs::remove_file(f.home.join("work/proj-b")).unwrap();
        std::fs::create_dir(f.home.join("work/proj-b")).unwrap();
        assert!(f.ws.project_path("bee").is_ok());
        assert!(!f.ws.project("bee", 0).await.unwrap().missing);
        std::fs::remove_dir(f.home.join("work/proj-b")).unwrap();
        f.ws.rename("bee", "bee2").await.unwrap();
        unlink(&f.ws, "bee2").await.unwrap();
        assert!(f.ws.list(&HashMap::new()).await.projects.is_empty());
    }

    #[tokio::test]
    async fn rename_trash_and_unlink_of_a_linked_project_never_touch_the_directory() {
        let mut f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        drain(&mut f.rx, methods::WORKSPACE_CHANGED);
        let dir = f.home.join("work/proj-a");
        // rename：只改名字，目录原地不动，库里的名字跟着变，事件是 renamed
        f.ws.rename("proj-a", "alpha").await.unwrap();
        assert!(dir.join("f.txt").is_file());
        assert_eq!(f.ws.project_path("alpha").unwrap(), dir);
        assert_eq!(code_of(f.ws.project_path("proj-a")), ErrorCode::NotFound);
        assert_eq!(f.store.list_linked().await.unwrap()[0].name, "alpha");
        assert_eq!(
            drain(&mut f.rx, methods::WORKSPACE_CHANGED),
            vec![WorkspaceChanged {
                renamed: vec![RenamedProject { from: "proj-a".into(), to: "alpha".into() }],
                ..Default::default()
            }]
        );
        assert_eq!(code_of(f.ws.rename("alpha", ".bad").await), ErrorCode::Invalid);
        assert_eq!(code_of(f.ws.rename("alpha", "alpha").await), ErrorCode::Exists);
        // trash：invalid，目录和回收站都没动
        let e = f.ws.trash("alpha").await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
        assert!(e.message.contains("unlink"), "{}", e.message);
        assert!(dir.join("f.txt").is_file() && f.ws.trash_list().await.is_empty());
        // unlink：根目录里的项目 / 不存在的名字是 invalid；关联项目成功，目录和文件原样
        f.ws.create("plain", false, None).await.unwrap();
        drain(&mut f.rx, methods::WORKSPACE_CHANGED);
        assert_eq!(code_of(unlink(&f.ws, "plain").await), ErrorCode::Invalid);
        assert_eq!(code_of(unlink(&f.ws, "ghost").await), ErrorCode::Invalid);
        assert!(f.root.join("plain").is_dir());
        assert_eq!(unlink(&f.ws, "alpha").await.unwrap(), dir);
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "hello");
        assert!(f.store.list_linked().await.unwrap().is_empty());
        assert_eq!(
            drain(&mut f.rx, methods::WORKSPACE_CHANGED),
            vec![WorkspaceChanged { removed: vec!["alpha".into()], ..Default::default() }]
        );
        assert_eq!(code_of(f.ws.project_path("alpha")), ErrorCode::NotFound);
        // 解除关联之后可以再次关联（路径和名字都自由了）
        assert_eq!(f.ws.link("~/work/proj-a", None).await.unwrap().name, "proj-a");
    }

    #[tokio::test]
    async fn browse_lists_sub_directories_with_flags_and_stays_inside_the_roots() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.create("mine", false, None).await.unwrap();
        f.ws.link("~/work/proj-a", None).await.unwrap();
        // 缺省 = 第一个 root（HOME）：只列目录，隐藏目录默认不列
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.path, f.home.to_string_lossy());
        assert_eq!(r.roots, vec![f.home.to_string_lossy()]);
        assert_eq!(r.parent, None, "a root has no parent to go to");
        assert!(!r.linkable, "HOME contains the data directory");
        let names: Vec<&str> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["anycon", "other", "work"]);
        let r = f.ws.browse(Some("~"), true).await.unwrap();
        let names: Vec<&str> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".anycon", "anycon", "other", "work"]);
        let by = |r: &WorkspaceBrowseResult, n: &str| r.entries.iter().find(|e| e.name == n).unwrap().clone();
        assert!(!by(&r, ".anycon").linkable && !by(&r, "anycon").linkable);
        assert!(by(&r, "work").linkable && by(&r, "other").linkable);
        // 往下一层
        let r = f.ws.browse(Some("~/work"), true).await.unwrap();
        assert_eq!(r.parent.as_deref(), Some(f.home.to_string_lossy().as_ref()));
        assert!(r.linkable);
        let names: Vec<&str> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".hidden", "proj-a", "proj-b"], "files are not listed");
        assert_eq!(by(&r, "proj-a").project.as_deref(), Some("proj-a"));
        assert!(!by(&r, "proj-a").linkable && by(&r, "proj-a").path == f.home.join("work/proj-a").to_string_lossy());
        assert!(by(&r, "proj-b").git && by(&r, "proj-b").linkable && by(&r, "proj-b").project.is_none());
        assert!(!by(&r, "proj-a").git);
        // path 本身已经是项目
        let r = f.ws.browse(Some("~/work/proj-a"), false).await.unwrap();
        assert_eq!((r.project.as_deref(), r.linkable), (Some("proj-a"), false));
        // workspace 根目录里：根目录本身不能关联，里面的项目标出名字
        let r = f.ws.browse(Some("~/anycon"), false).await.unwrap();
        assert!(!r.linkable && r.project.is_none());
        assert_eq!(by(&r, "mine").project.as_deref(), Some("mine"));
        assert!(!by(&r, "mine").linkable);
        // 范围之外 / 不是目录 / 不存在
        assert_eq!(code_of(f.ws.browse(Some("/"), false).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.browse(Some("~/.."), false).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.browse(Some("~/work/notes.txt"), false).await), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.browse(Some("~/work/ghost"), false).await), ErrorCode::NotFound);
        assert_eq!(code_of(f.ws.browse(Some("/nonexistent"), false).await), ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn browse_follows_symlinks_only_to_places_inside_the_roots() {
        let f = link_fixture("[\"~/work\"]").await;
        std::os::unix::fs::symlink(f.home.join("work/proj-a"), f.home.join("work/alias")).unwrap();
        std::os::unix::fs::symlink(f.base.join("outside/dir"), f.home.join("work/escape")).unwrap();
        std::os::unix::fs::symlink(f.home.join("work/notes.txt"), f.home.join("work/file-link")).unwrap();
        std::os::unix::fs::symlink(f.home.join("work/gone"), f.home.join("work/broken")).unwrap();
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.path, f.home.join("work").to_string_lossy());
        let names: Vec<&str> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alias", "proj-a", "proj-b"], "escape / file / broken links are not listed");
        // 通过符号链接进入的条目给的是解析后的路径，可以直接传给 browse / link
        let alias = r.entries.iter().find(|e| e.name == "alias").unwrap();
        assert_eq!(alias.path, f.home.join("work/proj-a").to_string_lossy());
        assert_eq!(code_of(f.ws.browse(Some("~/work/escape"), false).await), ErrorCode::Forbidden);
        // 无法读的目录不列（root 用户能读一切，跳过）
        // SAFETY: geteuid 没有前置条件。
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = f.home.join("work/locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let names: Vec<String> =
                f.ws.browse(None, false).await.unwrap().entries.into_iter().map(|e| e.name).collect();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!names.contains(&"locked".to_string()), "{names:?}");
        }
    }

    #[tokio::test]
    async fn browse_truncates_at_a_thousand_entries_and_sorts_by_name() {
        let f = link_fixture("[\"~/work\"]").await;
        let big = f.home.join("work/big");
        std::fs::create_dir(&big).unwrap();
        for i in 0..(BROWSE_MAX_ENTRIES + 5) {
            std::fs::create_dir(big.join(format!("d{i:04}"))).unwrap();
        }
        let r = f.ws.browse(Some("~/work/big"), false).await.unwrap();
        assert_eq!(r.entries.len(), BROWSE_MAX_ENTRIES);
        assert!(r.truncated);
        assert_eq!(r.entries[0].name, "d0000");
        assert_eq!(r.entries[BROWSE_MAX_ENTRIES - 1].name, format!("d{:04}", BROWSE_MAX_ENTRIES - 1));
        // 刚好 1000 个不算截断
        std::fs::remove_dir(big.join(format!("d{:04}", BROWSE_MAX_ENTRIES + 4))).unwrap();
        for i in BROWSE_MAX_ENTRIES..(BROWSE_MAX_ENTRIES + 4) {
            std::fs::remove_dir(big.join(format!("d{i:04}"))).unwrap();
        }
        let r = f.ws.browse(Some("~/work/big"), false).await.unwrap();
        assert_eq!((r.entries.len(), r.truncated), (BROWSE_MAX_ENTRIES, false));
        // 大小写不敏感地按名字排序
        for n in ["Zed", "alpha", "Beta"] {
            std::fs::create_dir(f.home.join("work").join(n)).unwrap();
        }
        let r = f.ws.browse(None, false).await.unwrap();
        let names: Vec<&str> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Beta", "big", "proj-a", "proj-b", "Zed"]);
    }

    #[test]
    fn home_expansion_and_path_text_rules() {
        let env = HashMap::from([("HOME".to_string(), "/home/me".to_string())]);
        assert_eq!(expand_home("~", &env).unwrap(), PathBuf::from("/home/me"));
        assert_eq!(expand_home("~/work/x", &env).unwrap(), PathBuf::from("/home/me/work/x"));
        assert_eq!(expand_home("/abs/x", &env).unwrap(), PathBuf::from("/abs/x"));
        for bad in ["rel/x", "~me/x", "", "./x"] {
            assert!(expand_home(bad, &env).is_err(), "{bad:?}");
        }
        assert!(expand_home("~", &HashMap::new()).is_err(), "no HOME");
        assert!(expand_home("~", &HashMap::from([("HOME".to_string(), "relative".to_string())])).is_err());
        assert_eq!(lexical_clean(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(lexical_clean(Path::new("/a/../../x")), PathBuf::from("/x"));
        assert!(check_path_text("/ok").is_ok());
        for bad in ["", "a\0b", "a\tb"] {
            assert!(check_path_text(bad).is_err(), "{bad:?}");
        }
        assert!(check_path_text(&"x".repeat(4097)).is_err() && check_path_text(&"x".repeat(4096)).is_ok());
    }

    /// 给 web 选择器的契约：路径都是 canonical 绝对路径、没有结尾分隔符；root 没有 parent；
    /// 不存在的 root 被跳过，一个都不剩就是 forbidden（不是 not_found，也不是空列表）。
    #[tokio::test]
    async fn browse_contract_canonical_paths_no_parent_at_a_root_and_forbidden_without_usable_roots() {
        // 带结尾斜杠的、不存在的、是文件的、经符号链接的 root：展开 + canonicalize + 去重 + 跳过不可用的
        let f = link_fixture_with(
            "[\"~/nope\", \"~/work/\", \"~/work/notes.txt\", \"~/alias-of-other\", \"~/other\", \"~/work/proj-a\"]",
            |home, _| std::os::unix::fs::symlink(home.join("other"), home.join("alias-of-other")).unwrap(),
        )
        .await;
        let work = f.home.join("work").to_string_lossy().into_owned();
        let other = f.home.join("other").to_string_lossy().into_owned();
        let proj_a = f.home.join("work/proj-a").to_string_lossy().into_owned();
        let check_canonical = |p: &str| {
            assert!(p.starts_with('/'), "{p} is not absolute");
            assert!(p == "/" || !p.ends_with('/'), "{p} has a trailing separator");
            assert!(!p.contains("/./") && !p.contains("/../") && !p.contains("//"), "{p} is not clean");
            assert_eq!(crate::sys::canonicalize(p).unwrap().to_string_lossy(), p, "{p} is not canonical");
        };
        // 缺省 path = 第一个可用的 root（跳过不存在的、文件、经过符号链接的）；roots 里每项 canonical，重复合并
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.path, work);
        assert_eq!(r.roots, vec![work.clone(), other.clone(), proj_a.clone()]);
        assert_eq!(r.parent, None, "the default path is a root");
        check_canonical(&r.path);
        r.roots.iter().for_each(|p| check_canonical(p));
        r.entries.iter().for_each(|e| check_canonical(&e.path));
        assert!(!r.entries.is_empty());
        // 调用方传带结尾斜杠 / 绕路 / 符号链接的路径：返回的 path 仍然是 canonical
        for given in ["~/work/", "~/work/./", &format!("{}/work/../work", f.home.display())] {
            let r = f.ws.browse(Some(given), false).await.unwrap();
            check_canonical(&r.path);
            r.entries.iter().for_each(|e| check_canonical(&e.path));
            assert_eq!(r.parent, None, "{given}: it is a root (through a trailing slash or `..`)");
        }
        // 经过符号链接的 root（`~/alias-of-other` -> `~/other`）整项忽略并说明原因，不是被「合并」进别的 root
        let ignored = f.ws.ignored_link_roots();
        assert_eq!(ignored.len(), 1, "{ignored:?}");
        assert_eq!(ignored[0].0, "~/alias-of-other");
        assert!(ignored[0].1.contains("symbolic link") && ignored[0].1.contains("real path"), "{ignored:?}");
        // 嵌套的 root（~/work 里面的 ~/work/proj-a）：它自己仍然是 root，没有 parent；再往下才有
        let r = f.ws.browse(Some("~/work/proj-a"), false).await.unwrap();
        assert_eq!(r.parent, None);
        let r = f.ws.browse(Some("~/work/proj-a/sub"), false).await.unwrap();
        assert_eq!(r.parent.as_deref(), Some(proj_a.as_str()));
        check_canonical(r.parent.as_deref().unwrap());
        // 不是 root 的目录有 parent，且 parent 也是 canonical
        let g = link_fixture("[\"~\"]").await;
        let r = g.ws.browse(Some("~/work"), false).await.unwrap();
        assert_eq!(r.parent.as_deref(), Some(g.home.to_string_lossy().as_ref()));
        check_canonical(r.parent.as_deref().unwrap());
        assert_eq!(g.ws.browse(None, false).await.unwrap().parent, None);
        // 没有 path 且没有可用的 root：forbidden（不是 not_found，也不是空列表）
        for roots in ["[]", "[\"~/nope\"]", "[\"/definitely/not/a/dir\", \"~/nope2\"]", "[\"~/work/notes.txt\"]"] {
            let h = link_fixture(roots).await;
            assert_eq!(code_of(h.ws.browse(None, false).await), ErrorCode::Forbidden, "{roots}");
            assert_eq!(code_of(h.ws.browse(Some("~/work"), false).await), ErrorCode::Forbidden, "{roots}");
        }
    }

    // ======================================================================= 拒绝表、符号链接替换、git 元数据、项目名与会话记下的位置

    /// 关联之后把目录换成指向数据目录 / 根外的符号链接，边界不能跟着链接走。
    /// 没有「canonicalize 之后必须等于登记的路径」这一步时，下面每个调用都会成功，`keys/node.key` 能读出来。
    #[tokio::test]
    async fn a_linked_directory_replaced_by_a_symlink_is_missing_everywhere() {
        let f = link_fixture("[\"~\"]").await;
        let dir = f.home.join("work/proj-a");
        f.ws.link("~/work/proj-a", None).await.unwrap();
        std::fs::create_dir_all(f.paths.home.join("keys")).unwrap();
        std::fs::write(f.paths.home.join("keys/node.key"), "SECRET KEY").unwrap();
        // 先确认关联本身是好的
        assert_eq!(f.ws.fs_read("proj-a/f.txt", None).await.unwrap().text, "hello");
        for target in [f.paths.home.clone(), f.base.join("outside"), f.root.clone(), f.home.join("work/proj-b")] {
            std::fs::remove_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(&target, &dir).unwrap();
            let t = target.display();
            assert_eq!(code_of(f.ws.fs_list("proj-a")), ErrorCode::NotFound, "fs.list through a link to {t}");
            assert_eq!(code_of(f.ws.fs_list("proj-a/keys")), ErrorCode::NotFound, "{t}");
            assert_eq!(code_of(f.ws.fs_read("proj-a/keys/node.key", None).await), ErrorCode::NotFound, "{t}");
            assert_eq!(code_of(f.ws.fs_read("proj-a/secret.txt", None).await), ErrorCode::NotFound, "{t}");
            assert_eq!(code_of(f.ws.project_path("proj-a")), ErrorCode::NotFound, "project_path {t}");
            assert_eq!(code_of(f.ws.project_loc("proj-a")), ErrorCode::NotFound, "{t}");
            let list = f.ws.list(&HashMap::new()).await;
            let p = list.projects.iter().find(|p| p.name == "proj-a").unwrap();
            assert!(p.missing && p.git.is_none() && p.modified_at_ms == 0, "workspace.list marks it missing: {t}");
            assert!(f.ws.project("proj-a", 0).await.unwrap().missing);
            // 说明里不带链接指向的新位置
            let m = f.ws.project_path("proj-a").unwrap_err().message;
            assert!(m.contains("no longer resolves") && !m.contains(&*t.to_string()), "{m}");
        }
        // 上级目录被换成符号链接（登记的路径经过它）同样不行
        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(f.ws.project_path("proj-a").is_ok(), "the real directory is back");
        let moved = f.home.join("work-moved");
        std::fs::rename(f.home.join("work"), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, f.home.join("work")).unwrap();
        // 登记的路径 `~/work/proj-a` 的 canonical 现在是 `~/work-moved/proj-a`
        assert_eq!(code_of(f.ws.project_path("proj-a")), ErrorCode::NotFound);
        // 取消关联、再关联：用新的 canonical 路径
        unlink(&f.ws, "proj-a").await.unwrap();
        let p = f.ws.link("~/work/proj-a", None).await.unwrap();
        assert_eq!(p.path, moved.join("proj-a").to_string_lossy());
        assert!(f.ws.project_path("proj-a").is_ok());
    }

    /// 规则 5 也按当前配置重跑规则 2–4：关联之后目录变得「不允许」（比如它成了数据目录的一部分）就当 missing。
    #[tokio::test]
    async fn existing_links_are_rechecked_against_the_current_rules() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        // 把一条旧的登记直接改成敏感位置（模拟旧版本留下的、按现在的规则不该存在的关联）
        std::fs::create_dir_all(f.home.join(".ssh")).unwrap();
        std::fs::create_dir_all(f.home.join("work/.cache/x")).unwrap();
        for (name, path) in [("old-ssh", f.home.join(".ssh")), ("old-hidden", f.home.join("work/.cache/x"))] {
            f.store
                .insert_linked(&LinkedRow { name: name.into(), path: path.to_string_lossy().into(), created_ms: 1 })
                .await
                .unwrap();
        }
        let again =
            Workspace::new(&f.root, f.config.clone(), f.store.clone(), Clients::new(), f.env.clone()).await.unwrap();
        let list = again.list(&HashMap::new()).await;
        let missing = |n: &str| list.projects.iter().find(|p| p.name == n).unwrap().missing;
        assert!(!missing("proj-a") && missing("old-ssh") && missing("old-hidden"), "{:?}", list.projects);
        assert_eq!(code_of(again.project_path("old-ssh")), ErrorCode::NotFound);
        assert!(again.project_path("old-ssh").unwrap_err().message.contains("not allowed"));
        assert_eq!(code_of(again.fs_list("old-hidden")), ErrorCode::NotFound);
        // 显式放行隐藏目录之后又可以用了
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/work/.cache\"]").unwrap();
        f.config.reload().unwrap();
        assert!(again.project_path("old-hidden").is_ok());
        assert_eq!(code_of(again.project_path("old-ssh")), ErrorCode::NotFound, "~/.ssh stays denied");
    }

    /// 默认配置（`link_roots = ["~"]`）下，凭证 / 自启动目录和隐藏目录不能关联，浏览里 `linkable = false` 并说明原因。
    #[tokio::test]
    async fn credential_autostart_and_hidden_directories_cannot_be_linked_by_default() {
        let f = link_fixture("[\"~\"]").await;
        for d in
            [".ssh", "Library/LaunchAgents", ".config/systemd/user", ".config/nvim", ".aws", ".codex", ".local/share"]
        {
            std::fs::create_dir_all(f.home.join(d)).unwrap();
        }
        std::fs::create_dir_all(f.home.join("work/.cache/x")).unwrap();
        for (path, why) in [
            ("~/.ssh", "protected"),
            ("~/Library/LaunchAgents", "protected"),
            ("~/.config/systemd/user", "protected"),
            ("~/.config/nvim", "protected"),
            ("~/.aws", "protected"),
            ("~/.codex", "protected"),
            ("~/.local/share", "protected"),
            ("~/work/.cache", "hidden"),
            ("~/work/.cache/x", "hidden"),
            ("~/work/.hidden", "hidden"),
        ] {
            let e = f.ws.link(path, Some("x")).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{path}");
            assert!(e.message.contains(why) && e.message.contains("link_roots"), "{path}: {}", e.message);
        }
        // 浏览：看得到，但不能关联，原因在 `blocked`
        let r = f.ws.browse(Some("~"), true).await.unwrap();
        let by = |r: &WorkspaceBrowseResult, n: &str| r.entries.iter().find(|e| e.name == n).unwrap().clone();
        for (name, block) in [
            (".ssh", LinkBlock::Sensitive),
            ("Library", LinkBlock::Sensitive),
            (".config", LinkBlock::Sensitive),
            (".anycon", LinkBlock::DataDir),
            ("anycon", LinkBlock::Workspace),
        ] {
            let e = by(&r, name);
            assert_eq!((e.linkable, e.blocked), (false, Some(block)), "{name}");
        }
        let work = by(&r, "work");
        assert_eq!((work.linkable, work.blocked), (true, None));
        assert_eq!((r.linkable, r.blocked), (false, Some(LinkBlock::Workspace)), "~ contains the workspace root");
        let r = f.ws.browse(Some("~/work"), true).await.unwrap();
        assert_eq!(by(&r, ".cache").blocked, Some(LinkBlock::Hidden));
        assert_eq!(by(&r, ".hidden").blocked, Some(LinkBlock::Hidden));
        assert_eq!((by(&r, "proj-a").linkable, by(&r, "proj-a").blocked), (true, None));
        // 已经是项目的没有 blocked，看 project
        f.ws.link("~/work/proj-a", None).await.unwrap();
        let r = f.ws.browse(Some("~/work"), true).await.unwrap();
        let a = by(&r, "proj-a");
        assert_eq!((a.linkable, a.project.as_deref(), a.blocked), (false, Some("proj-a"), None));
        // 拒绝表里的目录（和数据目录）不能浏览进去；隐藏目录可以（里面的条目各自判断）
        std::fs::create_dir_all(f.paths.home.join("keys")).unwrap();
        for p in ["~/.ssh", "~/Library", "~/Library/LaunchAgents", "~/.config/nvim", "~/.anycon", "~/.anycon/keys"] {
            let e = f.ws.browse(Some(p), true).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{p}");
        }
        let r = f.ws.browse(Some("~/work/.cache"), false).await.unwrap();
        assert_eq!((r.blocked, r.entries.len()), (Some(LinkBlock::Hidden), 1));
        assert_eq!(r.entries[0].blocked, Some(LinkBlock::Hidden), "x is below a hidden segment");
        // 「~」本身不能关联
        assert_eq!(code_of(f.ws.link("~", Some("home")).await), ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn explicit_link_roots_allow_what_is_below_them_but_deeper_denials_still_apply() {
        // `link_roots = ["~", "~/.config/nvim"]`：~/.config/nvim 可关联，~/.config 仍不可
        let f = link_fixture("[\"~\", \"~/.config/nvim\"]").await;
        for d in [".config/nvim/lua", ".config/other", ".config/systemd/user", ".config/autostart"] {
            std::fs::create_dir_all(f.home.join(d)).unwrap();
        }
        assert_eq!(f.ws.link("~/.config/nvim", None).await.unwrap().name, "nvim");
        assert_eq!(f.ws.link("~/.config/nvim/lua", None).await.unwrap().name, "lua");
        assert_eq!(code_of(f.ws.link("~/.config", Some("cfg")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/.config/other", Some("o")).await), ErrorCode::Forbidden);
        // 浏览：~/.config 本身不能进（拒绝表），但 ~/.config/nvim 作为 root 可以
        assert_eq!(code_of(f.ws.browse(Some("~/.config"), true).await), ErrorCode::Forbidden);
        assert!(f.ws.browse(Some("~/.config/nvim"), true).await.is_ok());
        // `link_roots = ["~/.config"]`：之下可以，更深的拒绝项（systemd、autostart）仍然生效，~/.config 自己不行
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~/.config\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link("~/.config/other", Some("other")).await.unwrap().name, "other");
        assert_eq!(code_of(f.ws.link("~/.config/systemd", Some("sd")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/.config/systemd/user", Some("sdu")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/.config/autostart", Some("as")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/.config", Some("cfg2")).await), ErrorCode::Forbidden);
        let r = f.ws.browse(None, false).await.unwrap();
        let by = |n: &str| r.entries.iter().find(|e| e.name == n).unwrap().clone();
        assert_eq!(by("systemd").blocked, Some(LinkBlock::Sensitive));
        assert_eq!(by("autostart").blocked, Some(LinkBlock::Sensitive));
        assert!(by("nvim").project.is_some(), "nvim was linked above");
        assert_eq!(r.blocked, Some(LinkBlock::Sensitive), "~/.config contains denied directories");
        // 隐藏目录：显式列出的那一项之下的隐藏段不算，更深的隐藏段仍然算
        std::fs::create_dir_all(f.home.join("work/.cache/.deeper/z")).unwrap();
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/work/.cache\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link("~/work/.cache", Some("cache")).await.unwrap().name, "cache");
        assert_eq!(
            f.ws.link("~/work/.cache/.deeper", Some("d")).await.unwrap_err().stable_code(),
            Some(ErrorCode::Forbidden)
        );
    }

    #[tokio::test]
    async fn the_system_directories_are_denied_even_for_a_slash_root_but_an_explicit_root_inside_them_works() {
        let f = link_fixture("[\"/\"]").await;
        for p in ["/etc", "/usr", "/usr/bin", "/System", "/private", "/var", "/opt"] {
            if std::fs::metadata(p).is_err() {
                continue; // 这台机器上没有这个目录（Linux 上没有 /System）
            }
            let e = f.ws.link(p, Some("sys")).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{p}");
        }
        assert_eq!(code_of(f.ws.link("/", Some("all")).await), ErrorCode::Forbidden);
        // 浏览 `/`：/usr 之类标 sensitive，进不去
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.path, "/");
        assert_eq!(r.blocked, Some(LinkBlock::FilesystemRoot));
        if let Some(usr) = r.entries.iter().find(|e| e.name == "usr") {
            assert_eq!((usr.linkable, usr.blocked), (false, Some(LinkBlock::Sensitive)));
            assert_eq!(code_of(f.ws.browse(Some("/usr"), false).await), ErrorCode::Forbidden);
        }
        // 系统临时目录下显式给的 root 照常可用（macOS 的 /private/var/folders/…，e2e 就是这样用的）
        let external = f.base.join("external");
        std::fs::create_dir_all(external.join("proj")).unwrap();
        let roots = serde_json::to_string(&[external.to_string_lossy()]).unwrap();
        crate::config::set_value(&f.paths, "workspace.link_roots", &roots).unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link(&external.join("proj").to_string_lossy(), None).await.unwrap().name, "proj");
        assert!(f.ws.project_path("proj").is_ok());
        assert!(f.ws.browse(None, false).await.is_ok());
    }

    /// 列出 `~` 时不探测 `~/Documents` 之类里面有没有 `.git`（stat 里面的条目会触发 macOS 的隐私授权弹窗）；
    /// 别处照常探测。观察得到的只有结果：这些目录里明明有 `.git`，`git` 标记却是 false。
    #[tokio::test]
    async fn listing_home_does_not_probe_git_inside_the_privacy_protected_directories() {
        let f = link_fixture("[\"~\"]").await;
        for d in ["Desktop", "Documents", "Downloads", "Pictures", "Movies", "Music", "Public", "code"] {
            std::fs::create_dir_all(f.home.join(d).join(".git")).unwrap();
        }
        let r = f.ws.browse(Some("~"), false).await.unwrap();
        let git = |n: &str| r.entries.iter().find(|e| e.name == n).unwrap().git;
        for d in ["Desktop", "Documents", "Downloads", "Pictures", "Movies", "Music", "Public"] {
            assert!(!git(d), "{d} is not probed");
        }
        assert!(git("code"), "other directories are");
        // 用户自己进到 ~/Documents 里面之后，里面的条目照常探测
        std::fs::create_dir_all(f.home.join("Documents/repo/.git")).unwrap();
        let r = f.ws.browse(Some("~/Documents"), false).await.unwrap();
        assert!(r.entries.iter().find(|e| e.name == "repo").unwrap().git);
        // /Volumes 下的卷同理（纯函数判断，机器上不一定有 /Volumes）
        assert!(f.ws.link_ctx_lenient().skips_git_probe(Path::new("/Volumes/USB")));
        assert!(!f.ws.link_ctx_lenient().skips_git_probe(Path::new("/Volumes/USB/proj")));
        assert!(!f.ws.link_ctx_lenient().skips_git_probe(&f.home.join("code")));
    }

    #[tokio::test]
    async fn a_directory_the_daemon_may_not_read_gives_forbidden_with_the_privacy_hint() {
        // SAFETY: geteuid 没有前置条件。
        if unsafe { libc::geteuid() } == 0 {
            return; // root 什么都读得了
        }
        use std::os::unix::fs::PermissionsExt;
        let f = link_fixture("[\"~\"]").await;
        let locked = f.home.join("work/locked");
        std::fs::create_dir_all(locked.join("inside")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = f.ws.browse(Some("~/work/locked"), false).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = r.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
        assert!(e.message.contains("Privacy & Security"), "{}", e.message);
        // 进不去的子目录还是 forbidden（不是 not_found）
        std::fs::set_permissions(f.home.join("work"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = f.ws.browse(Some("~/work/proj-a"), false).await;
        std::fs::set_permissions(f.home.join("work"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = r.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{e:?}");
    }

    /// daemon 自己跑的 `git` 不执行仓库自己配置的 fsmonitor / hook。
    #[test]
    fn every_git_invocation_disables_fsmonitor_and_hooks() {
        assert_eq!(
            git_invocation(&["status", "--porcelain"]),
            vec!["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "status", "--porcelain"]
        );
        assert_eq!(&GIT_SAFE_CONFIG[..], &["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"]);
    }

    #[tokio::test]
    async fn a_repository_configured_fsmonitor_hook_is_not_executed_by_workspace_list() {
        let f = link_fixture("[\"~\"]").await;
        let repo = f.home.join("work/evil");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        git(&["init", "-q"]);
        let marker = f.base.join("fsmonitor-ran");
        let hook = f.base.join("hook.sh");
        std::fs::write(&hook, format!("#!/bin/sh\necho ran >> {}\nprintf '\\0'\n", marker.display())).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
        // 没有安全配置时，一个普通的 `git status` 会跑这个 hook（证明这个夹具是有意义的）；很老的 git 不跑就跳过断言
        let plain =
            std::process::Command::new("git").args(["status", "--porcelain"]).current_dir(&repo).output().unwrap();
        assert!(plain.status.success());
        let hook_runs_for_plain_git = marker.exists();
        std::fs::remove_file(&marker).ok();
        // daemon 的路径：workspace.list 对关联项目取 git 元数据
        f.ws.link("~/work/evil", None).await.unwrap();
        let mut meta = None;
        for _ in 0..40 {
            let list = f.ws.list(&HashMap::new()).await;
            if let Some(g) = list.projects.iter().find(|p| p.name == "evil").and_then(|p| p.git.clone()) {
                meta = Some(g);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(meta.is_some(), "git metadata is still collected");
        assert!(
            !marker.exists(),
            "the repository's own fsmonitor command must not run (plain git would: {hook_runs_for_plain_git})"
        );
    }

    /// 项目名统一 NFC；不区分大小写的卷上 `proj` 和 `PROJ` 是同一个名字。
    #[tokio::test]
    async fn project_names_are_nfc_and_compared_case_insensitively_on_insensitive_volumes() {
        let mut f = link_fixture("[\"~\"]").await;
        let nfc_cafe = "Caf\u{e9}";
        let nfd_cafe = "Cafe\u{301}";
        // 新建时统一 NFC；另一种写法算同一个名字
        f.ws.create(nfd_cafe, false, None).await.unwrap();
        let list = f.ws.list(&HashMap::new()).await;
        assert_eq!(list.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec![nfc_cafe]);
        assert_eq!(code_of(f.ws.create(nfc_cafe, false, None).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.create(nfd_cafe, false, None).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.link("~/work/proj-a", Some(nfd_cafe)).await), ErrorCode::Exists);
        // 关联的名字也是 NFC；用 NFD 写法查得到，新建 NFC / NFD 都是 exists
        std::fs::create_dir_all(f.home.join("work/Résumé")).unwrap();
        let p = f.ws.link("~/work/Résumé", Some("Re\u{301}sume\u{301}")).await.unwrap();
        assert_eq!(p.name, "R\u{e9}sum\u{e9}");
        assert!(f.ws.is_linked("Re\u{301}sume\u{301}") && f.ws.is_linked("R\u{e9}sum\u{e9}"));
        assert_eq!(f.ws.project_path("Re\u{301}sume\u{301}").unwrap(), f.home.join("work/Résumé"));
        assert_eq!(code_of(f.ws.create("Re\u{301}sume\u{301}", false, None).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.rename(nfc_cafe, "Re\u{301}sume\u{301}").await), ErrorCode::Exists);
        // 项目名比较也用同一套规则（会话、busy 检查）
        assert!(f.ws.same_project_name(nfc_cafe, nfd_cafe));
        assert_eq!(f.ws.canonical_project_name(nfd_cafe), nfc_cafe);
        assert_eq!(f.ws.canonical_project_name("Re\u{301}sume\u{301}"), "R\u{e9}sum\u{e9}");

        // 大小写：这台机器的真实行为
        f.ws.link("~/work/proj-b", Some("proj")).await.unwrap();
        let on_ci_volume = f.ws.ci();
        let created = f.ws.create("PROJ", false, None).await;
        if on_ci_volume {
            assert_eq!(code_of(created), ErrorCode::Exists, "the workspace volume is case-insensitive");
        } else {
            created.unwrap();
        }
        // 不区分大小写的规则本身（和卷无关）：强制打开
        f.ws.set_case_insensitive_for_test(true);
        assert_eq!(code_of(f.ws.create("Proj", false, None).await), ErrorCode::Exists);
        assert_eq!(code_of(f.ws.link("~/work/proj-a", Some("PROJ")).await), ErrorCode::Exists);
        assert_eq!(
            code_of(f.ws.link("~/work/proj-a", Some("CAFE\u{301}")).await),
            ErrorCode::Exists,
            "NFD + upper case"
        );
        assert!(f.ws.same_project_name("proj", "PROJ"));
        assert_eq!(f.ws.project_path("PROJ").unwrap(), f.home.join("work/proj-b"), "fs.* and sessions find it too");
        assert_eq!(f.ws.fs_list("PROJ/.git").unwrap().entries.len(), 0);
        // 反过来：根目录里已有的文件夹挡住大小写不同的关联名
        f.ws.create("Folder", false, None).await.unwrap();
        assert_eq!(code_of(f.ws.link("~/work/proj-a", Some("FOLDER")).await), ErrorCode::Exists);
        // 被关联项目盖住的判断同样按键比较：根目录里的 `PROJ`（大小写不同）被 `proj` 盖住
        if !on_ci_volume {
            let list = f.ws.list(&HashMap::new()).await;
            assert!(list.projects.iter().all(|p| p.name != "PROJ"), "{:?}", list.projects);
        }
        drain(&mut f.rx, methods::WORKSPACE_CHANGED);
    }

    #[test]
    fn the_case_probe_finds_out_whether_the_volume_is_case_insensitive_without_writing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(TRASH_DIR)).unwrap();
        let before: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        let expect = std::fs::metadata(d.path().join(".TRASH")).is_ok();
        assert_eq!(probe_case_insensitive(d.path()), expect);
        let after: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(before, after, "nothing was written");
        assert!(!probe_case_insensitive(&d.path().join("missing")));
    }

    /// 每个会话记着创建时项目指向哪里。
    #[tokio::test]
    async fn project_locations_identify_the_directory_a_session_was_created_in() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.create("folder", false, None).await.unwrap();
        f.ws.link("~/work/proj-a", None).await.unwrap();
        let linked = f.ws.project_loc("proj-a").unwrap();
        let folder = f.ws.project_loc("folder").unwrap();
        assert_eq!(linked, format!("linked:{}", f.home.join("work/proj-a").display()));
        let md = std::fs::metadata(f.root.join("folder")).unwrap();
        assert_eq!(folder, format!("folder:folder#{}:{}", md.dev(), md.ino()));
        // 同一个位置 / 关联项目只认路径
        assert_eq!(f.ws.check_loc(&linked, &linked), LocCheck::Same);
        assert_eq!(f.ws.check_loc(&folder, &folder), LocCheck::Same);
        assert_eq!(f.ws.check_loc(&linked, &folder), LocCheck::Different);
        assert_eq!(f.ws.check_loc(&folder, &linked), LocCheck::Different);
        assert_eq!(
            f.ws.check_loc(&linked, &format!("linked:{}", f.home.join("work/proj-b").display())),
            LocCheck::Different
        );
        // 根目录项目：名字不同 = 不同；同设备 inode 不同 = 换了目录；设备不同 = 搬盘，接受并重新记；旧版本只有名字 = 重新记
        let now = format!("folder:folder#{}:{}", md.dev(), md.ino());
        assert_eq!(f.ws.check_loc(&format!("folder:other#{}:{}", md.dev(), md.ino()), &now), LocCheck::Different);
        assert_eq!(f.ws.check_loc(&format!("folder:folder#{}:{}", md.dev(), md.ino() + 1), &now), LocCheck::Different);
        assert_eq!(
            f.ws.check_loc(&format!("folder:folder#{}:{}", md.dev() + 1, md.ino() + 7), &now),
            LocCheck::Refresh(now.clone())
        );
        assert_eq!(f.ws.check_loc("folder:folder", &now), LocCheck::Refresh(now.clone()));
        assert_eq!(
            f.ws.check_loc("folder:FOLDER", &now),
            if f.ws.ci() { LocCheck::Refresh(now.clone()) } else { LocCheck::Different }
        );
        // 名字里带 `#` 的项目
        assert_eq!(parse_folder_loc("folder:a#b#12:34"), Some(("a#b", Some((12, 34)))));
        assert_eq!(parse_folder_loc("folder:a#b"), Some(("a#b", None)));
        assert_eq!(parse_folder_loc("linked:/x"), None);
        assert_eq!(Workspace::describe_loc(&linked), f.home.join("work/proj-a").to_string_lossy());
        assert!(Workspace::describe_loc(&folder).contains("folder"));
        assert_eq!(code_of(f.ws.project_loc("ghost")), ErrorCode::NotFound);
        // 进回收站再恢复（inode 不变）还是同一个位置；换成同名的新文件夹就是另一个
        let trashed = f.ws.trash("folder").await.unwrap();
        f.ws.restore(&trashed.id).await.unwrap();
        assert_eq!(f.ws.check_loc(&folder, &f.ws.project_loc("folder").unwrap()), LocCheck::Same);
        let trashed = f.ws.trash("folder").await.unwrap();
        f.ws.create("folder", false, None).await.unwrap();
        assert_eq!(f.ws.check_loc(&folder, &f.ws.project_loc("folder").unwrap()), LocCheck::Different);
        let _ = trashed;
    }

    /// 「有没有运行中的会话」的检查和删除登记在同一把锁里：检查期间别的占名字的操作（link / create…）插不进来。
    #[tokio::test]
    async fn unlink_checks_for_running_sessions_while_holding_the_namespace_lock() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        // busy -> 什么都没删
        let e = f.ws.unlink("proj-a", |name| async move { name == "proj-a" }).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Busy));
        assert!(f.ws.is_linked("proj-a") && !f.store.list_linked().await.unwrap().is_empty());
        // 不是关联项目：invalid，而且 busy 检查根本不会被调用
        let called = std::sync::atomic::AtomicBool::new(false);
        let e =
            f.ws.unlink("nope", |_| async {
                called.store(true, Ordering::SeqCst);
                false
            })
            .await
            .unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
        assert!(!called.load(Ordering::SeqCst));
        // 检查进行中（慢）时，另一个占名字的操作要等它做完
        let ws = f.ws.clone();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let o1 = order.clone();
        let unlinking = tokio::spawn(async move {
            ws.unlink("proj-a", |_| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                o1.lock().unwrap().push("checked");
                false
            })
            .await
            .unwrap();
            o1.lock().unwrap().push("unlinked");
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        f.ws.create("late", false, None).await.unwrap();
        order.lock().unwrap().push("created");
        unlinking.await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["checked", "unlinked", "created"]);
    }

    // ======================================================================= link_roots 经过符号链接、系统别名、探测一致性、检查之后换目录

    /// `link_roots = ["~", "~/work/shared"]`，`shared` 事后被换成指向 `~/.ssh` 的符号链接。
    /// 以前 link `~/.ssh` 成功、`fs.read` 读出私钥、`browse ~/.ssh` 也能进；没有快照 / 词法路径检查时下面第一个断言就红。
    #[tokio::test]
    async fn a_link_root_swapped_for_a_symlink_into_a_denied_directory_unlocks_nothing() {
        // `shared` 在 daemon 启动（拍快照）时是个普通目录
        let f = link_fixture_with("[\"~\", \"~/work/shared\"]", |home, _| {
            std::fs::create_dir_all(home.join("work/shared")).unwrap();
        })
        .await;
        std::fs::create_dir_all(f.home.join(".ssh/keys")).unwrap();
        std::fs::write(f.home.join(".ssh/keys/id_ed25519"), "PRIVATE KEY").unwrap();
        // 普通目录时：照常（`shared` 不在拒绝项里，~/.ssh 本来就不能关联）
        assert_eq!(code_of(f.ws.link("~/.ssh", Some("ssh")).await), ErrorCode::Forbidden);
        // 换成指向 ~/.ssh 的链接
        std::fs::remove_dir_all(f.home.join("work/shared")).unwrap();
        std::os::unix::fs::symlink(f.home.join(".ssh"), f.home.join("work/shared")).unwrap();
        for path in ["~/.ssh", "~/.ssh/keys", "~/work/shared", "~/work/shared/keys"] {
            let e = f.ws.link(path, Some("stolen")).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "link {path}: {}", e.message);
            let e = f.ws.browse(Some(path), true).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "browse {path}: {}", e.message);
        }
        assert!(f.ws.list(&HashMap::new()).await.projects.is_empty());
        assert_eq!(code_of(f.ws.fs_read("stolen/keys/id_ed25519", None).await), ErrorCode::NotFound);
        // 这一项被忽略了（canonical 路径和快照对不上），所以它不再算范围：browse 缺省仍是 `~`
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.roots, vec![f.home.to_string_lossy()]);
    }

    /// 一开始配置的 root 就是符号链接：它的 canonical 路径在拒绝项里、词法路径不在，例外不生效（并且启动时 warn）。
    #[tokio::test]
    async fn a_link_root_that_is_a_symlink_into_a_denied_directory_from_the_start_gets_no_exception() {
        let f = link_fixture_with("[\"~\", \"~/work/shared\"]", |home, _| {
            std::fs::create_dir_all(home.join(".ssh/keys")).unwrap();
            std::fs::create_dir_all(home.join("work")).unwrap();
            std::os::unix::fs::symlink(home.join(".ssh"), home.join("work/shared")).unwrap();
        })
        .await;
        for path in ["~/.ssh", "~/.ssh/keys", "~/work/shared", "~/work/shared/keys"] {
            assert_eq!(code_of(f.ws.link(path, Some("x")).await), ErrorCode::Forbidden, "{path}");
        }
        assert_eq!(f.ws.browse(None, false).await.unwrap().roots, vec![f.home.to_string_lossy()]);
        // 要放行就写真实路径：词法路径和 canonical 路径一致，例外生效
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/.ssh/keys\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link("~/.ssh/keys", Some("keys")).await.unwrap().name, "keys");
        assert_eq!(code_of(f.ws.link("~/.ssh", Some("ssh")).await), ErrorCode::Forbidden);
    }

    /// 每次请求都重判 `link_roots` 的每一项（没有快照）：目录事后变成符号链接，这一项立刻失效；换回来立刻生效。
    #[tokio::test]
    async fn link_roots_are_judged_again_on_every_request() {
        let f = link_fixture("[\"~\", \"~/.config/nvim\"]").await;
        std::fs::create_dir_all(f.home.join(".config/nvim/lua")).unwrap();
        std::fs::create_dir_all(f.home.join(".config/other")).unwrap();
        // 目录存在、路径里没有符号链接：生效
        assert_eq!(f.ws.link("~/.config/nvim", Some("nvim")).await.unwrap().name, "nvim");
        unlink(&f.ws, "nvim").await.unwrap();
        // 事后换成符号链接：这一项失效
        std::fs::rename(f.home.join(".config/nvim"), f.home.join(".config/nvim.real")).unwrap();
        std::os::unix::fs::symlink(f.home.join(".config/nvim.real"), f.home.join(".config/nvim")).unwrap();
        assert_eq!(code_of(f.ws.link("~/.config/nvim/lua", Some("lua")).await), ErrorCode::Forbidden);
        assert_eq!(f.ws.ignored_link_roots().len(), 1);
        // 配置没变的重载不改变判定
        f.config.reload().unwrap();
        assert_eq!(code_of(f.ws.link("~/.config/nvim/lua", Some("lua")).await), ErrorCode::Forbidden);
        // 换回普通目录：立刻又生效（不等重启 / 重载）
        std::fs::remove_file(f.home.join(".config/nvim")).unwrap();
        std::fs::rename(f.home.join(".config/nvim.real"), f.home.join(".config/nvim")).unwrap();
        assert_eq!(f.ws.link("~/.config/nvim/lua", Some("lua")).await.unwrap().name, "lua");
        assert!(f.ws.ignored_link_roots().is_empty());
        // 改成写真实路径的另一项也行
        assert_eq!(code_of(f.ws.link("~/.config/other", Some("o")).await), ErrorCode::Forbidden);
    }

    /// `["~", "~/work/shared"]`，`shared` 在 daemon 启动**之前**就是指向 `~/.cargo` 的符号链接（= 重启之后）。
    /// 以前（快照）重拍时认下链接的目标，`~/.cargo` 当作「显式列出的隐藏目录」放开：能关联并读出里面的文件。
    #[tokio::test]
    async fn a_symlinked_root_in_place_at_startup_cannot_open_a_hidden_directory() {
        let f = link_fixture_with("[\"~\", \"~/work/shared\"]", |home, _| {
            std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
            std::fs::write(home.join(".cargo/credentials.toml"), "token = \"dummy-value\"").unwrap();
            std::fs::create_dir_all(home.join("work")).unwrap();
            std::os::unix::fs::symlink(home.join(".cargo"), home.join("work/shared")).unwrap();
        })
        .await;
        for path in ["~/.cargo", "~/.cargo/bin", "~/work/shared", "~/work/shared/bin"] {
            let e = f.ws.link(path, Some("cargo")).await.unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "link {path}: {}", e.message);
            // 隐藏目录可以浏览（里面的条目各自判断），但整个目录和它下面的都不可关联
            let r = f.ws.browse(Some(path), true).await.unwrap();
            assert_eq!((r.linkable, r.blocked), (false, Some(LinkBlock::Hidden)), "browse {path}");
            assert!(r.entries.iter().all(|e| !e.linkable), "{path}: {:?}", r.entries);
        }
        assert!(f.ws.list(&HashMap::new()).await.projects.is_empty());
        assert_eq!(code_of(f.ws.fs_read("cargo/credentials.toml", None).await), ErrorCode::NotFound);
        // 范围里也没有 `.cargo`，被忽略的项有原因
        assert_eq!(f.ws.browse(None, false).await.unwrap().roots, vec![f.home.to_string_lossy()]);
        let ignored = f.ws.ignored_link_roots();
        assert_eq!(ignored.len(), 1);
        assert!(ignored[0].1.contains("symbolic link") && ignored[0].1.contains("write the real path"), "{ignored:?}");
        // 要放行就写真实路径
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/.cargo\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link("~/.cargo", Some("cargo")).await.unwrap().name, "cargo");
        assert!(f.ws.ignored_link_roots().is_empty());
    }

    /// 范围：`shared` 是指向 `~` 的符号链接，配置热加载成 `["~/work", "~/work/shared"]`：
    /// 以前 `shared` 的目标（整个 `~`）被当成 root，`~/Documents/taxes` 能关联；现在范围只有 `~/work`。
    #[tokio::test]
    async fn a_symlinked_root_cannot_widen_the_scope_after_a_hot_reload() {
        let f = link_fixture_with("[\"~\"]", |home, _| {
            std::fs::create_dir_all(home.join("Documents/taxes")).unwrap();
            std::fs::create_dir_all(home.join("work")).unwrap();
            std::os::unix::fs::symlink(home, home.join("work/shared")).unwrap();
        })
        .await;
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~/work\", \"~/work/shared\"]").unwrap();
        assert!(f.config.reload().unwrap());
        assert_eq!(code_of(f.ws.link("~/Documents/taxes", Some("taxes")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.browse(Some("~/Documents"), false).await), ErrorCode::Forbidden);
        assert_eq!(f.ws.browse(None, false).await.unwrap().roots, vec![f.home.join("work").to_string_lossy()]);
        assert_eq!(f.ws.link("~/work/proj-a", None).await.unwrap().name, "proj-a");
        assert_eq!(f.ws.ignored_link_roots().len(), 1);
    }

    /// `~/code -> ~/real-code`：root 是指向普通目录的链接 → 整项忽略，原因是「请写真实路径」。
    #[tokio::test]
    async fn a_root_that_is_a_symlink_to_an_ordinary_directory_is_ignored_with_a_hint() {
        let f = link_fixture_with("[\"~/code\"]", |home, _| {
            std::fs::create_dir_all(home.join("real-code/app")).unwrap();
            std::os::unix::fs::symlink(home.join("real-code"), home.join("code")).unwrap();
        })
        .await;
        let ignored = f.ws.ignored_link_roots();
        assert_eq!(ignored.len(), 1, "{ignored:?}");
        assert!(ignored[0].1.contains("real-code") && ignored[0].1.contains("write the real path"), "{ignored:?}");
        assert_eq!(code_of(f.ws.browse(None, false).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/code/app", None).await), ErrorCode::Forbidden);
        // 写真实路径就行
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~/real-code\"]").unwrap();
        f.config.reload().unwrap();
        assert_eq!(f.ws.link("~/real-code/app", None).await.unwrap().name, "app");
        assert!(f.ws.ignored_link_roots().is_empty());
    }

    /// HOME 本身在符号链接后面：`~` 按 canonical 的 HOME 展开，`["~"]` 照常可用，也不 warn；
    /// 经过链接的绝对写法同样被拒绝表挡住。
    #[tokio::test]
    async fn a_home_directory_behind_a_symlink_is_fine() {
        let f = link_fixture("[\"~\"]").await;
        std::fs::create_dir_all(f.home.join(".ssh")).unwrap();
        let via = f.base.join("home-link");
        std::os::unix::fs::symlink(&f.home, &via).unwrap();
        let mut env = (*f.env).clone();
        env.insert("HOME".into(), via.to_string_lossy().into_owned());
        let ws =
            Workspace::new(&f.root, f.config.clone(), f.store.clone(), Clients::new(), Arc::new(env)).await.unwrap();
        assert!(ws.ignored_link_roots().is_empty(), "{:?}", ws.ignored_link_roots());
        assert_eq!(ws.browse(None, false).await.unwrap().roots, vec![f.home.to_string_lossy()]);
        assert_eq!(ws.link("~/work/proj-a", None).await.unwrap().path, f.home.join("work/proj-a").to_string_lossy());
        assert_eq!(ws.link(&format!("{}/work/proj-b", via.display()), None).await.unwrap().name, "proj-b");
        for p in [format!("{}/.ssh", via.display()), format!("{}/.ssh/nope", via.display()), "~/.ssh".to_string()] {
            assert_eq!(code_of(ws.link(&p, Some("x")).await), ErrorCode::Forbidden, "{p}");
        }
    }

    /// `/tmp`、`/private/tmp`、`/var/folders/…` 这些经系统别名的写法都能用，也都不算「被忽略」（不 warn）。
    #[tokio::test]
    async fn system_alias_spellings_of_a_root_are_usable_and_not_reported_as_ignored() {
        let f = link_fixture("[]").await;
        let aliases = system_aliases();
        let tmp = crate::sys::canonicalize("/tmp").unwrap();
        let mut cases: Vec<(String, PathBuf)> = vec![("/tmp".into(), tmp.clone())];
        if Path::new("/private/tmp").exists() {
            cases.push(("/private/tmp".into(), PathBuf::from("/private/tmp")));
        }
        // 一个在系统临时目录里、真实存在的目录，用短写法（`/var/folders/…`）和长写法（`/private/var/folders/…`）都写一遍
        let external = f.base.join("external");
        std::fs::create_dir_all(external.join("proj")).unwrap();
        cases.push((external.to_string_lossy().into_owned(), external.clone()));
        if let Some((alias, real)) = aliases.iter().find(|(_, real)| external.starts_with(real)) {
            let short = alias.join(external.strip_prefix(real).unwrap());
            assert_ne!(short, external);
            cases.push((short.to_string_lossy().into_owned(), external.clone()));
        }
        for (written, real) in cases {
            let roots = serde_json::to_string(&[&written]).unwrap();
            crate::config::set_value(&f.paths, "workspace.link_roots", &roots).unwrap();
            f.config.reload().unwrap();
            assert!(f.ws.ignored_link_roots().is_empty(), "{written}: {:?}", f.ws.ignored_link_roots());
            let r = f.ws.browse(None, false).await.unwrap_or_else(|e| panic!("{written}: {e:?}"));
            assert_eq!(r.roots, vec![real.to_string_lossy().into_owned()], "{written}");
            if real == external {
                // 显式放行的 root 之下可用，用短写法探测同样可用
                assert_eq!(
                    f.ws.link(&format!("{written}/proj"), Some("proj")).await.unwrap().name,
                    "proj",
                    "{written}"
                );
                unlink(&f.ws, "proj").await.unwrap();
            }
        }
    }

    /// browse / link 不能用来探测拒绝表目录里有没有某个文件；`link` 的「不存在」和「不是目录」同一个错误。
    #[tokio::test]
    async fn probing_for_files_in_denied_directories_gives_identical_answers() {
        let f = link_fixture("[\"~\"]").await;
        std::fs::create_dir_all(f.home.join(".ssh")).unwrap();
        std::fs::write(f.home.join(".ssh/id_ed25519"), "k").unwrap();
        std::fs::create_dir_all(f.home.join(".aws")).unwrap();
        for (present, absent) in [
            ("~/.ssh/id_ed25519", "~/.ssh/nope"),
            ("~/.ssh", "~/.ssh/nope/deeper"),
            ("~/.aws", "~/.aws/credentials"),
            ("~/.anycon", "~/.anycon/keys/node.key"),
        ] {
            for probe in ["link", "browse"] {
                let ws = &f.ws;
                let run = |p: &'static str| async move {
                    let e = if probe == "link" {
                        ws.link(p, Some("x")).await.unwrap_err()
                    } else {
                        ws.browse(Some(p), true).await.unwrap_err()
                    };
                    (e.stable_code(), e.message)
                };
                let (a, b) = (run(present).await, run(absent).await);
                assert_eq!(a.0, Some(ErrorCode::Forbidden), "{probe} {present}");
                assert_eq!(a.0, b.0, "{probe}: {present} vs {absent}");
                // 消息里没有被探测的名字
                assert!(!a.1.contains("nope") && !b.1.contains("nope"), "{} / {}", a.1, b.1);
            }
        }
        // 范围内的普通路径：不存在和是文件给同一个 not_found（link）
        std::fs::write(f.home.join("work/notes.txt"), "n").unwrap();
        let a = f.ws.link("~/work/notes.txt", Some("x")).await.unwrap_err();
        let b = f.ws.link("~/work/missing", Some("x")).await.unwrap_err();
        assert_eq!((a.stable_code(), &a.message), (b.stable_code(), &b.message));
        assert_eq!(a.stable_code(), Some(ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn the_new_denied_locations_cannot_be_linked_with_a_fake_home() {
        let f = link_fixture("[\"~\"]").await;
        for d in
            ["bin", "go/bin", "Library/LaunchAgents", "Library/Keychains", ".config/gh", ".config/fish", "go/src/x"]
        {
            std::fs::create_dir_all(f.home.join(d)).unwrap();
        }
        for p in
            ["~/bin", "~/go/bin", "~/Library/LaunchAgents", "~/Library/Keychains", "~/.config/gh", "~/.config/fish"]
        {
            assert_eq!(code_of(f.ws.link(p, Some("x")).await), ErrorCode::Forbidden, "{p}");
        }
        assert_eq!(f.ws.link("~/go/src/x", Some("gox")).await.unwrap().name, "gox");
        // 显式放行 ~/Library 不放行 LaunchAgents / Keychains
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\", \"~/Library\"]").unwrap();
        f.config.reload().unwrap();
        std::fs::create_dir_all(f.home.join("Library/Application Support")).unwrap();
        assert_eq!(f.ws.link("~/Library/Application Support", Some("as")).await.unwrap().name, "as");
        assert_eq!(code_of(f.ws.link("~/Library/LaunchAgents", Some("la")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(f.ws.link("~/Library/Keychains", Some("kc")).await), ErrorCode::Forbidden);
    }

    /// 检查之后、打开之前把目录换成符号链接，`fs.read` / `fs.list` 也读不到 / 列不到边界外。
    /// 钩子在 `open` 之前换（竞态本身）；没有「打开之后按句柄真实路径复核」时 `fs_read` 会读出 `top secret`。
    #[tokio::test]
    async fn fs_read_and_list_survive_a_directory_swapped_after_the_check() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        std::fs::create_dir_all(f.root.join("rf/sub")).unwrap();
        std::fs::write(f.root.join("rf/sub/a.txt"), "inside").unwrap();
        std::fs::write(f.home.join("work/proj-a/sub/a.txt"), "inside").unwrap();
        // 关联项目：`sub` 在检查之后被换成指向 outside 的链接
        let outside = f.base.join("outside");
        std::fs::write(outside.join("a.txt"), "top secret").unwrap();
        for (project, root_dir) in [("proj-a", f.home.join("work/proj-a")), ("rf", f.root.join("rf"))] {
            let sub = root_dir.join("sub");
            let target = sub.join("a.txt");
            let (sub2, outside2, root2) = (sub.clone(), outside.clone(), root_dir.clone());
            let guard = crate::fsopen::add_before_open_hook(Box::new(move |p: &Path| {
                if p.starts_with(&root2) && p.ends_with("sub") || p == sub2.join("a.txt") {
                    let _ = std::fs::remove_dir_all(&sub2);
                    let _ = std::fs::remove_file(&sub2);
                    std::os::unix::fs::symlink(&outside2, &sub2).unwrap();
                }
            }));
            let read = f.ws.fs_read(&format!("{project}/sub/a.txt"), None).await;
            std::fs::remove_file(&sub).unwrap();
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(&target, "inside").unwrap();
            let listed = f.ws.fs_list(&format!("{project}/sub"));
            drop(guard);
            let r = read.map(|r| r.text);
            assert!(
                matches!(&r, Err(e) if e.stable_code() == Some(ErrorCode::Forbidden)),
                "{project}: read must not reach outside: {r:?}"
            );
            assert!(
                matches!(&listed, Err(e) if e.stable_code() == Some(ErrorCode::Forbidden)),
                "{project}: list must not reach outside: {listed:?}"
            );
            // 恢复后照常
            std::fs::remove_file(&sub).ok();
            std::fs::remove_dir_all(&sub).ok();
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(&target, "inside").unwrap();
            assert_eq!(f.ws.fs_read(&format!("{project}/sub/a.txt"), None).await.unwrap().text, "inside");
            assert_eq!(f.ws.fs_list(&format!("{project}/sub")).unwrap().entries[0].name, "a.txt");
        }
    }

    /// 根目录下 NFD 名字的文件夹：对外（列表、会话里存的、`session.list` 过滤）都是盘上原样的名字。
    #[tokio::test]
    async fn a_decomposed_folder_name_is_reported_exactly_as_it_is_on_disk() {
        let f = link_fixture("[\"~\"]").await;
        let nfd = "Re\u{301}sume\u{301}";
        let nfc = "R\u{e9}sum\u{e9}";
        std::fs::create_dir_all(f.root.join(nfd)).unwrap();
        let on_disk: Vec<String> = std::fs::read_dir(&f.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('R'))
            .collect();
        assert_eq!(on_disk.len(), 1);
        let disk_name = on_disk[0].clone(); // 在 APFS 上保留 NFD 写法，别的文件系统也是建出来的样子
        assert_eq!(f.ws.canonical_project_name(nfc), disk_name);
        assert_eq!(f.ws.canonical_project_name(nfd), disk_name);
        let list = f.ws.list(&HashMap::from([(nfc.to_string(), 2)])).await;
        let p = list.projects.iter().find(|p| p.name == disk_name).expect("listed under its on-disk name");
        assert_eq!(p.active_sessions, 2, "sessions stored under a different spelling still count");
        assert_eq!(f.ws.project(nfc, 0).await.unwrap().name, disk_name);
        assert!(f.ws.same_project_name(&disk_name, nfc) && f.ws.same_project_name(&disk_name, nfd));
        // 还不存在的项目：NFC
        assert_eq!(f.ws.canonical_project_name("Cafe\u{301}"), "Caf\u{e9}");
    }

    // ======================================================================= 「不存在」与「是文件」不可区分、项目外的硬链接

    /// 拒绝表之外，`browse` 和 `link` 对「不存在」和「不是目录」给同一个 `not_found` 和同一条消息；
    /// 经符号链接（指向拒绝表里的 `~/.ssh`）探测里面的文件也一样。
    #[tokio::test]
    async fn browse_and_link_do_not_tell_a_file_from_a_missing_path() {
        let f = link_fixture("[\"~\"]").await;
        std::fs::write(f.home.join(".netrc"), "machine dummy-value").unwrap();
        std::fs::create_dir_all(f.home.join(".ssh")).unwrap();
        std::fs::write(f.home.join(".ssh/id_ed25519"), "k").unwrap();
        std::os::unix::fs::symlink(f.home.join(".ssh"), f.home.join("work/sshlink")).unwrap();
        for (file, missing) in [
            ("~/.netrc", "~/.nope"),
            ("~/work/notes.txt", "~/work/nope.txt"),
            ("~/work/sshlink/id_ed25519", "~/work/sshlink/nope"),
        ] {
            let ws = &f.ws;
            let browse = |p: &'static str| async move { ws.browse(Some(p), true).await.unwrap_err() };
            let link = |p: &'static str| async move { ws.link(p, Some("x")).await.unwrap_err() };
            let (a, b) = (browse(file).await, browse(missing).await);
            assert_eq!((a.stable_code(), &a.message), (b.stable_code(), &b.message), "browse {file} vs {missing}");
            assert_eq!(a.stable_code(), Some(ErrorCode::NotFound), "browse {file}");
            let (a, b) = (link(file).await, link(missing).await);
            assert_eq!((a.stable_code(), &a.message), (b.stable_code(), &b.message), "link {file} vs {missing}");
            assert_eq!(a.stable_code(), Some(ErrorCode::NotFound), "link {file}");
        }
    }

    /// 项目里的文件在项目外另有一个硬链接，别处 `stat` 了外面的名字（macOS 上 `F_GETPATH` 因此变成外面的路径）：
    /// 项目里的名字照样读得出来。
    #[tokio::test]
    async fn fs_read_works_for_a_file_that_has_a_hard_link_outside_the_project() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        let inside = f.home.join("work/proj-a/hl.txt");
        let outside = f.base.join("outside/hl-outside.txt");
        std::fs::write(&inside, "same inode, two names").unwrap();
        std::fs::hard_link(&inside, &outside).unwrap();
        let (inside2, outside2) = (inside.clone(), outside.clone());
        let guard = crate::fsopen::add_after_open_hook(Box::new(move |p: &Path| {
            if p == inside2 {
                let _ = std::fs::metadata(&outside2); // 先 stat 外面那个名字
            }
        }));
        let r = f.ws.fs_read("proj-a/hl.txt", None).await;
        drop(guard);
        assert_eq!(r.unwrap().text, "same inode, two names");
        // 列目录不受影响
        assert!(f.ws.fs_list("proj-a").unwrap().entries.iter().any(|e| e.name == "hl.txt"));
    }

    // ===================================================================== 词法范围与大小写 / NFC、目录身份、firmlink、HOME 固定

    /// `link` 和 `browse` 对同一个路径的回答（错误码 + 消息）。
    type Answers = [(Option<ErrorCode>, String); 2];

    async fn answers(ws: &Workspace, p: &str) -> Answers {
        let l = ws.link(p, Some("probe")).await.unwrap_err();
        let b = ws.browse(Some(p), true).await.unwrap_err();
        [(l.stable_code(), l.message), (b.stable_code(), b.message)]
    }

    /// 用 `/Volumes/<启动盘>`（指向 `/` 的链接）这类「范围外的别名」拼出 HOME 下的路径。
    /// 以前存在的文件 -> `not_found`、不存在的 -> `forbidden`「outside」、拒绝表子目录 -> `forbidden`「protected」，
    /// 存在的普通子目录还会被接受。现在先看词法路径是否在 root 之内：不在就一律是同一句「outside link_roots」。
    /// 这里用 `<base>/bootdisk -> <base>` 模拟。
    #[tokio::test]
    async fn a_spelling_outside_the_scope_gets_the_same_answer_for_every_kind_of_path() {
        let f = link_fixture("[\"~\"]").await;
        std::fs::create_dir_all(f.home.join(".ssh/keys")).unwrap();
        std::fs::write(f.home.join(".ssh/id_x"), "k").unwrap();
        std::os::unix::fs::symlink(&f.base, f.base.join("bootdisk")).unwrap();
        let via = |rel: &str| format!("{}/bootdisk/home/{rel}", f.base.display());
        let mut seen: Vec<(String, Answers)> = Vec::new();
        for rel in [
            "work/notes.txt", // 存在的文件
            "work/nope",      // 不存在
            ".ssh/id_x",      // 拒绝表里存在的文件
            ".ssh/keys",      // 拒绝表子目录
            ".ssh/nope",      // 拒绝表里不存在的
            "work/proj-a",    // 存在的普通子目录（以前会被接受）
            "work/proj-a/sub",
        ] {
            seen.push((rel.to_string(), answers(&f.ws, &via(rel)).await));
        }
        let first = seen[0].1.clone();
        for (rel, a) in &seen {
            assert_eq!(a, &first, "{rel}");
        }
        assert_eq!(first[0].0, Some(ErrorCode::Forbidden));
        assert!(first[0].1.contains("outside workspace.link_roots"), "{first:?}");
        // 同一个地方的规范写法照常可用（范围里的不受影响）
        assert_eq!(f.ws.link("~/work/proj-a", None).await.unwrap().name, "proj-a");
        assert!(f.ws.list(&HashMap::new()).await.projects.iter().all(|p| p.name == "proj-a"));
    }

    /// 不区分大小写的卷：`~/.SSH/…`、`~/LIBRARY/…` 这样的大小写变体。以前词法层逐字节比较，存在的拒绝表子目录 -> `forbidden`，
    /// 不存在的 -> `not_found`。现在词法层按 NFC + 小写折叠比较（`case_insensitive` 时），存在与否同一个回答。
    #[tokio::test]
    async fn case_variants_of_denied_locations_get_the_same_answer_whether_or_not_they_exist() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.set_case_insensitive_for_test(true);
        std::fs::create_dir_all(f.home.join(".ssh/keys")).unwrap();
        std::fs::write(f.home.join(".ssh/id_x"), "k").unwrap();
        std::fs::create_dir_all(f.home.join("Library/Caches")).unwrap();
        let mut all = Vec::new();
        for p in [
            "~/.SSH/id_x",
            "~/.SSH/keys",
            "~/.SSH/nope",
            "~/.SSH/nope/deeper",
            "~/.ssh/nope",
            "~/.Ssh",
            "~/LIBRARY/Caches",
            "~/LIBRARY/nope",
            "~/library/Caches",
        ] {
            let a = answers(&f.ws, p).await;
            assert_eq!(a[0].0, Some(ErrorCode::Forbidden), "{p}: {a:?}");
            assert!(a[0].1.contains("protected"), "{p}: {a:?}");
            assert_eq!(a[0], a[1], "link and browse agree for {p}");
            all.push((p, a));
        }
        // 只有名字不同的两条（.ssh 与 Library）消息不同，同一个目录下的所有变体消息相同
        let ssh: Vec<_> =
            all.iter().filter(|(p, _)| p.to_lowercase().contains(".ssh")).map(|(_, a)| a.clone()).collect();
        assert!(ssh.windows(2).all(|w| w[0] == w[1]), "{ssh:?}");
    }

    /// 跳过「这个卷区分大小写」的环境：夹具里的 `work` 用大写能不能找到同一个目录。
    fn volume_is_case_insensitive(home: &Path) -> bool {
        match (std::fs::metadata(home.join("work")), std::fs::metadata(home.join("WORK"))) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }

    /// `["~/WORK"]`（盘上是 `~/work`）以前被当成「经过符号链接」整项忽略；现在逐段 `lstat`，没有链接就生效，
    /// 以 canonical 路径作为 root。
    #[tokio::test]
    async fn a_root_written_with_different_case_is_usable_when_no_segment_is_a_link() {
        let f = link_fixture("[\"~/WORK\"]").await;
        if !volume_is_case_insensitive(&f.home) {
            return; // 区分大小写的卷上 `~/WORK` 就是另一个（不存在的）目录
        }
        assert!(f.ws.ignored_link_roots().is_empty(), "{:?}", f.ws.ignored_link_roots());
        assert!(f.ws.link_roots_report().is_empty(), "{:?}", f.ws.link_roots_report());
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.roots, vec![f.home.join("work").to_string_lossy().into_owned()], "the real spelling on disk");
        // 用户写 `~/WORK/…` 和 `~/work/…` 都行
        f.ws.set_case_insensitive_for_test(true);
        assert_eq!(f.ws.link("~/WORK/proj-a", None).await.unwrap().name, "proj-a");
        assert_eq!(f.ws.link("~/work/proj-b", None).await.unwrap().name, "proj-b");
        // 范围之外的仍然不行
        assert_eq!(code_of(f.ws.link("~/other", Some("o")).await), ErrorCode::Forbidden);
    }

    /// NFC 写法的 `~/Café`（盘上是 NFD）。macOS 的 APFS 对两种写法是同一个目录。
    #[tokio::test]
    async fn a_root_written_in_nfc_is_usable_when_the_directory_on_disk_is_nfd() {
        let nfc_name = "Caf\u{e9}";
        let nfd_name = "Cafe\u{301}";
        let f = link_fixture_with(&format!("[\"~/{nfc_name}\"]"), |home, _| {
            std::fs::create_dir_all(home.join(nfd_name).join("app")).unwrap();
        })
        .await;
        if std::fs::symlink_metadata(f.home.join(nfc_name)).is_err() {
            return; // 这个文件系统把 NFC 和 NFD 当作两个名字（Linux）
        }
        assert!(f.ws.link_roots_report().is_empty(), "{:?}", f.ws.link_roots_report());
        let r = f.ws.browse(None, false).await.unwrap();
        assert_eq!(r.roots.len(), 1);
        assert_eq!(nfc(&r.roots[0]), nfc(&f.home.join(nfc_name).to_string_lossy()));
        assert_eq!(f.ws.link(&format!("~/{nfc_name}/app"), None).await.unwrap().name, "app");
    }

    /// 真的经过符号链接（`~/code -> ~/real-code`）的仍然被忽略；没有可用的 root 时错误消息列出每一项和原因
    /// （目录不存在的也列），但不带符号链接指向哪里（那个路径只给本机管理员看：日志和 `config set` 的输出）。
    #[tokio::test]
    async fn with_no_usable_root_the_error_lists_each_entry_and_why() {
        let f = link_fixture_with("[\"~/code\", \"~/nope\"]", |home, _| {
            std::fs::create_dir_all(home.join("real-code/app")).unwrap();
            std::os::unix::fs::symlink(home.join("real-code"), home.join("code")).unwrap();
        })
        .await;
        for e in [f.ws.browse(None, false).await.unwrap_err(), f.ws.link("~/code/app", None).await.unwrap_err()] {
            assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
            assert!(e.message.contains("~/code") && e.message.contains("symbolic link"), "{}", e.message);
            assert!(e.message.contains("~/nope") && e.message.contains("does not exist"), "{}", e.message);
            assert!(!e.message.contains("real-code"), "the link target stays out of the client message: {}", e.message);
        }
        // 完整的原因（本机管理员看）：被忽略的有真实路径，不存在的也在报告里
        let report = f.ws.link_roots_report();
        assert_eq!(report.len(), 2, "{report:?}");
        assert!(!report[0].missing && report[0].reason.contains("real-code"), "{report:?}");
        assert!(report[1].missing && report[1].entry == "~/nope", "{report:?}");
        // `ignored_link_roots` 只含确实被忽略的（warn 用）
        assert_eq!(f.ws.ignored_link_roots().len(), 1);
    }

    /// 「在里面 / 包含」按 `(dev, ino)` 沿祖先链判断。用 `<base>/alias -> <base>/sys` 造出同一个目录的另一种写法，
    /// 候选目录用这种非 canonical 的写法给出，路径前缀比较认不出来，身份比较认得出。
    #[test]
    fn absolute_denials_follow_directory_identity_not_the_spelling() {
        use crate::linkrules::{LinkCtx, LinkRoots, PathAliases};
        use anycon_protocol::LinkBlock as B;
        let d = tempfile::tempdir().unwrap();
        let base = crate::sys::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(base.join("sys/home/me/anycon/proj")).unwrap();
        std::fs::create_dir_all(base.join("sys/home/me/.anycon/keys")).unwrap();
        std::fs::create_dir_all(base.join("sys/home/me/code")).unwrap();
        std::os::unix::fs::symlink(base.join("sys"), base.join("alias")).unwrap();
        let ctx = |root: PathBuf, data: PathBuf| {
            let mut c = LinkCtx {
                roots: LinkRoots { raw: vec![base.clone()], canon: vec![base.clone()] },
                workspace_roots: vec![root.clone()],
                data: data.clone(),
                data_lexical: data,
                root_ids: Vec::new(),
                data_ids: Default::default(),
                deny: vec![],
                aliases: PathAliases::default(),
                tcc: vec![],
                linked: vec![],
                case_insensitive: false,
            };
            c.refresh_identities();
            c
        };
        let c = ctx(base.join("sys/home/me/anycon"), base.join("sys/home/me/.anycon"));
        let alias = base.join("alias/home/me");
        // 包含 workspace 根 / 数据目录：路径前缀认不出来（`alias/…` 不是 `sys/…` 的前缀），身份认得出
        assert!(!c.workspace_roots[0].starts_with(&alias));
        assert_eq!(c.block_of(&alias).map(|b| b.block), Some(B::Workspace));
        // 在 workspace 根里面（Invalid）/ 就是它（Forbidden）
        let b = c.block_of(&alias.join("anycon/proj")).unwrap();
        assert_eq!((b.block, b.code), (B::Workspace, ErrorCode::Invalid));
        let b = c.block_of(&alias.join("anycon")).unwrap();
        assert_eq!((b.block, b.code), (B::Workspace, ErrorCode::Forbidden));
        // 数据目录（里面不可浏览，包含它的可以）
        let c2 = ctx(base.join("elsewhere/anycon"), base.join("sys/home/me/.anycon"));
        let b = c2.block_of(&alias.join(".anycon/keys")).unwrap();
        assert_eq!((b.block, b.enterable), (B::DataDir, false));
        let b = c2.block_of(&alias.join(".anycon")).unwrap();
        assert_eq!((b.block, b.enterable), (B::DataDir, false));
        let b = c2.block_of(&alias).unwrap();
        assert_eq!((b.block, b.enterable), (B::DataDir, true), "it contains the data directory");
        // 无关的目录不受影响
        assert_eq!(c.block_of(&alias.join("code")).map(|b| b.block), None);
    }

    /// firmlink（`/System/Volumes/Data/X` 和 `/X` 是同一个目录，`realpath` 对两种写法都原样返回）：
    /// 身份相同的前缀换成系统卷的写法。测试里用 `<base>/datavol` 当数据卷前缀、`<base>` 当系统卷，
    /// `<base>/datavol/home` 是指向 `<base>/home` 的链接（身份相同）模拟。
    #[test]
    fn firmlink_spellings_are_normalized_when_the_identities_match() {
        use crate::linkrules::normalize_firmlinks;
        let d = tempfile::tempdir().unwrap();
        let base = crate::sys::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(base.join("home/me/work")).unwrap();
        std::fs::create_dir_all(base.join("datavol/usr/local/bin")).unwrap();
        std::fs::create_dir_all(base.join("usr/local")).unwrap();
        std::fs::create_dir_all(base.join("datavol/only-on-data")).unwrap();
        std::os::unix::fs::symlink(base.join("home"), base.join("datavol/home")).unwrap();
        // `usr` 在两边是不同的目录，`usr/local` 才是 firmlink
        std::fs::remove_dir_all(base.join("datavol/usr/local")).unwrap();
        std::os::unix::fs::symlink(base.join("usr/local"), base.join("datavol/usr/local")).unwrap();
        let fl = vec![(base.join("datavol"), base.clone())];
        assert_eq!(normalize_firmlinks(&base.join("datavol/home/me/work"), &fl), base.join("home/me/work"));
        assert_eq!(normalize_firmlinks(&base.join("datavol/home/../home/me"), &fl), base.join("home/me"));
        assert_eq!(normalize_firmlinks(&base.join("datavol/usr/local/bin"), &fl), base.join("usr/local/bin"));
        // 另一边没有对应的目录 / 不是同一个目录：不动
        assert_eq!(normalize_firmlinks(&base.join("datavol/only-on-data/x"), &fl), base.join("datavol/only-on-data/x"));
        assert_eq!(normalize_firmlinks(&base.join("datavol/usr/other"), &fl), base.join("datavol/usr/other"));
        assert_eq!(normalize_firmlinks(&base.join("home/me"), &fl), base.join("home/me"), "not under the data prefix");
        assert_eq!(normalize_firmlinks(&base.join("datavol"), &fl), base.join("datavol"));
    }

    /// HOME 的 `/System/Volumes/Data/…` 写法（这里模拟成 `<base>/datavol/home`）写进 `link_roots` 也能用，
    /// 而且和规范写法判成同一个 root（以前整项被当成「经过符号链接」忽略；真机上 `realpath` 对两种写法都原样返回，
    /// 这个 root 会以另一种写法生效，绕开按路径字符串比较的绝对拒绝）。
    #[tokio::test]
    async fn a_root_in_firmlink_spelling_is_the_same_root_as_the_normal_spelling() {
        let f = link_fixture("[]").await;
        let datavol = f.base.join("datavol");
        std::fs::create_dir_all(&datavol).unwrap();
        std::os::unix::fs::symlink(&f.home, datavol.join("home")).unwrap();
        f.ws.add_firmlink_for_test(&datavol, &f.base);
        let fl_home = datavol.join("home");
        let roots = serde_json::to_string(&[fl_home.join("work").to_string_lossy()]).unwrap();
        crate::config::set_value(&f.paths, "workspace.link_roots", &roots).unwrap();
        f.config.reload().unwrap();
        assert!(f.ws.link_roots_report().is_empty(), "{:?}", f.ws.link_roots_report());
        assert_eq!(f.ws.browse(None, false).await.unwrap().roots, vec![f.home.join("work").to_string_lossy()]);
        // 两种写法的请求都能关联，登记的是 canonical 路径
        let p = f.ws.link(&format!("{}/work/proj-a", fl_home.display()), None).await.unwrap();
        assert_eq!(p.path, f.home.join("work/proj-a").to_string_lossy());
        assert_eq!(f.ws.link(&format!("{}/work/proj-b", f.home.display()), None).await.unwrap().name, "proj-b");
        // 包含 workspace 根 / 数据目录的上级（用 firmlink 写法）照样被拒绝
        for up in [fl_home.clone(), f.home.clone()] {
            let roots = serde_json::to_string(&[up.to_string_lossy()]).unwrap();
            crate::config::set_value(&f.paths, "workspace.link_roots", &roots).unwrap();
            f.config.reload().unwrap();
            assert_eq!(code_of(f.ws.link(&up.to_string_lossy(), Some("up")).await), ErrorCode::Forbidden, "{up:?}");
            assert_eq!(code_of(f.ws.link(&format!("{}/anycon", up.display()), Some("up")).await), ErrorCode::Forbidden);
            assert_eq!(
                code_of(f.ws.link(&format!("{}/.anycon", up.display()), Some("up")).await),
                ErrorCode::Forbidden
            );
        }
    }

    /// 检查刚过、取身份之前目录被换成指向边界外的链接。以前 `fs.read` 在这之后按路径 `stat` 取「身份」，
    /// 换在这之前的话身份就是外面那个文件的，和 `F_GETPATH` 报出的外面的路径对上，读出了外面的内容。
    /// 现在逐段 `openat`，没有身份比较，在被换的那一段就拒绝。
    #[tokio::test]
    async fn fs_read_refuses_a_directory_swapped_right_after_the_boundary_check() {
        let f = link_fixture("[\"~\"]").await;
        f.ws.link("~/work/proj-a", None).await.unwrap();
        let outside = f.base.join("outside/sub");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("g.txt"), "dummy-value-outside").unwrap();
        let (sub, outside2) = (f.home.join("work/proj-a/sub"), outside.clone());
        let target = sub.join("g.txt");
        let t2 = target.clone();
        let guard = crate::fsopen::add_after_resolve_hook(Box::new(move |p: &Path| {
            if p == t2 {
                std::fs::remove_dir_all(&sub).unwrap();
                std::os::unix::fs::symlink(&outside2, &sub).unwrap();
            }
        }));
        let r = f.ws.fs_read("proj-a/sub/g.txt", None).await;
        drop(guard);
        let e = r.expect_err("must not read the file behind the swapped directory");
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden), "{e:?}");
        assert!(!e.message.contains("dummy-value"));
    }

    /// HOME 是一个链接，daemon 运行中它被改指向上一级目录。以前每个请求重新解析 HOME，`~` 的范围和拒绝表跟着
    /// 一起挪走，原来 HOME 下的 `Library/LaunchAgents` 变得可以关联。现在 HOME 的 canonical 路径在启动时固定：拒绝表仍然指着原来的目录，
    /// `~` 开头的项按被忽略处理。
    #[tokio::test]
    async fn re_pointing_the_home_link_at_runtime_moves_neither_the_scope_nor_the_deny_table() {
        let f = link_fixture("[]").await;
        std::fs::create_dir_all(f.home.join("Library/LaunchAgents")).unwrap();
        let link = f.base.join("home-link");
        std::os::unix::fs::symlink(&f.home, &link).unwrap();
        let mut env = (*f.env).clone();
        env.insert("HOME".into(), link.to_string_lossy().into_owned());
        // `~` 和整个临时目录都是 root：临时目录在 /private/var/… 里，显式列出才能关联
        let roots = serde_json::to_string(&["~".to_string(), f.base.to_string_lossy().into_owned()]).unwrap();
        crate::config::set_value(&f.paths, "workspace.link_roots", &roots).unwrap();
        f.config.reload().unwrap();
        let ws =
            Workspace::new(&f.root, f.config.clone(), f.store.clone(), Clients::new(), Arc::new(env)).await.unwrap();
        let denied = f.home.join("Library/LaunchAgents");
        assert!(ws.link_roots_report().is_empty(), "{:?}", ws.link_roots_report());
        assert_eq!(code_of(ws.link(&denied.to_string_lossy(), Some("la")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(ws.link("~/Library/LaunchAgents", Some("la")).await), ErrorCode::Forbidden);
        // 运行中把 HOME 这个链接改指向上一级（f.base）
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&f.base, &link).unwrap();
        // 原来 HOME 下的拒绝表子目录仍然不可关联（拒绝表指着启动时的 canonical HOME）
        assert_eq!(code_of(ws.link(&denied.to_string_lossy(), Some("la")).await), ErrorCode::Forbidden);
        assert_eq!(code_of(ws.link("~/Library/LaunchAgents", Some("la")).await), ErrorCode::Forbidden);
        assert_eq!(
            code_of(ws.link(&format!("{}/Library/LaunchAgents", link.display()), Some("la")).await),
            ErrorCode::Forbidden
        );
        // `~` 开头的项被忽略并说明原因；绝对写法的项不受影响
        let report = ws.link_roots_report();
        assert_eq!(report.len(), 1, "{report:?}");
        assert_eq!(report[0].entry, "~");
        assert!(report[0].reason.contains("HOME") && report[0].reason.contains("restart"), "{report:?}");
        assert_eq!(ws.browse(None, false).await.unwrap().roots, vec![f.base.to_string_lossy()]);
        // 只有 `~` 的配置：没有可用的 root，错误消息里有原因
        crate::config::set_value(&f.paths, "workspace.link_roots", "[\"~\"]").unwrap();
        f.config.reload().unwrap();
        let e = ws.browse(None, false).await.unwrap_err();
        assert!(e.message.contains('~') && e.message.contains("HOME"), "{}", e.message);
    }

    // ---------------- 多个 workspace 根（ADR-45） ----------------

    /// 夹具：主根 `<base>/home/anycon`，额外根 `<base>/ext1`、`<base>/gone`（不存在，模拟没插的盘）、`<base>/ext2`
    /// （配置顺序就是这样）。`link_roots = [<base>]`（它在拒绝表里的系统临时目录之下时，显式写出来才放行）。
    struct MultiFx {
        f: LinkFx,
        ext1: PathBuf,
        ext2: PathBuf,
        gone: PathBuf,
    }

    async fn multi_fixture() -> MultiFx {
        let f = link_fixture_cfg(
            |base| {
                let q = |p: PathBuf| format!("{:?}", p.to_string_lossy());
                format!(
                    "[workspace]\nlink_roots = [{}]\nextra_roots = [{}, {}, {}]\n",
                    q(base.to_path_buf()),
                    q(base.join("ext1")),
                    q(base.join("gone")),
                    q(base.join("ext2"))
                )
            },
            |_, base| {
                std::fs::create_dir_all(base.join("ext1")).unwrap();
                std::fs::create_dir_all(base.join("ext2")).unwrap();
            },
        )
        .await;
        let (ext1, ext2, gone) = (f.base.join("ext1"), f.base.join("ext2"), f.base.join("gone"));
        MultiFx { f, ext1, ext2, gone }
    }

    fn project_roots(list: &WorkspaceListResult) -> Vec<(String, Option<String>)> {
        let mut v: Vec<_> = list.projects.iter().map(|p| (p.name.clone(), p.root.clone())).collect();
        v.sort();
        v
    }

    fn some(p: &Path) -> Option<String> {
        Some(p.to_string_lossy().into_owned())
    }

    #[tokio::test]
    async fn startup_lists_the_roots_skips_a_missing_one_and_gives_each_available_root_a_trash() {
        let m = multi_fixture().await;
        let roots = m.f.ws.roots_info();
        assert_eq!(
            roots.iter().map(|r| (r.path.clone(), r.primary, r.available)).collect::<Vec<_>>(),
            vec![
                (m.f.root.to_string_lossy().into_owned(), true, true),
                (m.ext1.to_string_lossy().into_owned(), false, true),
                (m.gone.to_string_lossy().into_owned(), false, false),
                (m.ext2.to_string_lossy().into_owned(), false, true),
            ],
            "the primary root first, the extra roots in configuration order; the missing one is listed as unavailable"
        );
        assert!(m.ext1.join(TRASH_DIR).is_dir() && m.ext2.join(TRASH_DIR).is_dir());
        assert!(!m.ext1.join(SCRATCH_DIR).exists(), "scratch lives only in the primary root");
        assert!(!m.gone.exists(), "a missing root is not created");
        // 外接盘中途拔掉：启动时可用的根现在目录没了，`available` 跟着变
        std::fs::remove_dir_all(&m.ext2).unwrap();
        assert!(!m.f.ws.roots_info()[3].available);
    }

    #[tokio::test]
    async fn a_bad_extra_root_configuration_stops_the_workspace_from_starting() {
        let f = link_fixture("[\"~\"]").await;
        let inner = f.root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        crate::config::set_value(&f.paths, "workspace.extra_roots", &format!("[{:?}]", inner.to_string_lossy()))
            .unwrap();
        f.config.reload().unwrap();
        let e = Workspace::new(&f.root, f.config.clone(), f.store.clone(), Clients::new(), f.env.clone())
            .await
            .err()
            .expect("an extra root inside the primary root is a configuration error");
        assert!(matches!(e, WorkspaceError::Roots(_)), "{e}");
        assert!(e.to_string().contains("workspace.extra_roots") && e.to_string().contains("inside"), "{e}");
    }

    #[tokio::test]
    async fn projects_of_every_root_are_listed_with_their_root_and_resolve_to_it() {
        let m = multi_fixture().await;
        for (root, name) in [(&m.f.root, "a"), (&m.ext1, "b"), (&m.ext2, "c")] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(m.ext1.join("b/note.txt"), "in ext1").unwrap();
        std::fs::write(m.ext2.join("not-a-project.txt"), "x").unwrap();
        let list = m.f.ws.list(&HashMap::new()).await;
        assert_eq!(list.root, m.f.root.to_string_lossy(), "`root` stays the primary root");
        assert_eq!(
            project_roots(&list),
            vec![
                ("a".to_string(), some(&m.f.root)),
                ("b".to_string(), some(&m.ext1)),
                ("c".to_string(), some(&m.ext2))
            ]
        );
        let b = list.projects.iter().find(|p| p.name == "b").unwrap();
        assert_eq!(b.path, m.ext1.join("b").to_string_lossy());
        assert_eq!(m.f.ws.project_path("b").unwrap(), m.ext1.join("b"));
        assert_eq!(m.f.ws.project("c", 0).await.unwrap().root, some(&m.ext2));
        // 关联项目没有「所在的根」
        let linked = m.f.ws.link(&m.f.base.join("outside/dir").to_string_lossy(), Some("out")).await.unwrap();
        assert_eq!((linked.kind, linked.root), (ProjectKind::Linked, None));
        // fs.*：第一段是哪个根下的项目，就相对哪个根；空路径列主根
        assert_eq!(m.f.ws.fs_read("b/note.txt", None).await.unwrap().text, "in ext1");
        let names = |l: FsListResult| l.entries.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(m.f.ws.fs_list("b").unwrap()), vec!["note.txt"]);
        let top = names(m.f.ws.fs_list("").unwrap());
        assert!(top.contains(&"a".to_string()) && !top.contains(&"b".to_string()), "{top:?}");
        // 项目里指向它所在根之外的符号链接照样被拒绝，哪怕指向另一个根
        std::os::unix::fs::symlink(m.f.root.join("a"), m.ext1.join("b/to-primary")).unwrap();
        assert_eq!(code_of(m.f.ws.fs_list("b/to-primary")), ErrorCode::Forbidden);
        // 没插上的根下没有项目，也解析不到
        assert_eq!(code_of(m.f.ws.fs_list("nope")), ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_name_found_in_several_places_resolves_linked_then_primary_then_extra_roots_in_order() {
        let mut m = multi_fixture().await;
        let f = &m.f;
        // 额外根之间：按配置顺序，ext1 在 ext2 前
        for root in [&m.ext1, &m.ext2] {
            std::fs::create_dir_all(root.join("dup")).unwrap();
        }
        std::fs::write(m.ext1.join("dup/who"), "ext1").unwrap();
        std::fs::write(m.ext2.join("dup/who"), "ext2").unwrap();
        let list = f.ws.list(&HashMap::new()).await;
        assert_eq!(project_roots(&list), vec![("dup".to_string(), some(&m.ext1))]);
        assert_eq!(f.ws.fs_read("dup/who", None).await.unwrap().text, "ext1");
        assert_eq!(f.ws.project_path("dup").unwrap(), m.ext1.join("dup"));
        // 主根胜过额外根
        std::fs::create_dir_all(f.root.join("dup")).unwrap();
        std::fs::write(f.root.join("dup/who"), "primary").unwrap();
        let list = f.ws.list(&HashMap::new()).await;
        assert_eq!(project_roots(&list), vec![("dup".to_string(), some(&f.root))]);
        assert_eq!(f.ws.fs_read("dup/who", None).await.unwrap().text, "primary");
        assert_eq!(f.ws.project("dup", 0).await.unwrap().root, some(&f.root));
        // 每个输的文件夹每个名字只 warn 一次（ext1 和 ext2 的两个都记在同一个名字下）
        for _ in 0..3 {
            f.ws.list(&HashMap::new()).await;
        }
        assert_eq!(f.ws.shadow_warned.lock().unwrap().len(), 1);
        // 关联项目胜过所有根下的同名文件夹
        std::fs::create_dir_all(f.base.join("outside/dir")).unwrap();
        std::fs::write(f.base.join("outside/dir/who"), "linked").unwrap();
        std::fs::remove_dir_all(f.root.join("dup")).unwrap();
        std::fs::remove_dir_all(m.ext1.join("dup")).unwrap();
        std::fs::remove_dir_all(m.ext2.join("dup")).unwrap();
        let linked = f.ws.link(&f.base.join("outside/dir").to_string_lossy(), Some("dup")).await.unwrap();
        assert_eq!(linked.kind, ProjectKind::Linked);
        for root in [&f.root, &m.ext1, &m.ext2] {
            std::fs::create_dir_all(root.join("dup")).unwrap();
        }
        let list = f.ws.list(&HashMap::new()).await;
        assert_eq!(list.projects.iter().filter(|p| p.name == "dup").count(), 1);
        assert_eq!(list.projects.iter().find(|p| p.name == "dup").unwrap().kind, ProjectKind::Linked);
        assert_eq!(f.ws.fs_read("dup/who", None).await.unwrap().text, "linked");
        // 赢家换了（主根的同名文件夹没了，露出额外根里的）：同名项目换了落脚的目录，发 `updated` 让客户端重新拉
        unlink(&f.ws, "dup").await.unwrap();
        f.ws.rescan_and_notify().await;
        drain(&mut m.f.rx, methods::WORKSPACE_CHANGED);
        std::fs::remove_dir_all(m.f.root.join("dup")).unwrap();
        m.f.ws.rescan_and_notify().await;
        let events = drain(&mut m.f.rx, methods::WORKSPACE_CHANGED);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].updated, vec!["dup".to_string()]);
        assert!(events[0].added.is_empty() && events[0].removed.is_empty(), "{events:?}");
        assert_eq!(m.f.ws.project("dup", 0).await.unwrap().root, some(&m.ext1));
    }

    #[tokio::test]
    async fn a_file_with_a_projects_name_in_an_earlier_root_does_not_hide_the_folder_in_a_later_one() {
        let m = multi_fixture().await;
        std::fs::write(m.f.root.join("both"), "a file").unwrap();
        std::fs::create_dir_all(m.ext2.join("both")).unwrap();
        let list = m.f.ws.list(&HashMap::new()).await;
        assert_eq!(project_roots(&list), vec![("both".to_string(), some(&m.ext2))]);
        assert_eq!(m.f.ws.project_path("both").unwrap(), m.ext2.join("both"), "lookup agrees with the list");
        // 但这个名字在主根里被占着：新建 / 克隆 / 改名过来都是 exists
        assert_eq!(code_of(m.f.ws.create("both", false, None).await), ErrorCode::Exists);
    }

    #[tokio::test]
    async fn create_and_clone_go_to_the_requested_root_and_reject_anything_else() {
        let m = multi_fixture().await;
        let ws = &m.f.ws;
        // 缺省 = 主根
        assert_eq!(ws.create("p0", false, None).await.unwrap(), m.f.root.join("p0"));
        // 指定根：和 node.info 列出的路径一致
        assert_eq!(ws.create("p1", false, Some(&m.ext1.to_string_lossy())).await.unwrap(), m.ext1.join("p1"));
        assert!(m.ext1.join("p1").is_dir() && !m.f.root.join("p1").exists());
        let p1 = ws.project("p1", 0).await.unwrap();
        assert_eq!(p1.root, some(&m.ext1));
        // 同一个目录的另一种写法（`..`、符号链接）也认；`~` 展开
        let dotted = format!("{}/../ext2", m.ext2.display());
        assert_eq!(ws.create("p2", false, Some(&dotted)).await.unwrap(), m.ext2.join("p2"));
        let alias = m.f.base.join("alias-of-ext2");
        std::os::unix::fs::symlink(&m.ext2, &alias).unwrap();
        assert_eq!(ws.create("p3", false, Some(&alias.to_string_lossy())).await.unwrap(), m.ext2.join("p3"));
        assert_eq!(ws.create("p4", false, Some("~/anycon")).await.unwrap(), m.f.root.join("p4"));
        // git init 也在那个根里
        ws.create("repo", true, Some(&m.ext1.to_string_lossy())).await.unwrap();
        assert!(m.ext1.join("repo/.git").exists());
        // 不是任何一个根：invalid（根下面的子目录、别的目录、相对路径、空串、控制字符）
        for bad in [
            m.ext1.join("p1").to_string_lossy().into_owned(),
            m.f.base.join("outside/dir").to_string_lossy().into_owned(),
            "ext1".to_string(),
            String::new(),
            "/tmp\u{7}".to_string(),
        ] {
            assert_eq!(code_of(ws.create("nope", false, Some(&bad)).await), ErrorCode::Invalid, "{bad:?}");
        }
        assert!(!m.ext1.join("nope").exists() && !m.f.root.join("nope").exists());
        // 配置里有、但现在不可用的根：not_found，什么都没建
        let e = ws.create("nope", false, Some(&m.gone.to_string_lossy())).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::NotFound));
        assert!(e.message.contains("not available"), "{}", e.message);
        assert!(!m.gone.exists());
        // 名字全局唯一：别的根里有同名项目也是 exists
        assert_eq!(code_of(ws.create("p0", false, Some(&m.ext1.to_string_lossy())).await), ErrorCode::Exists);
        assert_eq!(code_of(ws.create("p1", false, None).await), ErrorCode::Exists);
        assert_eq!(code_of(ws.create("p1", false, Some(&m.ext2.to_string_lossy())).await), ErrorCode::Exists);
        // 克隆：根的校验同 create，目标目录在那个根里占好位
        assert_eq!(
            code_of(ws.clone_start("https://example.invalid/x/y.git", Some("c0"), Some("/not/a/root")).await),
            ErrorCode::Invalid
        );
        assert_eq!(
            code_of(
                ws.clone_start("https://example.invalid/x/y.git", Some("c0"), Some(&m.gone.to_string_lossy())).await
            ),
            ErrorCode::NotFound
        );
        assert_eq!(
            code_of(
                ws.clone_start("https://example.invalid/x/y.git", Some("p1"), Some(&m.ext2.to_string_lossy())).await
            ),
            ErrorCode::Exists,
            "the name is taken in another root"
        );
        let _job =
            ws.clone_start("ssh://127.0.0.1:1/x/c1.git", Some("c1"), Some(&m.ext2.to_string_lossy())).await.unwrap();
        assert!(m.ext2.join("c1").is_dir(), "the target directory is claimed in the requested root");
        assert!(!m.f.root.join("c1").exists());
        assert_eq!(code_of(ws.create("c1", false, None).await), ErrorCode::Exists);
        // 克隆失败只清掉自己在那个根里建的目录
        let t = std::time::Instant::now();
        while m.ext2.join("c1").exists() && t.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!m.ext2.join("c1").exists());
        assert!(m.ext2.join("p2").is_dir(), "other projects in that root are untouched");
    }

    #[tokio::test]
    async fn rename_stays_in_the_projects_root_and_the_name_is_checked_across_roots() {
        let m = multi_fixture().await;
        let ws = &m.f.ws;
        ws.create("x", false, Some(&m.ext1.to_string_lossy())).await.unwrap();
        ws.create("taken", false, None).await.unwrap();
        ws.create("y", false, Some(&m.ext2.to_string_lossy())).await.unwrap();
        std::fs::write(m.ext1.join("x/f"), "content").unwrap();
        // 别的根里的名字：exists
        assert_eq!(code_of(ws.rename("x", "taken").await), ErrorCode::Exists);
        assert_eq!(code_of(ws.rename("x", "y").await), ErrorCode::Exists);
        // 改名之后还在原来的根里
        ws.rename("x", "x2").await.unwrap();
        assert!(m.ext1.join("x2/f").is_file() && !m.ext1.join("x").exists() && !m.f.root.join("x2").exists());
        assert_eq!(ws.project("x2", 0).await.unwrap().root, some(&m.ext1));
        // scratch 只在主根，转项目也落在主根
        let scratch = ws.scratch_dir("sess_7").unwrap();
        assert!(scratch.starts_with(&m.f.root));
        assert!(!m.ext1.join(SCRATCH_DIR).exists() && !m.ext2.join(SCRATCH_DIR).exists());
        assert_eq!(ws.promote_scratch("sess_7", "from-scratch").await.unwrap(), m.f.root.join("from-scratch"));
        assert_eq!(code_of(ws.promote_scratch("sess_8", "x2").await), ErrorCode::NotFound);
        ws.scratch_dir("sess_9").unwrap();
        assert_eq!(code_of(ws.promote_scratch("sess_9", "x2").await), ErrorCode::Exists, "x2 lives in ext1");
    }

    #[tokio::test]
    async fn every_root_has_its_own_trash_and_restore_purge_and_cleanup_cover_all_of_them() {
        let m = multi_fixture().await;
        let ws = &m.f.ws;
        ws.create("t0", false, None).await.unwrap();
        ws.create("t1", false, Some(&m.ext1.to_string_lossy())).await.unwrap();
        ws.create("t2", false, Some(&m.ext2.to_string_lossy())).await.unwrap();
        std::fs::write(m.ext1.join("t1/keep"), "k").unwrap();
        let e0 = ws.trash("t0").await.unwrap();
        let e1 = ws.trash("t1").await.unwrap();
        let e2 = ws.trash("t2").await.unwrap();
        // 各进各自根的 .trash（同一个文件系统，rename 不跨盘）
        assert!(m.f.root.join(TRASH_DIR).join(&e0.id).is_dir());
        assert!(m.ext1.join(TRASH_DIR).join(&e1.id).is_dir());
        assert!(m.ext2.join(TRASH_DIR).join(&e2.id).is_dir());
        assert_eq!(
            (e0.root.as_deref(), e1.root.as_deref(), e2.root.as_deref()),
            (Some(m.f.root.to_str().unwrap()), Some(m.ext1.to_str().unwrap()), Some(m.ext2.to_str().unwrap()))
        );
        assert!(ws.list(&HashMap::new()).await.projects.is_empty());
        // 汇总，标出来自哪个根
        let list = ws.trash_list().await;
        let mut seen: Vec<(String, Option<String>)> =
            list.iter().map(|e| (e.original_name.clone(), e.root.clone())).collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("t0".to_string(), some(&m.f.root)),
                ("t1".to_string(), some(&m.ext1)),
                ("t2".to_string(), some(&m.ext2))
            ]
        );
        assert!(list.iter().find(|e| e.original_name == "t1").unwrap().bytes >= 1);
        // restore 回到原来的根
        assert_eq!(ws.restore(&e1.id).await.unwrap(), "t1");
        assert_eq!(std::fs::read_to_string(m.ext1.join("t1/keep")).unwrap(), "k");
        assert!(!m.f.root.join("t1").exists());
        assert_eq!(ws.project("t1", 0).await.unwrap().root, some(&m.ext1));
        // 原名被占：exists（哪个根里占的都算）
        ws.create("t2", false, None).await.unwrap();
        assert_eq!(code_of(ws.restore(&e2.id).await), ErrorCode::Exists);
        // purge 找得到任何根的条目
        ws.purge(&e2.id).await.unwrap();
        assert!(!m.ext2.join(TRASH_DIR).join(&e2.id).exists());
        assert_eq!(code_of(ws.purge(&e2.id).await), ErrorCode::NotFound);
        assert_eq!(code_of(ws.restore("../x-20260101T000000Z").await), ErrorCode::Invalid);
        // 自动清理覆盖所有根
        let old = now_ms() - 40 * 86_400_000;
        for root in [&m.f.root, &m.ext1, &m.ext2] {
            std::fs::create_dir_all(root.join(TRASH_DIR).join(format!(
                "old-{}-{}",
                root.file_name().unwrap().to_string_lossy(),
                fmt_compact(old)
            )))
            .unwrap();
        }
        assert_eq!(ws.cleanup_trash().await, 3, "the expired entries of all three roots, nothing else");
        assert_eq!(ws.trash_list().await.len(), 1, "only the fresh t0 is left");
        // 同一秒内再次删除同名项目：id 在所有根之间唯一，撞上就让客户端稍后重试
        ws.create("clash", false, Some(&m.ext1.to_string_lossy())).await.unwrap();
        let first = ws.trash("clash").await.unwrap();
        ws.create("clash", false, Some(&m.ext2.to_string_lossy())).await.unwrap();
        let again = ws.trash("clash").await;
        match again {
            Ok(e) => assert_ne!(e.id, first.id, "a different second gives a different id"),
            Err(e) => assert_eq!(e.stable_code(), Some(ErrorCode::Exists), "same second, other root's trash"),
        }
    }

    #[tokio::test]
    async fn the_watcher_covers_every_available_root() {
        let mut m = multi_fixture().await;
        let _guard = m.f.ws.spawn_watcher().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::create_dir(m.ext2.join("w-ext2")).unwrap();
        let n = next_changed(&mut m.f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.added, vec!["w-ext2".to_string()]);
        std::fs::create_dir(m.ext1.join("w-ext1")).unwrap();
        let n = next_changed(&mut m.f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.added, vec!["w-ext1".to_string()]);
        // 改名只在同一个根里认：在 ext1 里 mv
        std::fs::rename(m.ext1.join("w-ext1"), m.ext1.join("w-ext1b")).unwrap();
        let n = next_changed(&mut m.f.rx, methods::WORKSPACE_CHANGED, Duration::from_secs(8)).await.expect("event");
        let ch: WorkspaceChanged = serde_json::from_value(n.params).unwrap();
        assert_eq!(ch.renamed, vec![RenamedProject { from: "w-ext1".into(), to: "w-ext1b".into() }]);
        // 各根的 .trash 里的变化不是项目增删
        std::fs::create_dir(m.ext1.join(TRASH_DIR).join("junk")).unwrap();
        assert!(next_changed(&mut m.f.rx, methods::WORKSPACE_CHANGED, Duration::from_millis(1200)).await.is_none());
    }

    #[test]
    fn only_top_level_visible_entries_of_a_root_are_relevant_to_the_watcher() {
        let roots = vec![PathBuf::from("/w/main"), PathBuf::from("/mnt/ext")];
        for yes in ["/w/main", "/w/main/p", "/mnt/ext/p", "/mnt/ext", "/elsewhere/odd"] {
            assert!(event_is_relevant(&roots, Path::new(yes)), "{yes}");
        }
        for no in ["/w/main/.trash", "/w/main/p/src/x.rs", "/mnt/ext/.trash/p-1", "/mnt/ext/.hidden", "/mnt/ext/p/.git"]
        {
            assert!(!event_is_relevant(&roots, Path::new(no)), "{no}");
        }
    }

    #[tokio::test]
    async fn the_linking_rules_apply_to_every_root() {
        let m = multi_fixture().await;
        let (f, ws) = (&m.f, &m.f.ws);
        ws.create("in-primary", false, None).await.unwrap();
        ws.create("in-ext1", false, Some(&m.ext1.to_string_lossy())).await.unwrap();
        async fn link_to(ws: &Workspace, p: &Path) -> RpcResult<Project> {
            ws.link(&p.to_string_lossy(), Some("l")).await
        }
        // 额外根里面的目录已经是项目 / 在 .trash 里：invalid；根本身、包含根的目录：forbidden
        assert_eq!(code_of(link_to(ws, &m.ext1.join("in-ext1")).await), ErrorCode::Invalid);
        assert_eq!(code_of(link_to(ws, &m.ext1.join(TRASH_DIR)).await), ErrorCode::Invalid);
        assert_eq!(code_of(link_to(ws, &f.root.join("in-primary")).await), ErrorCode::Invalid);
        assert_eq!(code_of(link_to(ws, &m.ext1).await), ErrorCode::Forbidden);
        assert_eq!(code_of(link_to(ws, &m.ext2).await), ErrorCode::Forbidden);
        assert_eq!(
            code_of(link_to(ws, &f.base).await),
            ErrorCode::Forbidden,
            "contains every root and the data directory"
        );
        assert_eq!(code_of(link_to(ws, &f.base.join("gone")).await), ErrorCode::NotFound, "nothing is there to link");
        // 一个普通目录照常可以
        assert_eq!(link_to(ws, &f.base.join("outside/dir")).await.unwrap().kind, ProjectKind::Linked);
        // browse：额外根里的项目标成项目，不可关联；根里的目录不能再关联
        let b = ws.browse(Some(&m.ext1.to_string_lossy()), true).await.unwrap();
        assert_eq!((b.linkable, b.blocked), (false, Some(LinkBlock::Workspace)));
        let e = b.entries.iter().find(|e| e.name == "in-ext1").unwrap();
        assert_eq!((e.project.as_deref(), e.linkable), (Some("in-ext1"), false));
        let b = ws.browse(Some(&f.base.to_string_lossy()), true).await.unwrap();
        for name in ["ext1", "ext2"] {
            let e = b.entries.iter().find(|e| e.name == name).unwrap();
            assert_eq!((e.linkable, e.blocked), (false, Some(LinkBlock::Workspace)), "{name}");
        }
        // 规则引擎看得到所有根，包括这次启动时不可用的那个（只剩路径前缀可比：它之后被挂上时，关联项目复核会拒绝落进去的）
        let ctx = ws.link_ctx_lenient();
        assert!(ctx.workspace_roots.contains(&m.ext2) && ctx.workspace_roots.contains(&m.gone));
        assert_eq!(ctx.block_of(&m.ext2.join("anything")).map(|b| b.block), Some(LinkBlock::Workspace));
        assert_eq!(ctx.block_of(&m.gone.join("anything")).map(|b| b.block), Some(LinkBlock::Workspace));
        assert_eq!(ctx.block_of(&m.gone).map(|b| b.block), Some(LinkBlock::Workspace));
        assert_eq!(ctx.resolve_linked(&m.ext1.join("in-ext1")), Err(LinkedProblem::NotAllowed(LinkBlock::Workspace)));
    }

    #[tokio::test]
    async fn a_name_that_moved_to_another_roots_disk_is_a_different_directory_but_a_moved_root_is_not() {
        // 用真实的设备号：根所在的盘 = `dev`
        let d = tempfile::tempdir().unwrap();
        let (dev, ino) = crate::sys::identity_of(d.path()).unwrap();
        let paths = Paths::new(d.path().join("home"));
        std::fs::create_dir_all(&paths.home).unwrap();
        let store = Store::open(&paths.db()).await.unwrap();
        let config = ConfigStore::with_config(paths, Config::default());
        let ws = Workspace::new(&d.path().join("ws"), config, store, Clients::new(), Arc::new(HashMap::new()))
            .await
            .unwrap();
        let rec = |dev: u64, ino: u64| format!("folder:p#{dev}:{ino}");
        // 同一个设备：看 inode
        assert_eq!(ws.check_loc(&rec(dev, 5), &rec(dev, 5)), LocCheck::Same);
        assert_eq!(ws.check_loc(&rec(dev, 5), &rec(dev, 6)), LocCheck::Different);
        // 记下的设备已经不是任何一个根所在的盘：整个根搬了盘，接受并重新记下
        let gone_dev = dev.wrapping_add(7919);
        assert_eq!(ws.check_loc(&rec(gone_dev, 5), &rec(dev, ino)), LocCheck::Refresh(rec(dev, ino)));
        // 记下的设备现在还是某个根所在的盘，名字却指向了别的设备上的目录：是另一个根里的另一个目录，拒绝续接
        assert_eq!(ws.check_loc(&rec(dev, 5), &rec(gone_dev, 5)), LocCheck::Different);
        // 旧版本只记了名字：接受
        assert_eq!(ws.check_loc("folder:p", &rec(dev, 5)), LocCheck::Refresh(rec(dev, 5)));
    }
}
