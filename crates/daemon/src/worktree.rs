//! 每会话 git worktree 的磁盘与 git 部分（ADR-34，docs/spec/daemon.md § 10.1）。
//!
//! 本模块**不碰会话状态**：分配分支名 / 目录名、`git worktree add` / `remove`、`.worktreeinclude` 复制、`info/exclude`、
//! 目录复核（逐段 `lstat` + `(dev, ino)` + `.git` 指针文件）、归档 / 删除时的处置、分支列表、`git worktree repair`。
//! 会话状态机怎么用它见 `sessions/wt.rs`。
//!
//! 安全约束（ADR-34 第 9 条）：
//! - 只删 `worktrees` 表里记过的目录，删之前逐段 `lstat` 无符号链接且 `(dev, ino)` 与创建时记下的一致（[`verify`]）；
//!   `creating`（崩溃残局）的清理先确认没有别的记录用着同一个路径 / 分支，`failed` 的记录只改库；
//! - 不把主仓库的 `.git` 加进 agent 的可写目录（本模块不涉及 agent）；
//! - `.worktreeinclude` 只复制普通文件，不跟随符号链接，不执行任何脚本；
//! - 所有 git 调用走 [`crate::gitops::Git`]（关掉 fsmonitor / hooks / 外部 diff）。
//!
//! git 操作都不在会话锁里跑；调用方（`Sessions`）负责这一点。

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_protocol::{ErrorCode, RpcError, WorkspaceBranchesResult, WorktreeDisposal, WorktreeInfo, WorktreeState};
use rand::seq::SliceRandom;
use rand::Rng;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::config::ConfigStore;
use crate::gitops::{parse_status_v2, Git, GitOut, StatusV2};
use crate::store::{Store, WorktreeRow};
use crate::util::{new_worktree_id, now_ms};
use crate::workspace::{worktree_ready_fresh, Workspace};

type RpcResult<T> = Result<T, RpcError>;

/// worktree 目录在项目里的位置（相对项目目录）。
pub const WORKTREES_REL: &str = ".anycon/worktrees";
/// 写进 `<git common dir>/info/exclude` 的一行。
pub const EXCLUDE_LINE: &str = "/.anycon/";
/// 分支列表每组最多这么多个。
pub const BRANCHES_MAX: usize = 200;

const ADD_TIMEOUT: Duration = Duration::from_secs(300);

pub fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

/// 带稳定 `data.reason` 的错误。
pub fn err_reason(code: ErrorCode, msg: impl Into<String>, reason: &str) -> RpcError {
    let mut e = RpcError::new(code, msg);
    if let Some(d) = e.data.as_mut() {
        d.reason = Some(reason.to_string());
    }
    e
}

// ---------------------------------------------------------------- 名字

const ADJECTIVES: &[&str] = &[
    "brave", "bright", "calm", "clever", "cosmic", "crisp", "eager", "gentle", "happy", "jolly", "keen", "lively",
    "merry", "nimble", "proud", "quick", "quiet", "rapid", "shiny", "silent", "smooth", "steady", "sunny", "swift",
    "tidy", "vivid", "witty", "zesty", "bold", "mellow",
];
const NOUNS: &[&str] = &[
    "badger", "canyon", "cedar", "comet", "dolphin", "ember", "falcon", "fjord", "glacier", "harbor", "heron",
    "island", "juniper", "lagoon", "maple", "marmot", "meadow", "nebula", "orchid", "otter", "pebble", "puffin",
    "quartz", "raven", "river", "summit", "thicket", "walrus", "willow", "zephyr",
];

/// 从第一条消息里的英文单词生成 slug（daemon 不调模型）：取 `[A-Za-z0-9]+` 的词，转小写，最多 4 个用 `-` 连，截到 32 个字符。
/// 这几个词里一个长度 ≥ 2 的都没有（中文消息、没有 prompt、`a b c`）返回 `None`，调用方改用随机词。
pub fn slug_from_prompt(prompt: &str) -> Option<String> {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if !words.iter().any(|w| w.len() >= 2) {
        return None;
    }
    let mut slug = words.join("-");
    slug.truncate(32);
    let slug = slug.trim_end_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

/// 内置词表里随机取 `<形容词>-<名词>`。
pub fn random_slug<R: Rng>(rng: &mut R) -> String {
    format!("{}-{}", ADJECTIVES.choose(rng).copied().unwrap_or("brave"), NOUNS.choose(rng).copied().unwrap_or("otter"))
}

/// 去重：`base`、`base-2`、`base-3`…，取第一个 `taken` 说没被占用的。
pub fn dedupe_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let c = format!("{base}-{n}");
        if !taken(&c) {
            return c;
        }
    }
    format!("{base}-{:06x}", rand::thread_rng().gen_range(0..0xff_ffff))
}

/// worktree 目录名 = 分支名最后一段，只保留 `[a-z0-9._-]`（先转小写，其余换成 `-`），不以 `.` / `-` 开头；空了就用 `wt`。
pub fn dir_name_for_branch(branch: &str) -> String {
    let last = branch.rsplit('/').next().unwrap_or(branch);
    let mapped: String = last
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let mut name = mapped.trim_start_matches(['.', '-']).to_string();
    name.truncate(64);
    if name.is_empty() {
        "wt".into()
    } else {
        name
    }
}

/// `base` 的文本规则：不以 `-` 开头、不含控制字符、≤ 255 字节、不为空。
pub fn check_ref_text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && !s.starts_with('-') && !s.chars().any(char::is_control)
}

// ---------------------------------------------------------------- 目录复核

/// `.anycon/worktrees/<name>`：恰好三段，没有空段 / `.` / `..` / NUL。
pub fn rel_path_ok(rel: &str) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    segs.len() == 3
        && segs[0] == ".anycon"
        && segs[1] == "worktrees"
        && segs.iter().all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains('\0'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WtCheck {
    /// 链上每一段都是真目录（不是符号链接），`identity` 是最后一段现在的 `(dev, ino)`。
    Ok { dir: PathBuf, identity: (u64, u64) },
    /// 路径（或链上某一段）不存在。
    Gone,
    /// 路径存在但不是原来那个：符号链接、不是目录、`(dev, ino)` 不一致、`.git` 不是普通文件。
    Replaced(String),
    /// 目录身份没变，只是 `.git` 指针文件指向的不再是记下的那个管理目录（改名后 `git worktree repair` 没跑成、被 agent 改了…）：
    /// 提示用户在项目目录里运行 `git worktree repair`，不是「把路径移走」。
    PointerChanged,
}

/// 从项目目录起逐段 `lstat`：任何一段是符号链接 / 不是目录 → `Replaced`；不存在 → `Gone`。不看 `(dev, ino)`。
pub fn inspect(project_dir: &Path, rel: &str) -> WtCheck {
    if !rel_path_ok(rel) {
        return WtCheck::Replaced("the recorded path is not a worktree path".into());
    }
    let mut cur = project_dir.to_path_buf();
    let segs: Vec<&str> = rel.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        cur.push(seg);
        let md = match std::fs::symlink_metadata(&cur) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WtCheck::Gone,
            Err(e) => return WtCheck::Replaced(format!("cannot be inspected ({e})")),
        };
        if md.file_type().is_symlink() {
            return WtCheck::Replaced(format!("{} is a symbolic link", segs[..=i].join("/")));
        }
        if !md.is_dir() {
            return WtCheck::Replaced(format!("{} is not a directory", segs[..=i].join("/")));
        }
        if i + 1 == segs.len() {
            return WtCheck::Ok { dir: cur, identity: (md.dev(), md.ino()) };
        }
    }
    WtCheck::Gone
}

/// [`inspect`] 之后再核对 `(dev, ino)` 等于创建时记下的。
pub fn verify(project_dir: &Path, wt: &WorktreeRow) -> WtCheck {
    match inspect(project_dir, &wt.rel_path) {
        WtCheck::Ok { dir, identity } => match wt.identity {
            Some(rec) if rec == identity => WtCheck::Ok { dir, identity },
            Some(_) => WtCheck::Replaced("the directory is not the one anycon created".into()),
            None => WtCheck::Replaced("anycon has no record of creating this directory".into()),
        },
        other => other,
    }
}

/// worktree 根下 `.git` 指针文件的原始内容（`gitdir: <主仓库>/.git/worktrees/<名字>`）。读的时候不跟随符号链接、只认普通文件
/// （`.git` 被换成目录 / 链接 / 管道都读不出来）。
fn read_git_file(wt_dir: &Path) -> Option<String> {
    use std::io::Read;
    let f = crate::fsopen::open_file_in(&wt_dir.join(".git"), wt_dir).ok()?;
    let mut text = String::new();
    f.take(4096).read_to_string(&mut text).ok()?;
    Some(text.trim_end().to_string())
}

/// 创建 / 重建时记下的指针：原始内容里项目目录的绝对路径换成 `{project}`，项目改名之后记录仍然有意义。
pub fn read_git_pointer(project_dir: &Path, wt_dir: &Path) -> Option<String> {
    let project = project_dir.to_string_lossy();
    Some(read_git_file(wt_dir)?.replace(project.as_ref(), "{project}"))
}

/// 指针内容 `gitdir: <路径>` 指向哪里：绝对路径原样，相对路径（git 的 `worktree.useRelativePaths`）按 worktree 目录解析；
/// 词法规范化（去掉 `.` / `..`）。解析不了返回 `None`。
fn pointer_target(content: &str, wt_dir: &Path) -> Option<PathBuf> {
    let v = content.trim().strip_prefix("gitdir:")?.trim();
    if v.is_empty() || v.contains('\0') {
        return None;
    }
    let p = Path::new(v);
    let joined = if p.is_absolute() { p.to_path_buf() } else { wt_dir.join(p) };
    Some(crate::linkrules::lexical_clean(&joined))
}

/// 指针按**语义**比较：记下的那份（`{project}` 换回现在的项目目录）和现在文件里的，都解析成
/// `<git common dir>/worktrees/<管理目录名>` 的绝对路径再比较——绝对 / 相对两种写法都接受；agent 把它改到别处仍然拦得住。
fn pointer_matches(recorded: &str, project_dir: &Path, wt_dir: &Path) -> Option<bool> {
    let expected = pointer_target(&recorded.replace("{project}", &project_dir.to_string_lossy()), wt_dir)?;
    let now = pointer_target(&read_git_file(wt_dir)?, wt_dir)?;
    Some(expected == now)
}

/// [`verify`]，再核对 `.git` 指针文件仍指向创建时的那个管理目录（agent 的 cwd 就是 worktree，它能改这个文件：改成指向 cwd 里
/// 自建的 gitdir 之后，之后在这个目录里跑的 git——包括 daemon 自己的 `git status`——会读到 agent 写的配置）。
/// 没记过指针的旧记录不核对。**daemon 在 worktree 目录里跑 git 之前都用它**，不是只在拉起 / 开 turn 时。
/// 指针指向别处 → `PointerChanged`；`.git` 读不出来或解析不了（被换成目录 / 链接 / 乱写）→ `Replaced`。
pub fn verify_full(project_dir: &Path, wt: &WorktreeRow) -> WtCheck {
    match verify(project_dir, wt) {
        WtCheck::Ok { dir, identity } => {
            if let Some(rec) = &wt.git_pointer {
                match pointer_matches(rec, project_dir, &dir) {
                    Some(true) => {}
                    Some(false) => return WtCheck::PointerChanged,
                    None => return WtCheck::Replaced("the .git file inside it is missing or unreadable".into()),
                }
            }
            WtCheck::Ok { dir, identity }
        }
        other => other,
    }
}

// ---------------------------------------------------------------- base 的记法

/// 40 / 64 位十六进制（提交 SHA-1 / SHA-256）。
pub fn is_hex_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 已经是规范记法：完整引用名（`refs/heads/…`、`refs/remotes/…`、`refs/tags/…`）或提交 SHA。
pub fn is_normalized_base(b: &str) -> bool {
    b.starts_with("refs/heads/") || b.starts_with("refs/remotes/") || b.starts_with("refs/tags/") || is_hex_oid(b)
}

/// 展示名（`base_display`）：引用去掉前缀（`main`、`origin/main`、`v1.0`），提交 SHA 取前 12 位，别的原样。
/// 新记录创建时另存；旧记录（迁移 0006 之前）读出来时按现有的 `base` 推。
pub fn display_base(b: &str) -> String {
    for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
        if let Some(rest) = b.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    if is_hex_oid(b) {
        return b[..12].to_string();
    }
    b.to_string()
}

/// 拿 `wt.base` 去问 git 时用的写法。新记录是完整引用名或 SHA，原样用；**旧记录**（规范化之前创建的短名字）：
/// 像分支名的原样用（`refs/heads` / `refs/remotes` 在所有 worktree 里是同一份），`HEAD`、`HEAD~1`、`@{u}`、`ORIG_HEAD`…
/// 这类在 worktree 里会指向 worktree 自己的东西的写法改用创建时的提交。
pub fn base_spec(wt: &WorktreeRow) -> String {
    let b = wt.base.as_str();
    if is_normalized_base(b) {
        return b.to_string();
    }
    let pseudo = b == "HEAD" || b == "@" || b.ends_with("_HEAD") || b == "AUTO_MERGE";
    let expr = b.contains(['~', '^', '@', ':', '?', '*', '[', '\\', ' ']) || b.contains("..");
    if pseudo || expr {
        wt.base_sha.clone()
    } else {
        b.to_string()
    }
}

// ---------------------------------------------------------------- 创建

/// 分配分支名和目录名需要的输入。
pub struct PlanInput<'a> {
    pub session_id: &'a str,
    /// 对外的项目名（`Workspace::canonical_project_name`）。
    pub project: &'a str,
    pub project_loc: Option<String>,
    pub project_dir: &'a Path,
    pub base: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// 第一条消息，生成 slug 用。
    pub prompt: Option<&'a str>,
}

/// 创建成功的结果。
#[derive(Debug, Clone)]
pub struct Created {
    pub identity: (u64, u64),
    /// 建好时 worktree 根下 `.git` 指针文件的内容（见 [`read_git_pointer`]）。
    pub git_pointer: Option<String>,
    /// 从 `.worktreeinclude` 复制的文件数。
    pub copied: u32,
    pub note: Option<String>,
}

/// 处置 worktree 的参数。
#[derive(Debug, Clone, Copy)]
pub struct DisposeOpts {
    pub disposal: WorktreeDisposal,
    /// 删除 worktree 之后再删分支（`git branch -D`）。
    pub delete_branch: bool,
    /// `Remove` 时是否带 `--force`（归档 / 删除总是 true；`workspace.remove_worktree` 看 `force`）。
    pub force_remove: bool,
}

/// 处置结果。
#[derive(Debug, Clone)]
pub struct DisposeOutcome {
    /// 处置之后的状态：`Kept` 或 `Removed`。
    pub state: WorktreeState,
    /// 这次真的删了目录。
    pub removed_dir: bool,
    pub branch_deleted: bool,
    /// 处置之后分支还在不在。
    pub branch_exists: bool,
    /// 保留或没删干净的原因（事件的 `note`）。
    pub note: Option<String>,
    /// 想删而没删成（`git worktree remove` 报错）。
    pub error: Option<String>,
    /// 只改了库：磁盘和分支一概没碰（`failed` 的记录，或者和别的记录重名的创建残局）。
    pub record_only: bool,
}

#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct TestHooks {
    /// `git worktree add` 成功之后让创建失败（测清理）。
    pub fail_after_add: std::sync::atomic::AtomicBool,
    /// `git worktree add` 成功之后停多久（毫秒）：测「建 worktree 期间 agent 的启动超时不算」。
    pub delay_after_add_ms: std::sync::atomic::AtomicU64,
    /// `session.sync_base` 标忙之后先停多久（毫秒）：测同步期间 `send` / 归档 / 再同步都是 `busy`。
    pub sync_delay_ms: std::sync::atomic::AtomicU64,
    /// `session.diff` 列表给未跟踪文件数行数的字节预算（0 = 缺省 64 MiB）。
    pub diff_count_max_bytes: std::sync::atomic::AtomicU64,
    /// 同上的时间预算（毫秒，0 = 缺省 2 秒）。
    pub diff_count_budget_ms: std::sync::atomic::AtomicU64,
    /// `session.diff` 为数行数读过多少个未跟踪文件（累计）。
    pub untracked_reads: std::sync::atomic::AtomicU64,
}

pub struct WorktreeMgr {
    git: Git,
    store: Store,
    workspace: Arc<Workspace>,
    config: Arc<ConfigStore>,
    /// 串行化「分配分支名 / 目录名并登记」：两个并发创建不会拿到同一个名字。
    alloc: Arc<AsyncMutex<()>>,
    version: Option<String>,
    #[cfg(any(test, feature = "testing"))]
    pub hooks: TestHooks,
}

impl WorktreeMgr {
    /// 启动时跑一次 `git --version`（daemon 环境）：找不到 `git` 时 `available()` 为 false，`NodeCapabilities.worktree` 随之为 false。
    pub async fn new(git: Git, store: Store, workspace: Arc<Workspace>, config: Arc<ConfigStore>) -> Arc<WorktreeMgr> {
        let version = match git.at(Path::new("/")).args(["--version"]).secs(10).run().await {
            Ok(o) if o.success() => Some(o.text()),
            Ok(o) => {
                tracing::warn!(stderr = %o.err_tail(200), "`git --version` failed; per-session worktrees are unavailable");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "git is not available; per-session worktrees are unavailable");
                None
            }
        };
        Arc::new(WorktreeMgr {
            git,
            store,
            workspace,
            config,
            alloc: Arc::new(AsyncMutex::new(())),
            version,
            #[cfg(any(test, feature = "testing"))]
            hooks: TestHooks::default(),
        })
    }

    /// `session.diff` 列表给未跟踪文件数行数的预算：(累计字节, 时间)。
    pub fn count_budget(&self) -> (u64, Duration) {
        #[allow(unused_mut)]
        let (mut bytes, mut time) = (crate::sessions::COUNT_BYTES_MAX, crate::sessions::COUNT_TIME_MAX);
        #[cfg(any(test, feature = "testing"))]
        {
            use std::sync::atomic::Ordering::SeqCst;
            let (b, ms) = (self.hooks.diff_count_max_bytes.load(SeqCst), self.hooks.diff_count_budget_ms.load(SeqCst));
            if b > 0 {
                bytes = b;
            }
            if ms > 0 {
                time = Duration::from_millis(ms);
            }
        }
        (bytes, time)
    }

    /// 测试用计数：`session.diff` 为数行数读了几个未跟踪文件。
    pub fn note_untracked_reads(&self, _n: u64) {
        #[cfg(any(test, feature = "testing"))]
        self.hooks.untracked_reads.fetch_add(_n, std::sync::atomic::Ordering::SeqCst);
    }

    /// 找得到 `git`。
    pub fn available(&self) -> bool {
        self.version.is_some()
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    // ------------------------------------------------------------ 小工具

    async fn git_text(&self, dir: &Path, args: &[&str]) -> Option<String> {
        match self.git.at(dir).args(args).secs(15).run().await {
            Ok(o) if o.success() => Some(o.text()),
            _ => None,
        }
    }

    async fn git_ok(&self, dir: &Path, args: &[&str], secs: u64) -> Result<GitOut, String> {
        match self.git.at(dir).args(args).secs(secs).run().await {
            Ok(o) => Ok(o),
            Err(e) => Err(e.to_string()),
        }
    }

    /// `rev-parse --verify --end-of-options <spec>^{commit}`。
    pub async fn resolve_commit(&self, dir: &Path, spec: &str) -> Option<String> {
        let target = format!("{spec}^{{commit}}");
        self.git_text(dir, &["rev-parse", "--verify", "--quiet", "--end-of-options", &target]).await
    }

    /// `refs/heads/<branch>` 现在指向的提交。
    pub async fn branch_tip(&self, dir: &Path, branch: &str) -> Option<String> {
        self.git_text(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}^{{commit}}")]).await
    }

    pub async fn branch_exists(&self, dir: &Path, branch: &str) -> bool {
        self.branch_tip(dir, branch).await.is_some()
    }

    /// 分支名占用：它自己存在、它下面有别的分支（`a` 对 `a/b`）、它的某个上级是分支（`a/b` 对 `a`）。
    async fn ref_conflict(&self, dir: &Path, branch: &str) -> bool {
        if self.branch_exists(dir, branch).await {
            return true;
        }
        let children = format!("refs/heads/{branch}/");
        if let Some(t) = self.git_text(dir, &["for-each-ref", "--count=1", "--format=%(refname)", &children]).await {
            if !t.is_empty() {
                return true;
            }
        }
        let segs: Vec<&str> = branch.split('/').collect();
        for n in 1..segs.len() {
            if self.branch_exists(dir, &segs[..n].join("/")).await {
                return true;
            }
        }
        false
    }

    /// 项目当前检出的分支（detached 为 `None`）。
    pub async fn current_branch(&self, dir: &Path) -> Option<String> {
        self.git_text(dir, &["symbolic-ref", "--short", "-q", "HEAD"]).await.filter(|s| !s.is_empty())
    }

    /// 新会话默认的 base：当前分支有（存在的）上游就是上游，否则是当前分支；detached 是 HEAD 的提交。
    pub async fn default_base(&self, dir: &Path) -> Option<String> {
        match self.current_branch(dir).await {
            Some(cur) => {
                let refname = format!("refs/heads/{cur}");
                let up_full = self.git_text(dir, &["for-each-ref", "--format=%(upstream)", &refname]).await;
                if let Some(full) = up_full.filter(|s| !s.is_empty()) {
                    if self.git_text(dir, &["rev-parse", "--verify", "--quiet", &full]).await.is_some() {
                        if let Some(short) =
                            self.git_text(dir, &["for-each-ref", "--format=%(upstream:short)", &refname]).await
                        {
                            if !short.is_empty() {
                                return Some(short);
                            }
                        }
                    }
                }
                Some(cur)
            }
            None => self.git_text(dir, &["rev-parse", "HEAD"]).await,
        }
    }

    /// 校验一个显式给的分支名的写法：文本规则、`check-ref-format --branch` 通过且原样返回（`@{-1}` 之类会被展开，不收）。
    async fn check_branch_name(&self, dir: &Path, branch: &str) -> RpcResult<()> {
        if !check_ref_text(branch) {
            return Err(err(ErrorCode::Invalid, "invalid branch name"));
        }
        let out = self.git_ok(dir, &["check-ref-format", "--branch", branch], 10).await;
        let valid = matches!(&out, Ok(o) if o.success() && o.text() == branch);
        if !valid {
            return Err(err(ErrorCode::Invalid, format!("{branch:?} is not a valid git branch name")));
        }
        Ok(())
    }

    /// base 的规范记法（daemon.md § 10.1「创建」）：`input` 是本地分支（优先）、远端跟踪分支或标签的写法，记完整引用名
    /// （`refs/heads/main`、`refs/remotes/origin/main`、`refs/tags/v1.0`）；其他写法（`HEAD`、`HEAD~1`、`@{u}`、SHA、
    /// `main@{yesterday}`）只记解析出的提交 `sha`。以前原样记下，之后在 worktree 里重新解析：`HEAD` 变成 worktree 自己的 HEAD，
    /// 已提交的改动从「相对 base」里消失、同步永远「已是最新」。
    pub async fn normalize_base(&self, dir: &Path, input: &str, sha: &str) -> String {
        let mut candidates: Vec<String> = Vec::new();
        if input.starts_with("refs/heads/") || input.starts_with("refs/remotes/") || input.starts_with("refs/tags/") {
            candidates.push(input.to_string());
        } else {
            // 本地分支优先，其次远端跟踪分支，最后标签
            candidates.push(format!("refs/heads/{input}"));
            candidates.push(format!("refs/remotes/{input}"));
            candidates.push(format!("refs/tags/{input}"));
        }
        for full in candidates {
            // 只认「完整引用名的写法合法」的（拒绝 `@{`、`~`、`..` 之类的表达式）并且这个引用确实存在
            let valid = matches!(self.git_ok(dir, &["check-ref-format", &full], 10).await, Ok(o) if o.success());
            if valid && self.branch_ref_commit(dir, &full).await.is_some() {
                return full;
            }
        }
        sha.to_string()
    }

    async fn branch_ref_commit(&self, dir: &Path, full_ref: &str) -> Option<String> {
        self.git_text(dir, &["rev-parse", "--verify", "--quiet", &format!("{full_ref}^{{commit}}")]).await
    }

    /// 旧记录（规范化之前创建的）的 base 能规范成什么：已经规范的返回 `None`；`HEAD` 类写法变成创建时的提交，像分支名的
    /// 按现在的仓库变成完整引用名（找不到就用创建时的提交）。
    pub async fn normalized_base_of(&self, project_dir: &Path, wt: &WorktreeRow) -> Option<String> {
        if is_normalized_base(&wt.base) {
            return None;
        }
        let spec = base_spec(wt);
        if spec == wt.base_sha {
            return Some(wt.base_sha.clone());
        }
        Some(self.normalize_base(project_dir, &spec, &wt.base_sha).await)
    }

    // ------------------------------------------------------------ 创建前：校验并分配名字

    /// `session.create { workspace: worktree }` 的同步校验和名字分配（daemon.md § 10.1「创建」第 1 步）。
    /// 返回 `state = creating` 的 worktree 行（还没落库）和分配锁的守卫——调用方把行落库之后再放掉守卫，
    /// 这样并发的创建看得到彼此的名字。
    pub async fn plan(&self, i: PlanInput<'_>) -> RpcResult<(WorktreeRow, OwnedMutexGuard<()>)> {
        if !self.available() {
            return Err(err(ErrorCode::Unsupported, "git is not available on this node"));
        }
        let dir = i.project_dir;
        if !worktree_ready_fresh(self.git.env(), dir).await {
            return Err(err_reason(
                ErrorCode::Invalid,
                "this project cannot have per-session worktrees: its directory must be the top level of a git repository with at least one commit",
                "not_worktree_ready",
            ));
        }
        let cfg = self.config.get();
        // base
        let base = match i.base.filter(|b| !b.is_empty()) {
            Some(b) => {
                if !check_ref_text(b) {
                    return Err(err_reason(ErrorCode::Invalid, "invalid base", "bad_base"));
                }
                b.to_string()
            }
            None => self
                .default_base(dir)
                .await
                .ok_or_else(|| err_reason(ErrorCode::Invalid, "cannot determine a default base", "bad_base"))?,
        };
        let base_sha = self.resolve_commit(dir, &base).await.ok_or_else(|| {
            err_reason(
                ErrorCode::Invalid,
                format!("base {base:?} does not name a commit in this repository"),
                "bad_base",
            )
        })?;
        // 规范化：分支写法记完整引用名，其他写法（`HEAD`、`HEAD~1`、`@{u}`、标签、SHA）只记提交
        let base = self.normalize_base(dir, &base, &base_sha).await;
        // 名字：分配锁里做，调用方落库之后才放
        let guard = self.alloc.clone().lock_owned().await;
        let rows: Vec<WorktreeRow> = self
            .store
            .list_worktrees()
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("cannot read the worktree records: {e}")))?
            .into_iter()
            // `failed` 的记录继续占着它的分支名和目录名：失败当时已经清理干净了，但释放名字之后新会话会拿到
            // 同一个，之后处置那条 failed 的记录就可能碰到别人的东西
            .filter(|r| self.workspace.same_project_name(&r.project, i.project))
            .collect();
        let dir_taken = |name: &str| {
            let rel = format!("{WORKTREES_REL}/{name}");
            rows.iter().any(|r| r.rel_path == rel)
                || std::fs::symlink_metadata(dir.join(&rel)).is_ok()
                || std::fs::symlink_metadata(dir.join(".anycon")).is_ok_and(|m| !m.is_dir())
        };
        let prefix = cfg.worktree.branch_prefix.clone();
        let with_prefix = |slug: &str| if prefix.is_empty() { slug.to_string() } else { format!("{prefix}/{slug}") };
        let (branch, dir_name) = match i.branch.filter(|b| !b.is_empty()) {
            Some(b) => {
                self.check_branch_name(dir, b).await?;
                if self.ref_conflict(dir, b).await {
                    return Err(err(ErrorCode::Exists, format!("branch {b} already exists")));
                }
                if let Some(r) = rows.iter().find(|r| r.branch == b) {
                    // 名字被创建失败的会话占着（git 里并没有这个分支）：告诉用户怎么办，别说「已存在」
                    return Err(match (r.state, &r.session_id) {
                        (WorktreeState::Failed, Some(sid)) => err(
                            ErrorCode::Exists,
                            format!(
                                "the branch name {b} is reserved by session {sid}, whose worktree could not be created: send that session a message to retry with this name, or delete it to free the name"
                            ),
                        ),
                        _ => err(ErrorCode::Exists, format!("branch {b} already exists")),
                    });
                }
                if matches!(b, "main" | "master" | "HEAD") {
                    return Err(err(ErrorCode::Invalid, format!("{b} is a reserved branch name")));
                }
                (b.to_string(), dedupe_name(&dir_name_for_branch(b), dir_taken))
            }
            None => {
                let slug = i.prompt.and_then(slug_from_prompt).unwrap_or_else(|| random_slug(&mut rand::thread_rng()));
                let mut chosen = None;
                for n in 1..200u32 {
                    let s = if n == 1 { slug.clone() } else { format!("{slug}-{n}") };
                    let b = with_prefix(&s);
                    let reserved = matches!(b.as_str(), "main" | "master" | "HEAD");
                    let name = dir_name_for_branch(&b);
                    if reserved
                        || dir_taken(&name)
                        || rows.iter().any(|r| r.branch == b)
                        || self.ref_conflict(dir, &b).await
                    {
                        continue;
                    }
                    chosen = Some((b, name));
                    break;
                }
                match chosen {
                    Some(c) => c,
                    None => {
                        let s = format!("{slug}-{:06x}", rand::thread_rng().gen_range(0..0xff_ffff));
                        let b = with_prefix(&s);
                        let name = dir_name_for_branch(&b);
                        (b, name)
                    }
                }
            }
        };
        let row = WorktreeRow {
            id: new_worktree_id(),
            project_loc: i.project_loc,
            project: i.project.to_string(),
            rel_path: format!("{WORKTREES_REL}/{dir_name}"),
            branch,
            base_display: display_base(&base),
            base,
            base_sha,
            session_id: Some(i.session_id.to_string()),
            state: WorktreeState::Creating,
            identity: None,
            git_pointer: None,
            created_ms: now_ms(),
        };
        Ok((row, guard))
    }

    // ------------------------------------------------------------ 创建 / 重建（磁盘）

    /// `<项目>/.anycon` 和 `<项目>/.anycon/worktrees`：不存在就建，存在时必须是真目录（不是符号链接、不是文件）。
    fn ensure_dirs(project_dir: &Path) -> Result<(), String> {
        let mut cur = project_dir.to_path_buf();
        for seg in [".anycon", "worktrees"] {
            cur.push(seg);
            match std::fs::symlink_metadata(&cur) {
                Ok(md) if md.file_type().is_symlink() => {
                    return Err(format!(
                        "{} is a symbolic link; refusing to create worktrees through it",
                        cur.display()
                    ))
                }
                Ok(md) if !md.is_dir() => return Err(format!("{} exists and is not a directory", cur.display())),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&cur).map_err(|e| format!("cannot create {}: {e}", cur.display()))?;
                }
                Err(e) => return Err(format!("cannot inspect {}: {e}", cur.display())),
            }
        }
        Ok(())
    }

    /// `<git common dir>/info/exclude` 里没有 `/.anycon/` 就追加一行（文件不存在就建）。不改仓库里的 `.gitignore`。
    async fn ensure_exclude(&self, project_dir: &Path) -> Result<(), String> {
        let out = self.git_ok(project_dir, &["rev-parse", "--git-common-dir"], 10).await?;
        if !out.success() {
            return Err(format!("git rev-parse --git-common-dir failed: {}", out.err_tail(300)));
        }
        let p = out.text();
        let common = if Path::new(&p).is_absolute() { PathBuf::from(&p) } else { project_dir.join(&p) };
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let info = common.join("info");
            match std::fs::symlink_metadata(&info) {
                Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {}
                Ok(_) => return Err(format!("{} is not a directory", info.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&info).map_err(|e| format!("cannot create {}: {e}", info.display()))?;
                }
                Err(e) => return Err(format!("cannot inspect {}: {e}", info.display())),
            }
            let file = info.join("exclude");
            let existing = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&file) {
                Ok(mut f) => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut f, &mut s)
                        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
                    s
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
            };
            if existing.lines().any(|l| l.trim() == EXCLUDE_LINE) {
                return Ok(());
            }
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&file)
                .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            let lead = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
            f.write_all(format!("{lead}{EXCLUDE_LINE}\n").as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", file.display()))
        })
        .await
        .map_err(|e| format!("exclude task failed: {e}"))?
    }

    /// `git worktree list --porcelain` 里有没有**恰好这个路径**。不 `canonicalize`、路径本身是符号链接就不认：
    /// 否则一个指向别的 worktree 的符号链接会让清理顺着它删掉那个 worktree（`git worktree remove` 会按链接解析）。
    async fn is_registered(&self, project_dir: &Path, abs: &Path) -> bool {
        if std::fs::symlink_metadata(abs).is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
        let Some(text) = self.git_text(project_dir, &["worktree", "list", "--porcelain"]).await else { return false };
        text.lines().filter_map(|l| l.strip_prefix("worktree ")).any(|p| Path::new(p) == abs)
    }

    /// 别的记录（同项目、`id` 不同、不是 `failed`）用着同一个路径（且没被删掉）或同一个分支：这条记录的清理不能碰磁盘和分支。
    /// 读不了表就当作有（不动手）。
    async fn shares_name_with_other_row(&self, wt: &WorktreeRow) -> bool {
        let Ok(rows) = self.store.list_worktrees().await else { return true };
        rows.iter().any(|r| {
            r.id != wt.id
                && r.state != WorktreeState::Failed
                && (self.workspace.same_project_name(&r.project, &wt.project)
                    || (r.project_loc.is_some() && r.project_loc == wt.project_loc))
                && (r.branch == wt.branch || (r.rel_path == wt.rel_path && r.state != WorktreeState::Removed))
        })
    }

    /// 创建 / 重建失败后的清理：登记过的 worktree 用 git 删（`git worktree remove --force`）；分支是我们这次建的才删；最后 `prune`。
    /// 不碰没有登记的目录。幂等（上次创建中途崩溃留下的残局也走这里）。
    ///
    /// 动手之前核对：别的记录没有用着同一个路径 / 分支（老数据里 `failed` 的记录释放过名字，同名的新会话可能正在用）、
    /// 路径上每一段都不是符号链接、git 登记的是恰好这个路径。
    pub async fn cleanup_attempt(&self, project_dir: &Path, wt: &WorktreeRow, delete_branch: bool) {
        if self.shares_name_with_other_row(wt).await {
            tracing::warn!(worktree = %wt.id, path = %wt.rel_path, branch = %wt.branch, "another worktree record uses the same path or branch; the leftovers of this one are not touched");
            return;
        }
        let abs = project_dir.join(&wt.rel_path);
        if rel_path_ok(&wt.rel_path)
            && !matches!(inspect(project_dir, &wt.rel_path), WtCheck::Replaced(_))
            && self.is_registered(project_dir, &abs).await
        {
            if let Some(path) = abs.to_str() {
                let _ = self.git.at(project_dir).args(["worktree", "remove", "--force", path]).secs(120).run().await;
            }
        }
        if delete_branch {
            // 只删还指在 base 上的分支（没有任何提交），不会删掉有内容的
            if self.branch_tip(project_dir, &wt.branch).await.as_deref() == Some(wt.base_sha.as_str()) {
                let _ = self.git.at(project_dir).args(["branch", "-D", &wt.branch]).secs(30).run().await;
            }
        }
        let _ = self.git.at(project_dir).args(["worktree", "prune"]).secs(30).run().await;
    }

    /// 在磁盘上建出（或重建）worktree。`rebuild = false`：新分支 `-b <branch>` 从 `base_sha` 开，建好后复制 `.worktreeinclude`（两种都复制）；
    /// `rebuild = true`：分支还在就 `git worktree add <path> <branch>`，不在就 `-b <branch> <path> <base_sha>`，同样复制 `.worktreeinclude`。
    /// 失败时已经清理干净，返回英文原因。
    pub async fn create_on_disk(&self, project_dir: &Path, wt: &WorktreeRow, rebuild: bool) -> Result<Created, String> {
        if !rel_path_ok(&wt.rel_path) {
            return Err("invalid worktree path".into());
        }
        let pd = project_dir.to_path_buf();
        tokio::task::spawn_blocking(move || Self::ensure_dirs(&pd))
            .await
            .map_err(|e| format!("mkdir task failed: {e}"))??;
        self.ensure_exclude(project_dir).await?;
        let abs = project_dir.join(&wt.rel_path);
        let abs_str = abs.to_str().ok_or("the worktree path is not valid UTF-8")?.to_string();
        // 上次中途崩溃留下的残局（登记过但没记完成的）先清掉；没登记的目录不碰，下面的 add 会因为路径已存在而失败
        if wt.identity.is_none() {
            self.cleanup_attempt(project_dir, wt, false).await;
        } else {
            let _ = self.git.at(project_dir).args(["worktree", "prune"]).secs(30).run().await;
        }
        if std::fs::symlink_metadata(&abs).is_ok() {
            return Err(format!("{} already exists", wt.rel_path));
        }
        let tip = self.branch_tip(project_dir, &wt.branch).await;
        let branch_existed = tip.is_some();
        // 分支已经在：重建总是复用它；新建时只有「还指在 base 上」才复用（上次创建中途崩溃留下的），否则是别人的分支
        let reuse_branch = branch_existed && (rebuild || tip.as_deref() == Some(wt.base_sha.as_str()));
        if branch_existed && !reuse_branch {
            return Err(format!("branch {} already exists", wt.branch));
        }
        let mut args: Vec<&str> = vec!["worktree", "add"];
        if reuse_branch {
            args.extend([abs_str.as_str(), wt.branch.as_str()]);
        } else {
            // 新分支不跟踪 base（`--no-track` 只能和 `-b` 一起用）
            args.extend(["--no-track", "-b", wt.branch.as_str(), abs_str.as_str(), wt.base_sha.as_str()]);
        }
        let added = self.git.at(project_dir).args(args).timeout(ADD_TIMEOUT).run().await;
        let fail = |msg: String| async move {
            self.cleanup_attempt(project_dir, wt, !branch_existed).await;
            Err::<Created, String>(msg)
        };
        match added {
            Err(e) => return fail(format!("git worktree add: {e}")).await,
            Ok(o) if !o.success() => return fail(format!("git worktree add failed: {}", o.failure_text(1500))).await,
            Ok(_) => {}
        }
        #[cfg(any(test, feature = "testing"))]
        {
            let ms = self.hooks.delay_after_add_ms.load(std::sync::atomic::Ordering::SeqCst);
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
        #[cfg(any(test, feature = "testing"))]
        if self.hooks.fail_after_add.load(std::sync::atomic::Ordering::SeqCst) {
            return fail("injected failure after git worktree add".into()).await;
        }
        let identity = match inspect(project_dir, &wt.rel_path) {
            WtCheck::Ok { identity, .. } => identity,
            WtCheck::Gone => return fail("the worktree directory is missing right after git created it".into()).await,
            WtCheck::Replaced(why) => return fail(format!("the worktree directory is not usable: {why}")).await,
            WtCheck::PointerChanged => return fail("the worktree directory is not usable".into()).await,
        };
        // 重建也复制 `.worktreeinclude`（被忽略的 `.env` 之类不算未提交，`auto` 归档会把干净的 worktree 连它们一起删掉）
        let cfg = self.config.get();
        let (copied, note) = self
            .copy_includes(project_dir, &abs, cfg.worktree.include_max_files, cfg.worktree_include_max_bytes())
            .await;
        let git_pointer = read_git_pointer(project_dir, &abs);
        Ok(Created { identity, git_pointer, copied, note })
    }

    /// 复制 `.worktreeinclude` 列出的未跟踪文件（`git ls-files -z --others --ignored --exclude-from=<文件>`，在项目目录跑）。
    /// 只复制普通文件（源端逐段 `openat` + `O_NOFOLLOW`，符号链接和别的类型跳过），目标路径的每一段都不能是符号链接，
    /// 保留权限位；累计超过上限就停下。失败不让创建失败：返回已复制的数量和说明。
    async fn copy_includes(
        &self,
        project_dir: &Path,
        wt_dir: &Path,
        max_files: u64,
        max_bytes: u64,
    ) -> (u32, Option<String>) {
        let include_file = project_dir.join(".worktreeinclude");
        match std::fs::symlink_metadata(&include_file) {
            Ok(md) if md.is_file() => {}
            Ok(_) => return (0, Some(".worktreeinclude is not a regular file; skipped".into())),
            Err(_) => return (0, None),
        }
        let Some(path_str) = include_file.to_str() else { return (0, None) };
        let exclude_from = format!("--exclude-from={path_str}");
        let listed = self
            .git
            .at(project_dir)
            .args(["ls-files", "-z", "--others", "--ignored", &exclude_from])
            .secs(60)
            .cap(32 * 1024 * 1024)
            .run()
            .await;
        let list = match listed {
            Ok(o) if o.success() => o.stdout,
            Ok(o) if o.truncated => {
                return (0, Some(".worktreeinclude matches too many files; nothing was copied".into()))
            }
            Ok(o) => return (0, Some(format!("git ls-files failed: {}", o.err_tail(300)))),
            Err(e) => return (0, Some(format!("git ls-files failed: {e}"))),
        };
        let (src_root, dst_root) = (project_dir.to_path_buf(), wt_dir.to_path_buf());
        tokio::task::spawn_blocking(move || copy_listed_files(&src_root, &dst_root, &list, max_files, max_bytes))
            .await
            .unwrap_or((0, Some("the copy task failed".into())))
    }

    // ------------------------------------------------------------ 状态

    /// worktree / 目录里 `git status` 的摘要；git 失败返回 `None`。
    pub async fn status_in(&self, dir: &Path) -> Option<StatusV2> {
        let out = self
            .git
            .at(dir)
            .args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"])
            .secs(15)
            .run()
            .await
            .ok()?;
        (out.success() || out.truncated).then(|| parse_status_v2(&out.stdout))
    }

    /// 未提交的文件数（含未跟踪）。git 失败返回 `None`。
    pub async fn uncommitted_in(&self, dir: &Path) -> Option<u32> {
        self.status_in(dir).await.map(|s| s.uncommitted)
    }

    /// 分支上 `base_sha` 之后自己的提交数。
    pub async fn own_commits(&self, project_dir: &Path, wt: &WorktreeRow) -> Option<u32> {
        let range = format!("{}..refs/heads/{}", wt.base_sha, wt.branch);
        self.git_text(project_dir, &["rev-list", "--count", &range]).await?.parse().ok()
    }

    /// 分支配了上游。
    pub async fn has_upstream(&self, project_dir: &Path, branch: &str) -> bool {
        let refname = format!("refs/heads/{branch}");
        self.git_text(project_dir, &["for-each-ref", "--format=%(upstream)", &refname])
            .await
            .is_some_and(|s| !s.is_empty())
    }

    // ------------------------------------------------------------ 归档 / 删除时的处置

    /// 按 `opts` 处置一个 worktree（归档 / 删除 / `remove_worktree` 共用）。**不更新数据库**，由调用方根据结果写。
    ///
    /// - `Keep`：不动，`Kept`；
    /// - `Auto`：没有未提交改动（含未跟踪）才 `git worktree remove`，有（或查不出来）就 `Kept`；
    /// - `Remove`：`git worktree remove [--force]`；
    /// - 目录不在 / 被换掉了：不碰磁盘，`Removed`（`git worktree prune` 由 git 自己判断）；
    /// - 删完目录之后：`delete_branch`，或 `Auto` 且分支上没有自己的提交也没有上游，就 `git branch -D`。
    pub async fn dispose(&self, project_dir: &Path, wt: &WorktreeRow, opts: DisposeOpts) -> DisposeOutcome {
        let mut out = DisposeOutcome {
            state: WorktreeState::Removed,
            removed_dir: false,
            branch_deleted: false,
            branch_exists: true,
            note: None,
            error: None,
            record_only: false,
        };
        // `failed`：创建失败当时已经把目录、登记、分支清理干净了，这条记录之后只是占着名字——只改库，不碰磁盘和分支。
        // （老数据里 `failed` 的记录可能已经和一个同名的新会话撞上，按路径 / 分支去清理就会删到别人的 worktree）
        if wt.state == WorktreeState::Failed {
            out.record_only = true;
            out.branch_exists = false;
            return out;
        }
        let check = if wt.state == WorktreeState::Creating {
            // 创建中途留下的残局：先确认没有别的记录用着同一个路径 / 分支，再按登记过的 worktree 清掉
            if self.shares_name_with_other_row(wt).await {
                out.record_only = true;
                out.branch_exists = false;
                out.note = Some("another worktree record uses the same path or branch; left untouched".into());
                return out;
            }
            self.cleanup_attempt(project_dir, wt, true).await;
            WtCheck::Gone
        } else {
            verify_full(project_dir, wt)
        };
        match check {
            WtCheck::Ok { .. } if opts.disposal == WorktreeDisposal::Keep => {
                out.state = WorktreeState::Kept;
                out.branch_exists = self.branch_exists(project_dir, &wt.branch).await;
                return out;
            }
            WtCheck::Ok { dir, .. } => {
                let abs = project_dir.join(&wt.rel_path);
                let mut force = false;
                match opts.disposal {
                    WorktreeDisposal::Auto => match self.uncommitted_in(&dir).await {
                        Some(0) => {}
                        Some(n) => {
                            out.state = WorktreeState::Kept;
                            out.note = Some(format!("{n} uncommitted file(s)"));
                        }
                        None => {
                            out.state = WorktreeState::Kept;
                            out.note = Some("could not inspect the working tree".into());
                        }
                    },
                    _ => force = opts.force_remove,
                }
                if out.state == WorktreeState::Kept {
                    out.branch_exists = self.branch_exists(project_dir, &wt.branch).await;
                    return out;
                }
                let path = abs.to_string_lossy().into_owned();
                let mut args = vec!["worktree", "remove"];
                if force {
                    args.push("--force");
                }
                args.push(&path);
                // 删之前再核一次身份：从上面的检查到现在不应该有人换掉它
                if !matches!(verify_full(project_dir, wt), WtCheck::Ok { .. }) {
                    out.state = WorktreeState::Missing;
                    out.note = Some("the directory changed; the directory was not touched".into());
                    return self.after_untouched(project_dir, wt, opts, out).await;
                } else {
                    match self.git.at(project_dir).args(args).secs(120).run().await {
                        Ok(o) if o.success() => out.removed_dir = true,
                        Ok(o) => {
                            let msg = o.failure_text(600);
                            out.state = WorktreeState::Kept;
                            out.note = Some(format!("git worktree remove failed: {msg}"));
                            out.error = Some(msg);
                        }
                        Err(e) => {
                            out.state = WorktreeState::Kept;
                            out.note = Some(format!("git worktree remove failed: {e}"));
                            out.error = Some(e.to_string());
                        }
                    }
                }
            }
            WtCheck::Gone => {}
            // 目录「被换掉」（身份、链接、`.git` 指针不对）：磁盘一概不动，也**不能**说它被删了——记录保持 `missing`，
            // 目录和里面未提交的文件还在
            WtCheck::Replaced(why) => {
                out.state = WorktreeState::Missing;
                out.note = Some(format!("{why}; the directory was not touched"));
                return self.after_untouched(project_dir, wt, opts, out).await;
            }
            WtCheck::PointerChanged => {
                out.state = WorktreeState::Missing;
                out.note = Some(
                    "its .git file no longer points at the repository (run `git worktree repair` in the project directory); the directory was not touched"
                        .into(),
                );
                return self.after_untouched(project_dir, wt, opts, out).await;
            }
        }
        let _ = self.git.at(project_dir).args(["worktree", "prune"]).secs(30).run().await;
        if out.state == WorktreeState::Removed {
            let exists = self.branch_exists(project_dir, &wt.branch).await;
            let auto_delete = exists
                && opts.disposal == WorktreeDisposal::Auto
                && self.own_commits(project_dir, wt).await == Some(0)
                && !self.has_upstream(project_dir, &wt.branch).await;
            if exists && ((opts.delete_branch && opts.disposal != WorktreeDisposal::Keep) || auto_delete) {
                match self.git.at(project_dir).args(["branch", "-D", &wt.branch]).secs(30).run().await {
                    Ok(o) if o.success() => out.branch_deleted = true,
                    Ok(o) => {
                        let msg = format!("git branch -D failed: {}", o.failure_text(300));
                        out.note = Some(out.note.take().map_or(msg.clone(), |n| format!("{n}; {msg}")));
                        if opts.delete_branch {
                            out.error = Some(msg);
                        }
                    }
                    Err(e) => out.error = Some(e.to_string()),
                }
            }
            out.branch_exists =
                if out.branch_deleted { false } else { self.branch_exists(project_dir, &wt.branch).await };
        }
        out
    }

    /// 处置时发现目录「被换掉」、一概没碰之后的收尾：不 `prune`、不按 `auto` 顺手删分支；只有请求里明确带 `delete_branch`
    /// （且不是 `keep`）才 `git branch -D`——分支还检出在原来的目录里时 git 会拒绝，那就只是个说明。
    async fn after_untouched(
        &self,
        project_dir: &Path,
        wt: &WorktreeRow,
        opts: DisposeOpts,
        mut out: DisposeOutcome,
    ) -> DisposeOutcome {
        if opts.delete_branch
            && opts.disposal != WorktreeDisposal::Keep
            && self.branch_exists(project_dir, &wt.branch).await
        {
            match self.git.at(project_dir).args(["branch", "-D", &wt.branch]).secs(30).run().await {
                Ok(o) if o.success() => out.branch_deleted = true,
                Ok(o) => {
                    let msg = format!("git branch -D failed: {}", o.failure_text(300));
                    out.note = Some(out.note.take().map_or(msg.clone(), |n| format!("{n}; {msg}")));
                }
                Err(e) => out.note = Some(out.note.take().map_or(e.to_string(), |n| format!("{n}; {e}"))),
            }
        }
        out.branch_exists = !out.branch_deleted && self.branch_exists(project_dir, &wt.branch).await;
        out
    }

    // ------------------------------------------------------------ 项目层面

    /// `workspace.branches`：本地分支和远端跟踪分支各最多 200 个，按最近提交时间倒序；不列 `*/HEAD` 和
    /// `<branch_prefix>/` 开头的本地分支。
    pub async fn branches(&self, project_dir: &Path) -> RpcResult<WorkspaceBranchesResult> {
        if !project_dir.join(".git").exists() {
            return Err(err_reason(ErrorCode::Invalid, "the project is not a git repository", "not_git"));
        }
        let out = self
            .git
            .at(project_dir)
            .args(["for-each-ref", "--sort=-committerdate", "--format=%(refname)", "refs/heads", "refs/remotes"])
            .secs(15)
            .cap(8 * 1024 * 1024)
            .run()
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("git for-each-ref: {e}")))?;
        if !out.success() && !out.truncated {
            return Err(err_reason(
                ErrorCode::Invalid,
                format!("not a usable git repository: {}", out.err_tail(300)),
                "not_git",
            ));
        }
        let prefix = self.config.get().worktree.branch_prefix.clone();
        let own = (!prefix.is_empty()).then(|| format!("{prefix}/"));
        let (mut local, mut remote, mut truncated) = (Vec::new(), Vec::new(), out.truncated);
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(b) = line.strip_prefix("refs/heads/") {
                if own.as_deref().is_some_and(|p| b.starts_with(p)) {
                    continue;
                }
                if local.len() < BRANCHES_MAX {
                    local.push(b.to_string());
                } else {
                    truncated = true;
                }
            } else if let Some(b) = line.strip_prefix("refs/remotes/") {
                if b.ends_with("/HEAD") {
                    continue;
                }
                if remote.len() < BRANCHES_MAX {
                    remote.push(b.to_string());
                } else {
                    truncated = true;
                }
            }
        }
        let current = self.current_branch(project_dir).await;
        let default_base = self
            .default_base(project_dir)
            .await
            .ok_or_else(|| err_reason(ErrorCode::Invalid, "the repository has no commits", "no_commits"))?;
        Ok(WorkspaceBranchesResult { current, default_base, local, remote, truncated })
    }

    /// 项目改名 / 从回收站恢复之后：worktree 的 `.git` 文件和主仓库里的记录都是绝对路径，`git worktree repair <路径…>` 修一遍。
    pub async fn repair_project(&self, project: &str) {
        let Ok(rows) = self.store.list_worktrees().await else { return };
        let Ok(dir) = self.workspace.project_path(project) else { return };
        let paths: Vec<String> = rows
            .iter()
            .filter(|r| self.workspace.same_project_name(&r.project, project))
            .filter(|r| matches!(r.state, WorktreeState::Active | WorktreeState::Kept | WorktreeState::Missing))
            .filter(|r| rel_path_ok(&r.rel_path) && matches!(inspect(&dir, &r.rel_path), WtCheck::Ok { .. }))
            .map(|r| dir.join(&r.rel_path).to_string_lossy().into_owned())
            .collect();
        if paths.is_empty() {
            return;
        }
        let mut args = vec!["worktree".to_string(), "repair".to_string()];
        args.extend(paths);
        match self.git.at(&dir).args(args).secs(60).run().await {
            Ok(o) if o.success() => {}
            Ok(o) => tracing::warn!(project, stderr = %o.err_tail(300), "git worktree repair failed"),
            Err(e) => tracing::warn!(project, error = %e, "git worktree repair failed"),
        }
    }

    /// 重建时原来的路径被占用：给这个分支换一个目录名（不和磁盘上已有的、别的行用着的重名）。
    pub async fn fresh_rel_path(&self, project_dir: &Path, project: &str, branch: &str, own_id: &str) -> String {
        let _guard = self.alloc.lock().await;
        let rows = self.store.list_worktrees().await.unwrap_or_default();
        let taken = |name: &str| {
            let rel = format!("{WORKTREES_REL}/{name}");
            std::fs::symlink_metadata(project_dir.join(&rel)).is_ok()
                || rows.iter().any(|r| {
                    r.id != own_id && self.workspace.same_project_name(&r.project, project) && r.rel_path == rel
                })
        };
        format!("{WORKTREES_REL}/{}", dedupe_name(&dir_name_for_branch(branch), taken))
    }

    /// 一个目录刚被（重新）关联为项目 `name`（`loc` = `linked:<目录>`）：在同一个目录里创建、而旧项目名现在已经不指向那里的
    /// worktree 行跟着改成新名字（和 `Sessions::adopt_orphans` 对会话做的一样）。返回改了哪些行。
    pub async fn adopt_rows(&self, loc: &str, name: &str) -> Vec<WorktreeRow> {
        let Ok(rows) = self.store.list_worktrees().await else { return Vec::new() };
        let mut changed = Vec::new();
        for mut r in rows {
            if r.project_loc.as_deref() != Some(loc) || self.workspace.same_project_name(&r.project, name) {
                continue;
            }
            if self.workspace.project_loc(&r.project).is_ok_and(|l| l == loc) {
                continue;
            }
            r.project = name.to_string();
            if self.store.upsert_worktree(&r).await.is_ok() {
                changed.push(r);
            }
        }
        changed
    }

    /// 项目改名之后更新 worktree 行里的项目名和位置。返回改了哪些行。
    pub async fn rename_project_rows(&self, from: &str, to: &str, new_loc: Option<&str>) -> Vec<WorktreeRow> {
        let Ok(rows) = self.store.list_worktrees().await else { return Vec::new() };
        let mut changed = Vec::new();
        for mut r in rows {
            if self.workspace.same_project_name(&r.project, from) {
                r.project = to.to_string();
                if let Some(l) = new_loc {
                    r.project_loc = Some(l.to_string());
                }
                if let Err(e) = self.store.upsert_worktree(&r).await {
                    tracing::warn!(worktree = %r.id, error = %e, "cannot update a worktree record after a project rename");
                    continue;
                }
                changed.push(r);
            }
        }
        changed
    }

    /// `workspace.worktrees` 的一项：现算未提交文件数、自己的提交数、磁盘占用（共用一个时间预算，超了的项 `bytes` 为空）。
    pub async fn info_of(&self, wt: &WorktreeRow, title: Option<String>, deadline: Instant) -> WorktreeInfo {
        let project_dir = self.workspace.project_path(&wt.project).ok();
        let mut state = wt.state;
        let (mut uncommitted, mut ahead, mut bytes) = (0u32, 0u32, None);
        if let Some(pd) = &project_dir {
            if matches!(wt.state, WorktreeState::Active | WorktreeState::Kept | WorktreeState::Missing) {
                match verify_full(pd, wt) {
                    WtCheck::Ok { dir, .. } => {
                        if wt.state == WorktreeState::Missing {
                            state = WorktreeState::Active;
                        }
                        uncommitted = self.uncommitted_in(&dir).await.unwrap_or(0);
                        bytes = dir_bytes_until(dir, deadline).await;
                    }
                    _ => state = WorktreeState::Missing,
                }
            }
            if !matches!(wt.state, WorktreeState::Creating | WorktreeState::Failed) {
                ahead = self.own_commits(pd, wt).await.unwrap_or(0);
            }
        } else if matches!(wt.state, WorktreeState::Active | WorktreeState::Kept) {
            state = WorktreeState::Missing;
        }
        WorktreeInfo {
            id: wt.id.clone(),
            project: wt.project.clone(),
            path: wt.rel_path.clone(),
            branch: wt.branch.clone(),
            base: wt.base_display.clone(),
            session_id: wt.session_id.clone(),
            session_title: title,
            state,
            uncommitted,
            ahead,
            bytes,
            created_at_ms: wt.created_ms,
        }
    }
}

/// 会话摘要 / `session.changed` 里的 worktree 描述。
pub fn session_worktree(w: &WorktreeRow) -> anycon_protocol::SessionWorktree {
    anycon_protocol::SessionWorktree {
        id: w.id.clone(),
        branch: w.branch.clone(),
        base: w.base_display.clone(),
        base_sha: w.base_sha.clone(),
        path: w.rel_path.clone(),
        state: w.state,
    }
}

/// 目录的磁盘占用（遍历，不跟随符号链接）；到 `deadline` 还没统计完返回 `None`。
async fn dir_bytes_until(dir: PathBuf, deadline: Instant) -> Option<u64> {
    tokio::task::spawn_blocking(move || {
        let mut total = 0u64;
        let mut stack = vec![dir];
        let mut visited = 0u32;
        while let Some(d) = stack.pop() {
            let rd = std::fs::read_dir(&d).ok()?;
            for e in rd.flatten() {
                visited += 1;
                if visited.is_multiple_of(256) && Instant::now() >= deadline {
                    return None;
                }
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    stack.push(e.path());
                } else {
                    total += md.len();
                }
            }
        }
        Some(total)
    })
    .await
    .ok()
    .flatten()
}

/// 把 `list`（`git ls-files -z` 的输出，相对 `src_root` 的路径）里的普通文件复制到 `dst_root` 下的同一相对路径。
fn copy_listed_files(
    src_root: &Path,
    dst_root: &Path,
    list: &[u8],
    max_files: u64,
    max_bytes: u64,
) -> (u32, Option<String>) {
    use std::os::unix::ffi::OsStrExt;
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut made_dirs: HashSet<PathBuf> = HashSet::new();
    let mut note = None;
    'next: for raw in list.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = Path::new(std::ffi::OsStr::from_bytes(raw));
        let comps: Vec<&std::ffi::OsStr> = rel
            .components()
            .map(|c| match c {
                std::path::Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        // 空 / 含 `..` 或绝对路径 / 动到 .git、.anycon 的一律跳过
        if comps.is_empty() || comps[0] == ".git" || comps[0] == ".anycon" {
            continue;
        }
        let src = src_root.join(rel);
        let Ok(mut f) = crate::fsopen::open_file_in(&src, src_root) else { continue };
        let Ok(md) = f.metadata() else { continue };
        if files + 1 > max_files || bytes + md.len() > max_bytes {
            note = Some(format!(
                "stopped copying .worktreeinclude files after {files} files / {bytes} bytes (limits: {max_files} files / {max_bytes} bytes)"
            ));
            break;
        }
        // 目标目录：一段一段建，每一段都必须是真目录
        let mut cur = dst_root.to_path_buf();
        for seg in &comps[..comps.len() - 1] {
            cur.push(seg);
            if made_dirs.contains(&cur) {
                continue;
            }
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => continue 'next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if std::fs::create_dir(&cur).is_err() {
                        continue 'next;
                    }
                }
                Err(_) => continue 'next,
            }
            made_dirs.insert(cur.clone());
        }
        let dst = dst_root.join(rel);
        let Ok(mut out) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(md.permissions().mode() & 0o777)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&dst)
        else {
            continue;
        };
        if std::io::copy(&mut f, &mut out).is_err() {
            drop(out);
            let _ = std::fs::remove_file(&dst);
            continue;
        }
        // umask 可能削掉了权限位：补回源文件的
        let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(md.permissions().mode() & 0o777));
        files += 1;
        bytes += md.len();
    }
    (files as u32, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_come_from_the_first_english_words() {
        assert_eq!(slug_from_prompt("Fix the login redirect bug please").as_deref(), Some("fix-the-login-redirect"));
        // 单个字母的词也是词（只要这几个词里有一个长度 ≥ 2 的）；大小写统一
        assert_eq!(slug_from_prompt("A B Refactor API").as_deref(), Some("a-b-refactor-api"));
        assert_eq!(slug_from_prompt("write a file").as_deref(), Some("write-a-file"));
        // 标点和非 ASCII 字符当分隔符
        assert_eq!(slug_from_prompt("add: café/menu!").as_deref(), Some("add-caf-menu"));
        // 截到 32 个字符，不留结尾的 `-`
        let s = slug_from_prompt("abcdefghijklmnopqrstuvwxyz abcdefgh xyz").unwrap();
        assert!(s.len() <= 32 && !s.ends_with('-'), "{s}");
        // 中文 / 空 / 单字母：没有可用的词
        assert_eq!(slug_from_prompt("修复登录问题"), None);
        assert_eq!(slug_from_prompt(""), None);
        assert_eq!(slug_from_prompt("a b c"), None);
        assert_eq!(slug_from_prompt("a b c d fix"), None, "only the first four words count");
    }

    #[test]
    fn bases_are_displayed_without_the_ref_prefix_and_legacy_spellings_never_follow_the_worktrees_own_head() {
        let row = |base: &str| WorktreeRow {
            id: "wt".into(),
            project_loc: None,
            project: "p".into(),
            rel_path: ".anycon/worktrees/x".into(),
            branch: "b".into(),
            base: base.into(),
            base_display: display_base(base),
            base_sha: "1111111111111111111111111111111111111111".into(),
            session_id: None,
            state: WorktreeState::Active,
            identity: None,
            git_pointer: None,
            created_ms: 0,
        };
        let sha = "2222222222222222222222222222222222222222";
        assert_eq!(display_base("refs/heads/main"), "main");
        assert_eq!(display_base("refs/remotes/origin/main"), "origin/main");
        assert_eq!(display_base("refs/tags/v1.0"), "v1.0");
        assert_eq!(display_base(sha), "222222222222", "a commit is shown by its first 12 digits");
        assert_eq!(display_base("main"), "main");
        // 新记录原样用
        for b in ["refs/heads/main", "refs/remotes/origin/feature/x", "refs/tags/v1.0", sha] {
            assert!(is_normalized_base(b), "{b}");
            assert_eq!(base_spec(&row(b)), b);
        }
        // 旧记录（短名字）：像分支名的原样用，在 worktree 里会指向 worktree 自己的东西的写法改用创建时的提交
        for b in ["main", "origin/main", "release/1.0", "v1.2.3"] {
            assert!(!is_normalized_base(b), "{b}");
            assert_eq!(base_spec(&row(b)), b);
        }
        for b in [
            "HEAD",
            "@",
            "ORIG_HEAD",
            "FETCH_HEAD",
            "MERGE_HEAD",
            "AUTO_MERGE",
            "HEAD~1",
            "main~2",
            "main^",
            "@{u}",
            "main@{1}",
            ":/fix",
            "a..b",
            "a b",
        ] {
            assert_eq!(base_spec(&row(b)), "1111111111111111111111111111111111111111", "{b}");
        }
    }

    #[test]
    fn random_slugs_are_two_words_from_the_built_in_lists() {
        let mut rng = rand::thread_rng();
        for _ in 0..50 {
            let s = random_slug(&mut rng);
            let (a, n) = s.split_once('-').unwrap();
            assert!(ADJECTIVES.contains(&a) && NOUNS.contains(&n), "{s}");
        }
    }

    #[test]
    fn names_are_deduplicated_with_numeric_suffixes() {
        let taken: HashSet<&str> = ["fix", "fix-2", "fix-3"].into();
        assert_eq!(dedupe_name("fix", |n| taken.contains(n)), "fix-4");
        assert_eq!(dedupe_name("other", |n| taken.contains(n)), "other");
    }

    #[test]
    fn directory_names_keep_only_safe_characters() {
        assert_eq!(dir_name_for_branch("anycon/Fix-Login"), "fix-login");
        assert_eq!(dir_name_for_branch("feature/a b/c@d"), "c-d");
        assert_eq!(dir_name_for_branch("anycon/.hidden"), "hidden");
        assert_eq!(dir_name_for_branch("anycon/.."), "wt");
        assert_eq!(dir_name_for_branch("anycon/"), "wt");
        assert_eq!(dir_name_for_branch("x/日本語"), "wt");
    }

    #[test]
    fn recorded_paths_must_be_exactly_three_safe_segments() {
        assert!(rel_path_ok(".anycon/worktrees/fix-login"));
        for bad in [
            "",
            ".anycon/worktrees",
            ".anycon/worktrees/a/b",
            ".anycon/worktrees/..",
            ".anycon/worktrees/.",
            "other/worktrees/a",
            "/.anycon/worktrees/a",
            ".anycon//a",
            "../x/y",
        ] {
            assert!(!rel_path_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_worktree_directory_is_verified_segment_by_segment_and_by_identity() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(d.path()).unwrap();
        let rel = ".anycon/worktrees/x";
        assert_eq!(inspect(&project, rel), WtCheck::Gone);
        std::fs::create_dir_all(project.join(rel)).unwrap();
        let WtCheck::Ok { identity, dir } = inspect(&project, rel) else { panic!("expected a directory") };
        assert_eq!(dir, project.join(rel));
        let mut wt = WorktreeRow {
            id: "wt_x".into(),
            project_loc: None,
            project: "p".into(),
            rel_path: rel.into(),
            branch: "b".into(),
            base: "main".into(),
            base_display: "main".into(),
            base_sha: "0".into(),
            session_id: None,
            state: WorktreeState::Active,
            identity: Some(identity),
            git_pointer: None,
            created_ms: 0,
        };
        assert!(matches!(verify(&project, &wt), WtCheck::Ok { .. }));
        // 身份不一致：换了一个目录（删掉重建）
        wt.identity = Some((identity.0, identity.1 + 12345));
        assert!(matches!(verify(&project, &wt), WtCheck::Replaced(_)));
        // 没记过身份：不认
        wt.identity = None;
        assert!(matches!(verify(&project, &wt), WtCheck::Replaced(_)));
        // 末段换成指向别处的符号链接
        std::fs::remove_dir(project.join(rel)).unwrap();
        let elsewhere = project.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, project.join(rel)).unwrap();
        assert!(matches!(inspect(&project, rel), WtCheck::Replaced(_)));
        // 中间段换成符号链接
        std::fs::remove_file(project.join(rel)).unwrap();
        std::fs::remove_dir(project.join(".anycon/worktrees")).unwrap();
        std::fs::remove_dir(project.join(".anycon")).unwrap();
        symlink(&elsewhere, project.join(".anycon")).unwrap();
        assert!(matches!(inspect(&project, rel), WtCheck::Replaced(_)));
        // 中间段是文件
        std::fs::remove_file(project.join(".anycon")).unwrap();
        std::fs::write(project.join(".anycon"), "x").unwrap();
        assert!(matches!(inspect(&project, rel), WtCheck::Replaced(_)));
    }

    #[test]
    fn the_include_copier_skips_links_and_special_paths_and_stops_at_the_limits() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let (src, dst) = (root.join("src"), root.join("dst"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(src.join(".anycon/worktrees/other")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join(".env"), "A=1").unwrap();
        std::fs::set_permissions(src.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(src.join("sub/run.sh"), "#!/bin/sh").unwrap();
        std::fs::set_permissions(src.join("sub/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(src.join("outside.txt"), "secret").unwrap();
        symlink(src.join("outside.txt"), src.join("link.txt")).unwrap();
        std::fs::write(src.join(".anycon/worktrees/other/f"), "nested").unwrap();
        // 目标里 `sub` 事先是个指向别处的符号链接：不能写穿
        let trap = root.join("trap");
        std::fs::create_dir(&trap).unwrap();
        let list = b".env\0sub/run.sh\0link.txt\0.anycon/worktrees/other/f\0../escape\0.git/config\0missing.txt\0";
        let (n, note) = copy_listed_files(&src, &dst, list, 100, 1 << 20);
        assert_eq!((n, note), (2, None));
        assert_eq!(std::fs::read_to_string(dst.join(".env")).unwrap(), "A=1");
        assert_eq!(std::fs::metadata(dst.join(".env")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dst.join("sub/run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!dst.join("link.txt").exists() && std::fs::symlink_metadata(dst.join("link.txt")).is_err());
        assert!(!dst.join(".anycon").exists());
        // 符号链接的目标目录
        let dst2 = root.join("dst2");
        std::fs::create_dir(&dst2).unwrap();
        symlink(&trap, dst2.join("sub")).unwrap();
        let (n, _) = copy_listed_files(&src, &dst2, b"sub/run.sh\0.env\0", 100, 1 << 20);
        assert_eq!(n, 1, "only .env: sub is a symbolic link in the destination");
        assert!(std::fs::read_dir(&trap).unwrap().next().is_none(), "nothing may be written through the link");
        // 上限
        let dst3 = root.join("dst3");
        std::fs::create_dir(&dst3).unwrap();
        let (n, note) = copy_listed_files(&src, &dst3, b".env\0sub/run.sh\0", 1, 1 << 20);
        assert_eq!(n, 1);
        assert!(note.unwrap().contains("limits: 1 files"));
        let dst4 = root.join("dst4");
        std::fs::create_dir(&dst4).unwrap();
        let (n, note) = copy_listed_files(&src, &dst4, b".env\0sub/run.sh\0", 100, 4);
        assert_eq!(n, 1);
        assert!(note.is_some());
    }
}
