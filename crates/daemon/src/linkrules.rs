//! 关联已有目录的规则引擎（ADR-33「风险与对策」五条、daemon.md § 10）。
//!
//! 这里只有「给定一份快照（`link_roots`、workspace 根、数据目录、拒绝表、已关联的项目），一个 canonical 目录能不能关联、
//! 为什么不能」的纯判断，不碰会话、不发事件。`workspace.rs` 在 `link` / `browse` 和每次使用关联项目（`list`、`fs.*`、
//! 新建会话、拉起 agent）时各取一份快照，用同一套规则判断。
//!
//! 规则（顺序即报告原因的优先级）：
//! 1. 范围：`link_roots` 之内（`link` / `browse` 检查；使用已有关联时不查，收紧范围不自动解除关联）。**每一项都不能经过符号链接**：
//!    `~` 按启动时固定的 canonical HOME 展开、系统别名（`/var` `/tmp` `/etc` ↔ `/private/…`，macOS 的
//!    firmlink `/System/Volumes/Data/X` ↔ `/X`，见 [`PathAliases`]）规范化之后**逐段 `lstat`**，任何一段是符号链接就整项忽略
//!    （`workspace.rs` 的 `judge_root`）；大小写或 NFC / NFD 写法和盘上不同不算链接，以 canonical 路径为准；每次请求都重判，不靠快照。
//!    请求的路径先做词法规范化，**不在任何 root 之内就一律回「不在 `link_roots` 之内」**，不管它解析到哪里、存不存在；
//! 2. 绝对拒绝（不能放行）：文件系统根；workspace 根目录里面（`invalid`）或包含它；anycon 数据目录、在它里面或包含它。「在里面 / 包含」
//!    除了路径前缀，还按目录身份 `(dev, ino)` 沿祖先链判断；
//! 3. 内置拒绝表（`Sensitive`，启发式的）：系统、凭证、自启动目录。等于、在里面、或包含其中任何一个都拒绝；只有 `link_roots` 的某一项
//!    本身就在拒绝项 D 里面（含等于）时，那一项**之下**不再受 D 限制，更深的拒绝项仍然生效。root 不经过符号链接（规则 1），
//!    所以「root 在 D 里面」只需要看它的 canonical 路径；
//! 4. 隐藏目录（`Hidden`）：从包含它的、最深的 link root 往下，路径里有以 `.` 开头的一段就拒绝；
//! 5. 使用时复核（[`LinkCtx::resolve_linked`]）：canonicalize 之后必须**等于登记的路径**，并重跑 2–4。
//!
//! Windows 上：「符号链接」包括 junction（标准库的 `is_symlink` 两者都认）；文件系统根是每个盘符的根和 UNC 共享的根；
//! 拒绝表换成 Windows 的系统目录（按 `SystemRoot`、`ProgramFiles` 等环境变量定位）和 home 下的 `AppData`、PowerShell 配置等；
//! 没有 `/var` 这类系统别名和 firmlink。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anycon_protocol::{ErrorCode, LinkBlock, RpcError};
use unicode_normalization::UnicodeNormalization;

pub const MAX_PATH_BYTES: usize = 4096;

type RpcResult<T> = Result<T, RpcError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

/// 拒绝表里 `$HOME` 之下的项（ADR-33 第 3 条）。`.config/autostart` 等是 `.config` 的子项，单列是为了
/// 「放行 `~/.config` 仍然不放行 `~/.config/systemd`」。
const DENY_UNDER_HOME: &[&str] = &[
    "Library",
    "Library/LaunchAgents",
    "Library/Keychains",
    "bin",
    "go/bin",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config",
    ".config/autostart",
    ".config/systemd",
    ".config/gcloud",
    ".config/gh",
    ".config/fish",
    ".local",
    ".password-store",
    ".codex",
    ".claude",
];

/// Windows：拒绝表里 home 之下的项。`AppData` 里有自启动目录、凭证和各种程序的配置；PowerShell 的配置脚本每次启动都会执行。
/// 第二项比 `AppData` 深，单列是为了「放行 `AppData` 仍然不放行自启动目录」。
#[cfg(windows)]
const DENY_UNDER_HOME_WINDOWS: &[&str] = &[
    "AppData",
    "AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup",
    "Documents/PowerShell",
    "Documents/WindowsPowerShell",
    "scoop",
];

/// 拒绝表里的绝对路径（系统目录）。macOS 上 `/etc` `/var` 是 `/private/…` 的符号链接，canonicalize 之后才比较。
#[cfg(not(windows))]
const DENY_ABSOLUTE: &[&str] = &[
    "/etc",
    "/private",
    "/System",
    "/Library",
    "/usr",
    "/bin",
    "/sbin",
    "/var",
    "/dev",
    "/opt",
    "/proc",
    "/sys",
    "/boot",
    "/root",
    "/Applications",
];

/// Windows 的系统目录：按环境变量定位（系统盘不一定是 `C:`），取不到时用默认位置。
#[cfg(windows)]
fn deny_absolute() -> Vec<PathBuf> {
    let var = |k: &str, default: &str| std::env::var_os(k).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default));
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    vec![
        var("SystemRoot", r"C:\Windows"),
        var("ProgramFiles", r"C:\Program Files"),
        var("ProgramFiles(x86)", r"C:\Program Files (x86)"),
        var("ProgramW6432", r"C:\Program Files"),
        var("ProgramData", r"C:\ProgramData"),
        PathBuf::from(format!(r"{drive}\Users\Public")),
        PathBuf::from(format!(r"{drive}\Users\Default")),
        PathBuf::from(format!(r"{drive}\$Recycle.Bin")),
        PathBuf::from(format!(r"{drive}\System Volume Information")),
        PathBuf::from(format!(r"{drive}\Recovery")),
    ]
}

#[cfg(not(windows))]
fn deny_absolute() -> Vec<PathBuf> {
    DENY_ABSOLUTE.iter().map(PathBuf::from).collect()
}

/// home 之下的拒绝项（相对路径）。
fn deny_under_home() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        DENY_UNDER_HOME.iter().chain(DENY_UNDER_HOME_WINDOWS).copied().collect()
    }
    #[cfg(not(windows))]
    {
        DENY_UNDER_HOME.to_vec()
    }
}

/// macOS 上第一次访问就会弹「隐私与安全性」授权的 home 下的目录；列 `~` 时不去探测它们里面有没有 `.git`。
const TCC_UNDER_HOME: &[&str] = &["Desktop", "Documents", "Downloads", "Pictures", "Movies", "Music", "Public"];

/// 路径参数的文本检查：非空、≤ 4096 字节、不含 NUL / 控制字符。
pub fn check_path_text(raw: &str) -> RpcResult<()> {
    if raw.is_empty() {
        return Err(err(ErrorCode::Invalid, "path is empty"));
    }
    if raw.len() > MAX_PATH_BYTES {
        return Err(err(ErrorCode::Invalid, format!("path is longer than {MAX_PATH_BYTES} bytes")));
    }
    if raw.chars().any(|c| c.is_control()) {
        return Err(err(ErrorCode::Invalid, "path contains NUL or control characters"));
    }
    Ok(())
}

/// 绝对路径原样返回；`~` / `~/…`（Windows 上也认 `~\…`）按 `home` 展开；其余（相对路径、`~user`）拒绝。
pub fn expand_with_home(raw: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let rest = crate::util::strip_tilde_slash(raw);
    if raw == "~" || rest.is_some() {
        let home = home.ok_or_else(|| "cannot expand `~`: the daemon environment has no usable HOME".to_string())?;
        return Ok(match rest {
            Some(rest) => home.join(rest),
            None => home.to_path_buf(),
        });
    }
    if Path::new(raw).is_absolute() {
        return Ok(PathBuf::from(raw));
    }
    Err("path must be absolute, `~` or `~/…`".to_string())
}

/// [`expand_with_home`]，`home` 取 daemon 环境里的家目录（`HOME`，Windows 上 `USERPROFILE`；必须是绝对路径）。
pub fn expand_home(raw: &str, env: &HashMap<String, String>) -> Result<PathBuf, String> {
    expand_with_home(raw, anycon_agent_api::env_home(env).map(Path::new))
}

/// 不碰文件系统地去掉 `.` 和 `..`（只用来决定错误码，不用来授权）。Windows 上 `\\?\C:\…` 这种写法先换成 `C:\…`
/// （和 [`crate::sys::canonicalize`] 的结果一致），否则同一个目录会被当成两个不同的路径。
pub fn lexical_clean(p: &Path) -> PathBuf {
    let p = crate::sys::simplified(p);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 存在的那一段 canonicalize，再接上还不存在的尾巴（拒绝表里的 `~/.ssh` 现在不存在也要按 canonical 的 `$HOME` 比较）。
pub fn canonicalize_lenient(p: &Path) -> PathBuf {
    let p = lexical_clean(p);
    let mut existing = p.clone();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match crate::sys::canonicalize(&existing) {
            Ok(mut c) => {
                for t in tail.iter().rev() {
                    c.push(t);
                }
                return c;
            }
            Err(_) => match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name);
                    existing = parent.to_path_buf();
                }
                _ => return p,
            },
        }
    }
}

/// 词法比较用的键：每一段按 NFC 规范化，`case_insensitive` 时再转小写。只用来决定「是不是在某个前缀之下」，
/// 不用来授权——真正的判断仍然是 canonical 路径。
pub fn fold_path(p: &Path, case_insensitive: bool) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => match s.to_str() {
                Some(t) => {
                    let n = nfc(t);
                    out.push(if case_insensitive { n.to_lowercase() } else { n });
                }
                None => out.push(s),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `p` 在 `prefix` 之下（含相等）：按 [`fold_path`] 比较。
pub fn starts_with_folded(p: &Path, prefix: &Path, case_insensitive: bool) -> bool {
    fold_path(p, case_insensitive).starts_with(fold_path(prefix, case_insensitive))
}

/// 目录的身份：`(设备号, inode)`（Windows：卷序列号和文件索引）。
pub fn identity_of(p: &Path) -> Option<(u64, u64)> {
    crate::sys::identity_of(p)
}

/// 一个目录自己和它所有祖先的身份：绝对拒绝的「在里面 / 包含」沿这条链判断，不看路径是哪种写法。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identities {
    /// 目录自己。
    pub me: Option<(u64, u64)>,
    /// 目录自己和所有祖先（能 `stat` 到的）。
    pub chain: Vec<(u64, u64)>,
}

impl Identities {
    pub fn of(p: &Path) -> Identities {
        Identities { me: identity_of(p), chain: p.ancestors().filter_map(identity_of).collect() }
    }
}

/// 项目名统一 NFC。
pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// 比较项目名用的键：NFC，在不区分大小写的文件系统上再转小写。
pub fn name_key(name: &str, case_insensitive: bool) -> String {
    let n = nfc(name);
    if case_insensitive {
        n.to_lowercase()
    } else {
        n
    }
}

/// 拒绝表的一项：词法路径（`$HOME` 按 daemon 环境里的原样展开，`/etc` 就是 `/etc`）和 canonical 路径（`/private/etc`）。
/// 命中时两种写法都认；显式放行的例外要求 root 的词法路径和 canonical 路径**都**落在这一项里。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DenyEntry {
    pub lexical: PathBuf,
    pub canon: PathBuf,
}

impl DenyEntry {
    fn new(lexical: PathBuf) -> DenyEntry {
        let lexical = lexical_clean(&lexical);
        let canon = canonicalize_lenient(&lexical);
        DenyEntry { lexical, canon }
    }
}

/// 内置拒绝表（`$HOME` 之下的项只有 `home` 有效时才有）。
pub fn deny_table(home: Option<&Path>) -> Vec<DenyEntry> {
    deny_table_with(home, home.map(canonicalize_lenient).as_deref())
}

/// [`deny_table`]，`$HOME` 的词法路径（daemon 环境里的原样）和 canonical 路径分开给：canonical 的那个由调用方在启动时固定下来，
/// 之后 HOME 这个链接被改指向别处，拒绝表仍然指着原来的目录。
pub fn deny_table_with(home: Option<&Path>, home_canon: Option<&Path>) -> Vec<DenyEntry> {
    let mut out: Vec<DenyEntry> = deny_absolute().into_iter().map(DenyEntry::new).collect();
    if let (Some(h), Some(c)) = (home, home_canon) {
        out.extend(deny_under_home().into_iter().map(|rel| {
            let lexical = lexical_clean(&h.join(rel));
            DenyEntry { lexical, canon: canonicalize_lenient(&c.join(rel)) }
        }));
    }
    out.sort();
    out.dedup();
    out
}

/// 系统别名：macOS 上 `/var` `/tmp` `/etc` 是指向 `/private/…` 的符号链接，运维和脚本通常写短的那个。返回 `(别名, 真实位置)`，
/// 只含这台机器上确实是别名的（Linux 上一般是空的，Windows 上没有）。
pub fn system_aliases() -> Vec<(PathBuf, PathBuf)> {
    if cfg!(windows) {
        return Vec::new();
    }
    ["/var", "/tmp", "/etc"]
        .iter()
        .filter_map(|a| {
            let alias = PathBuf::from(a);
            let real = crate::sys::canonicalize(&alias).ok()?;
            (real != alias).then_some((alias, real))
        })
        .collect()
}

/// macOS 的 firmlink：`/System/Volumes/Data/X` 和 `/X` 是同一个目录，`realpath` 对两种写法都原样返回，所以不是符号链接，
/// 路径字符串却不同。返回 `(数据卷前缀, 系统卷前缀)`；这台机器上没有 `/System/Volumes/Data` 就是空的。
pub fn system_firmlinks() -> Vec<(PathBuf, PathBuf)> {
    let data = PathBuf::from("/System/Volumes/Data");
    if data.is_dir() {
        vec![(data, PathBuf::from("/"))]
    } else {
        Vec::new()
    }
}

/// 词法路径以某个系统别名开头就换成真实位置（`/var/folders/…` → `/private/var/folders/…`）。只认 [`system_aliases`] 列出的，
/// 不信任别的符号链接。
pub fn normalize_aliases(p: &Path, aliases: &[(PathBuf, PathBuf)]) -> PathBuf {
    let p = lexical_clean(p);
    for (alias, real) in aliases {
        if let Ok(rest) = p.strip_prefix(alias) {
            return real.join(rest);
        }
    }
    p
}

/// `data/X…` 和 `sys/X…` 是同一个目录（`(dev, ino)` 相同）时，把前者换成后者。从最短的前缀开始找（`Data/Users` ↔ `/Users`，
/// `Data/usr/local` ↔ `/usr/local`）；找不到相同身份的前缀就原样返回。
pub fn normalize_firmlinks(p: &Path, firmlinks: &[(PathBuf, PathBuf)]) -> PathBuf {
    let p = lexical_clean(p);
    for (data, sys) in firmlinks {
        let Ok(rest) = p.strip_prefix(data) else { continue };
        let comps: Vec<&std::ffi::OsStr> = rest.components().map(|c| c.as_os_str()).collect();
        let mut upto = PathBuf::new();
        for (i, c) in comps.iter().enumerate() {
            upto.push(c);
            let (a, b) = (identity_of(&data.join(&upto)), identity_of(&sys.join(&upto)));
            if a.is_some() && a == b {
                let mut out = sys.join(&upto);
                out.extend(&comps[i + 1..]);
                return out;
            }
        }
    }
    p
}

/// 词法比较前要换掉的所有写法：前缀别名（`/var` → `/private/var`，以及 HOME 的原样路径 → canonical 路径）和 firmlink。
#[derive(Debug, Clone, Default)]
pub struct PathAliases {
    pub prefixes: Vec<(PathBuf, PathBuf)>,
    pub firmlinks: Vec<(PathBuf, PathBuf)>,
}

impl PathAliases {
    /// 这台机器的系统别名和 firmlink。
    pub fn system() -> PathAliases {
        PathAliases { prefixes: system_aliases(), firmlinks: system_firmlinks() }
    }

    /// 去掉 `.` / `..`，换成规范的写法（firmlink 在前，前缀别名在后）。
    pub fn normalize(&self, p: &Path) -> PathBuf {
        normalize_aliases(&normalize_firmlinks(p, &self.firmlinks), &self.prefixes)
    }
}

/// 列目录时不探测 `.git` 的目录（canonical）。
pub fn tcc_table(home: Option<&Path>) -> Vec<PathBuf> {
    match home {
        Some(h) => TCC_UNDER_HOME.iter().map(|rel| canonicalize_lenient(&h.join(rel))).collect(),
        None => Vec::new(),
    }
}

/// 一个目录不能关联的原因。
#[derive(Debug, Clone)]
pub struct Blocked {
    pub block: LinkBlock,
    /// `link` 返回的错误码（在 workspace 根里面是 `invalid`，其余 `forbidden`）。
    pub code: ErrorCode,
    pub message: String,
    /// `workspace.browse` 能不能进到这个目录里面：拒绝表目录和数据目录（以及它们里面的）不能；
    /// 「包含」它们的上级目录（比如 `~`）和隐藏目录可以进（里面的条目各自再判断）。
    pub enterable: bool,
}

/// `link_roots` 展开并 canonicalize 之后的范围（只含**生效**的 root：不经过符号链接、目录存在）。
#[derive(Debug, Clone, Default)]
pub struct LinkRoots {
    /// 生效的 root 的规范化路径（canonical HOME 展开 `~`、系统别名和 firmlink 换成规范写法、大小写和盘上一致）。因为生效的 root
    /// 不能经过符号链接，它和 `canon` 相同；两个字段分开留着是为了「词法路径」和「canonical 路径」两种比较。
    pub raw: Vec<PathBuf>,
    /// 生效的 root 的 canonical 路径。
    pub canon: Vec<PathBuf>,
}

/// 「不在 `link_roots` 之内」的错误消息：`link` / `browse` 对范围之外的一切路径（存在与否、是文件还是目录、经什么写法进来）
/// 给同一句话。
pub const OUTSIDE_ROOTS: &str = "the path is outside workspace.link_roots";

impl LinkRoots {
    pub fn within(&self, canon: &Path) -> bool {
        self.canon.iter().any(|r| canon.starts_with(r))
    }

    /// 词法上落在某个生效的 root 之内（先规范化的路径，不碰文件系统）。`case_insensitive` 时按 NFC + 小写折叠比较。
    pub fn lexically_within(&self, p: &Path, case_insensitive: bool) -> bool {
        let p = fold_path(&lexical_clean(p), case_insensitive);
        self.raw.iter().chain(self.canon.iter()).any(|r| p.starts_with(fold_path(r, case_insensitive)))
    }

    pub fn is_root(&self, canon: &Path) -> bool {
        self.canon.iter().any(|r| r == canon)
    }

    /// 包含 `canon` 的、最深的 root。
    fn deepest_containing(&self, canon: &Path) -> Option<&PathBuf> {
        self.canon.iter().filter(|r| canon.starts_with(r)).max_by_key(|r| r.components().count())
    }
}

/// 使用已有关联时发现的问题（规则 5）：当作 `missing`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkedProblem {
    /// 目录不在了，或不是目录，或无法访问。
    Missing,
    /// canonicalize 之后不等于登记的路径：目录（或它的上级）被换成了符号链接 / 被移走又换了别的。
    Moved,
    /// 按当前配置这个目录已经不允许关联（隐藏目录、拒绝表、workspace 根、数据目录…）。
    NotAllowed(LinkBlock),
}

impl LinkedProblem {
    /// 给 `not_found` 错误用的说明（不写出新的解析结果：它可能正是攻击者想让你看到的位置）。
    pub fn describe(&self, name: &str) -> String {
        match self {
            LinkedProblem::Missing => format!("the directory of linked project {name} is missing"),
            LinkedProblem::Moved => format!(
                "the directory of linked project {name} no longer resolves to the path it was linked at (it was moved or replaced by a symbolic link); unlink it and link it again"
            ),
            LinkedProblem::NotAllowed(b) => format!(
                "linked project {name} is not allowed any more under the current rules ({}); unlink it",
                block_name(*b)
            ),
        }
    }
}

pub fn block_name(b: LinkBlock) -> &'static str {
    match b {
        LinkBlock::Hidden => "hidden directory",
        LinkBlock::Sensitive => "sensitive system or credential directory",
        LinkBlock::Workspace => "inside or containing the workspace root",
        LinkBlock::DataDir => "anycon data directory",
        LinkBlock::FilesystemRoot => "filesystem root",
    }
}

/// 一次关联 / 浏览 / 使用要用到的全部判断依据（取一份快照，浏览时可以搬进阻塞线程）。
#[derive(Debug, Clone)]
pub struct LinkCtx {
    pub roots: LinkRoots,
    /// workspace 根目录（canonical）。
    pub root: PathBuf,
    /// anycon 数据目录（canonical；不存在就是配置里的原样）。
    pub data: PathBuf,
    /// anycon 数据目录的词法路径（配置里的原样，只用于不存在性无关的词法判断）。
    pub data_lexical: PathBuf,
    /// 内置拒绝表。
    pub deny: Vec<DenyEntry>,
    /// workspace 根目录和它所有祖先的身份。
    pub root_ids: Identities,
    /// anycon 数据目录和它所有祖先的身份。
    pub data_ids: Identities,
    /// 这台机器上的系统别名（`/var` → `/private/var`、firmlink …），词法比较前先换掉。
    pub aliases: PathAliases,
    /// 列目录时不探测 `.git` 的目录（canonical）。
    pub tcc: Vec<PathBuf>,
    /// 已关联的项目：名字 → 登记的路径。
    pub linked: Vec<(String, PathBuf)>,
    /// workspace 根所在的文件系统不区分大小写。
    pub case_insensitive: bool,
}

impl LinkCtx {
    /// 这个 canonical 目录现在已经是哪个项目（根目录下的一级文件夹，或已关联的）。
    pub fn project_name(&self, canon: &Path) -> Option<String> {
        if let Some((name, _)) = self.linked.iter().find(|(_, p)| p == canon) {
            return Some(name.clone());
        }
        if canon.parent() == Some(self.root.as_path()) {
            let name = canon.file_name()?.to_str()?;
            let key = name_key(name, self.case_insensitive);
            if !name.starts_with('.') && !self.linked.iter().any(|(n, _)| name_key(n, self.case_insensitive) == key) {
                return Some(name.to_string());
            }
        }
        None
    }

    /// 显式放行（ADR-33 第 3 条）：有没有一项 root 本身在拒绝项 `d` 里面，并且 `under` 在它之下。root 不经过符号链接（规则 1，
    /// 经过的整项被忽略，到不了这里），所以「在 D 里面」只看 canonical 路径。`under_is_canonical` 说明 `under` 是 canonical
    /// 路径（和 root 的 canonical 路径比）还是（已经规范化的）词法路径（和 root 的路径按 [`fold_path`] 比）。
    fn exempts(&self, d: &DenyEntry, under: &Path, under_is_canonical: bool) -> bool {
        self.roots.raw.iter().zip(self.roots.canon.iter()).any(|(raw, canon)| {
            canon.starts_with(&d.canon)
                && if under_is_canonical {
                    under.starts_with(canon)
                } else {
                    starts_with_folded(under, raw, self.case_insensitive)
                }
        })
    }

    /// 规则 2–4：这个 canonical 目录按当前配置能不能被关联（不含范围和「已关联」）。
    pub fn block_of(&self, canon: &Path) -> Option<Blocked> {
        let blocked = |block, code, enterable, message: String| Some(Blocked { block, code, message, enterable });
        // ---- 绝对拒绝
        if canon.parent().is_none() {
            return blocked(
                LinkBlock::FilesystemRoot,
                ErrorCode::Forbidden,
                true,
                "the filesystem root cannot be linked".into(),
            );
        }
        // 绝对拒绝按路径前缀**和**目录身份 (dev, ino) 沿祖先链判断：同一个目录的另一种写法（firmlink、大小写变体、
        // 别的 canonical 写法）也绕不过去。身份取不到（路径不存在）时只剩路径前缀。
        let cand = if self.root_ids.me.is_some() || self.data_ids.me.is_some() {
            Identities::of(canon)
        } else {
            Identities::default()
        };
        let is_or_contains = |ids: &Identities| cand.me.is_some_and(|m| ids.chain.contains(&m));
        let is_or_inside = |ids: &Identities| ids.me.is_some_and(|m| cand.chain.contains(&m));
        if self.root.starts_with(canon) || is_or_contains(&self.root_ids) {
            return blocked(
                LinkBlock::Workspace,
                ErrorCode::Forbidden,
                true,
                "the directory is, or contains, the workspace root".into(),
            );
        }
        if canon.starts_with(&self.root) || is_or_inside(&self.root_ids) {
            return blocked(
                LinkBlock::Workspace,
                ErrorCode::Invalid,
                true,
                "the directory is inside the workspace root: it is already a project there (or lives in .trash / .scratch)"
                    .into(),
            );
        }
        if canon.starts_with(&self.data) || is_or_inside(&self.data_ids) {
            return blocked(
                LinkBlock::DataDir,
                ErrorCode::Forbidden,
                false,
                "the directory is, or is inside, the anycon data directory".into(),
            );
        }
        if self.data.starts_with(canon) || is_or_contains(&self.data_ids) {
            return blocked(
                LinkBlock::DataDir,
                ErrorCode::Forbidden,
                true,
                "the directory contains the anycon data directory".into(),
            );
        }
        // ---- 内置拒绝表（启发式）
        for d in &self.deny {
            if canon.starts_with(&d.canon) {
                // 例外：link_roots 的某一项本身在 D 里面（词法路径和 canonical 路径都在），并且 canon 在那一项之下
                if !self.exempts(d, canon, true) {
                    return blocked(
                        LinkBlock::Sensitive,
                        ErrorCode::Forbidden,
                        false,
                        format!(
                            "the directory is inside a protected system or credential location ({}); to allow a directory in there, list it explicitly in workspace.link_roots",
                            d.canon.display()
                        ),
                    );
                }
            }
        }
        // 包含拒绝项（`~`、`/Users` 这类上级目录）：点名一个**确实存在**的（没有存在的就点名第一个）
        let contained: Vec<&DenyEntry> =
            self.deny.iter().filter(|d| d.canon.starts_with(canon) && d.canon != canon).collect();
        if let Some(d) = contained.iter().find(|d| d.canon.exists()).or(contained.first()) {
            return blocked(
                LinkBlock::Sensitive,
                ErrorCode::Forbidden,
                true,
                format!(
                    "the directory contains a protected system or credential location ({}); link a narrower directory",
                    d.canon.display()
                ),
            );
        }
        // ---- 隐藏目录：相对于包含它的、最深的 root
        if let Some(r) = self.roots.deepest_containing(canon) {
            if let Ok(rel) = canon.strip_prefix(r) {
                let hidden = rel.components().find_map(|c| match c {
                    Component::Normal(s) if s.to_string_lossy().starts_with('.') => {
                        Some(s.to_string_lossy().into_owned())
                    }
                    _ => None,
                });
                if let Some(seg) = hidden {
                    return blocked(
                        LinkBlock::Hidden,
                        ErrorCode::Forbidden,
                        true,
                        format!(
                            "the path has a hidden segment ({seg}); a hidden directory can be linked only if it (or a hidden directory above it) is listed explicitly in workspace.link_roots"
                        ),
                    );
                }
            }
        }
        None
    }

    /// 词法路径的规范化：去掉 `.` / `..`，系统别名和 firmlink 换成规范的写法。
    pub fn normalize(&self, p: &Path) -> PathBuf {
        self.aliases.normalize(p)
    }

    /// 按当前的 `root` / `data` 重新取它们和祖先的身份。
    pub fn refresh_identities(&mut self) {
        self.root_ids = Identities::of(&self.root);
        self.data_ids = Identities::of(&self.data);
    }

    /// 按**词法路径**判断（不碰文件系统）：落在拒绝表 / 数据目录里面就拒绝，不管这个路径存不存在、是不是目录
    /// （`forbidden` 和 `not_found` / `invalid` 的差别不能用来探测 `~/.aws/credentials` 之类有没有）。
    /// 不区分大小写的卷上按 NFC + 小写折叠比较（`~/.SSH/…` 和 `~/.ssh/…` 一样）。
    /// 显式放行的 root 之下交给后面按 canonical 路径的判断。
    pub fn lexical_block(&self, lexical: &Path) -> Option<Blocked> {
        let ci = self.case_insensitive;
        let lex = self.normalize(lexical);
        if starts_with_folded(&lex, &self.data, ci) || starts_with_folded(&lex, &self.data_lexical, ci) {
            return Some(Blocked {
                block: LinkBlock::DataDir,
                code: ErrorCode::Forbidden,
                message: "the directory is, or is inside, the anycon data directory".into(),
                enterable: false,
            });
        }
        for d in &self.deny {
            if (starts_with_folded(&lex, &d.lexical, ci) || starts_with_folded(&lex, &d.canon, ci))
                && !(self.exempts(d, &lex, false) || self.exempts(d, &lex, true))
            {
                return Some(Blocked {
                    block: LinkBlock::Sensitive,
                    code: ErrorCode::Forbidden,
                    message: format!(
                        "the directory is inside a protected system or credential location ({}); to allow a directory in there, list it explicitly in workspace.link_roots",
                        d.canon.display()
                    ),
                    enterable: false,
                });
            }
        }
        None
    }

    /// 能不能把这个 canonical 目录关联为项目；不能时给出 daemon.md § 10 规定的错误码。
    pub fn verdict(&self, canon: &Path) -> RpcResult<()> {
        if !self.roots.within(canon) {
            return Err(err(ErrorCode::Forbidden, OUTSIDE_ROOTS));
        }
        if let Some(b) = self.block_of(canon) {
            return Err(err(b.code, b.message));
        }
        if let Some((name, _)) = self.linked.iter().find(|(_, p)| p == canon) {
            return Err(err(ErrorCode::Exists, format!("this directory is already linked as project {name}")));
        }
        Ok(())
    }

    /// 把调用方给的路径解析成 canonical 路径，并要求它在 `link_roots` 之内。顺序：
    /// ① 词法规范化（去 `.` / `..`、系统别名、firmlink），**不在任何生效的 root 之内就一律 `forbidden`「不在 link_roots 之内」**，
    ///    不管 canonicalize 能不能成功、路径存不存在（经范围外的链接或 `/Volumes/<启动盘>` 拼进来的路径本来也不接受）；
    /// ② 按词法路径判断拒绝表 / 数据目录（命中就 `forbidden`，不管存不存在）；
    /// ③ canonicalize，结果必须在 root 之内；不存在统一是 `not_found`（`link` 里「不存在」和「不是目录」给同一个错误和消息）。
    pub fn canonicalize_within(&self, expanded: &Path) -> RpcResult<PathBuf> {
        let normalized = self.normalize(expanded);
        if !self.roots.lexically_within(&normalized, self.case_insensitive) {
            return Err(err(ErrorCode::Forbidden, OUTSIDE_ROOTS));
        }
        if let Some(b) = self.lexical_block(&normalized) {
            return Err(err(b.code, b.message));
        }
        match crate::sys::canonicalize(&normalized) {
            Ok(c) if self.roots.within(&c) => Ok(c),
            Ok(_) => Err(err(ErrorCode::Forbidden, OUTSIDE_ROOTS)),
            Err(e) => match e.kind() {
                std::io::ErrorKind::PermissionDenied => Err(err(ErrorCode::Forbidden, permission_message())),
                _ => Err(err(ErrorCode::NotFound, NOT_A_DIRECTORY)),
            },
        }
    }

    /// 规则 5：使用一个已关联的项目之前复核登记的路径。
    pub fn resolve_linked(&self, registered: &Path) -> Result<PathBuf, LinkedProblem> {
        let canon = crate::sys::canonicalize(registered).map_err(|_| LinkedProblem::Missing)?;
        if !canon.is_dir() {
            return Err(LinkedProblem::Missing);
        }
        if canon != registered {
            return Err(LinkedProblem::Moved);
        }
        match self.block_of(&canon) {
            Some(b) => Err(LinkedProblem::NotAllowed(b.block)),
            None => Ok(canon),
        }
    }

    /// 列目录时要不要跳过这个条目的 `.git` 探测（macOS 隐私授权目录和外接卷）。
    pub fn skips_git_probe(&self, canon: &Path) -> bool {
        self.tcc.iter().any(|t| t == canon) || canon.parent() == Some(Path::new("/Volumes"))
    }
}

/// `link` 里「不存在」和「不是目录」共用的消息（不让错误消息区分这两种）。
pub const NOT_A_DIRECTORY: &str = "no such directory";

/// 权限被拒（EACCES / EPERM）时的说明：macOS 的隐私授权要在那台机器上点一下。
pub fn permission_message() -> &'static str {
    "permission denied; on macOS this is usually a privacy restriction: allow anycon in System Settings > Privacy & Security (Files and Folders, or Full Disk Access) on that machine"
}

// 下面的测试用的是 unix 路径（`/h/.ssh`、`/etc`）；Windows 的规则在 `windows_tests`
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn ctx(roots: &[&str]) -> LinkCtx {
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        LinkCtx {
            roots: LinkRoots { raw: roots.clone(), canon: roots },
            root: PathBuf::from(r"C:\h\anycon"),
            data: PathBuf::from(r"C:\h\.anycon"),
            data_lexical: PathBuf::from(r"C:\h\.anycon"),
            root_ids: Identities::default(),
            data_ids: Identities::default(),
            aliases: PathAliases::default(),
            deny: deny_table(Some(Path::new(r"C:\h"))),
            tcc: tcc_table(Some(Path::new(r"C:\h"))),
            linked: Vec::new(),
            case_insensitive: true,
        }
    }

    fn block(c: &LinkCtx, p: &str) -> Option<LinkBlock> {
        c.block_of(&canonicalize_lenient(Path::new(p))).map(|b| b.block)
    }

    #[test]
    fn system_and_profile_directories_are_denied() {
        let c = ctx(&[r"C:\"]);
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        for p in [sysroot.clone(), format!(r"{sysroot}\System32")] {
            assert_eq!(block(&c, &p), Some(LinkBlock::Sensitive), "{p}");
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            assert_eq!(block(&c, &format!(r"{pf}\Vendor")), Some(LinkBlock::Sensitive));
        }
        for p in [
            r"C:\h\AppData",
            r"C:\h\AppData\Roaming\npm",
            r"C:\h\.ssh",
            r"C:\h\Documents\PowerShell",
            r"C:\h\Documents\WindowsPowerShell\profile.ps1",
        ] {
            assert_eq!(block(&c, p), Some(LinkBlock::Sensitive), "{p}");
        }
        assert_eq!(block(&c, r"C:\h\code\proj"), None);
        assert_eq!(block(&c, r"C:\h\Documents\GitHub\proj"), None);
        assert_eq!(block(&c, r"C:\"), Some(LinkBlock::FilesystemRoot));
    }

    #[test]
    fn allowing_appdata_still_denies_the_startup_folder() {
        let c = ctx(&[r"C:\h", r"C:\h\AppData"]);
        assert_eq!(block(&c, r"C:\h\AppData\Local\proj"), None);
        assert_eq!(
            block(&c, r"C:\h\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup"),
            Some(LinkBlock::Sensitive)
        );
    }

    #[test]
    fn tilde_expands_with_either_separator_and_drive_paths_are_absolute() {
        let h = Some(Path::new(r"C:\Users\me"));
        assert_eq!(expand_with_home("~", h).unwrap(), PathBuf::from(r"C:\Users\me"));
        assert_eq!(expand_with_home(r"~\code", h).unwrap(), PathBuf::from(r"C:\Users\me\code"));
        assert_eq!(expand_with_home("~/code", h).unwrap(), PathBuf::from(r"C:\Users\me\code"));
        assert_eq!(expand_with_home(r"D:\work", h).unwrap(), PathBuf::from(r"D:\work"));
        assert!(expand_with_home(r"\work", h).is_err(), "no drive letter: not absolute");
        assert!(expand_with_home("work", h).is_err());
        assert!(expand_with_home("~", None).is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ctx(roots: &[&str], linked: &[(&str, &str)]) -> LinkCtx {
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        LinkCtx {
            roots: LinkRoots { raw: roots.clone(), canon: roots },
            root: PathBuf::from("/h/anycon"),
            data: PathBuf::from("/h/.anycon"),
            data_lexical: PathBuf::from("/h/.anycon"),
            root_ids: Identities::default(),
            data_ids: Identities::default(),
            aliases: PathAliases::default(),
            deny: deny_table(Some(Path::new("/h"))),
            tcc: tcc_table(Some(Path::new("/h"))),
            linked: linked.iter().map(|(n, p)| (n.to_string(), PathBuf::from(p))).collect(),
            case_insensitive: false,
        }
    }

    /// 和生产一样先 canonicalize（macOS 上 `/etc` 是 `/private/etc`）；不存在的路径按存在的那一段处理。
    fn block(c: &LinkCtx, p: &str) -> Option<LinkBlock> {
        c.block_of(&canonicalize_lenient(Path::new(p))).map(|b| b.block)
    }

    #[test]
    fn hidden_segments_are_judged_relative_to_the_deepest_root_containing_the_path() {
        let c = ctx(&["/h"], &[]);
        assert_eq!(block(&c, "/h/work/proj"), None);
        assert_eq!(block(&c, "/h/work/.cache"), Some(LinkBlock::Hidden));
        assert_eq!(block(&c, "/h/work/.cache/x"), Some(LinkBlock::Hidden));
        assert_eq!(block(&c, "/h/.hidden"), Some(LinkBlock::Hidden));
        // 显式列出的隐藏目录：那一项之下的段不算
        let c = ctx(&["/h", "/h/.cache"], &[]);
        assert_eq!(block(&c, "/h/.cache"), None);
        assert_eq!(block(&c, "/h/.cache/x/y"), None);
        assert_eq!(block(&c, "/h/.cache/x/.deeper"), Some(LinkBlock::Hidden), "hidden below the root still counts");
        assert_eq!(block(&c, "/h/.other"), Some(LinkBlock::Hidden), "only the listed hidden directory is allowed");
        // root 本身在隐藏目录里（路径上游的点不算：只看 root 以下）
        let c = ctx(&["/h/.config/nvim"], &[]);
        assert_eq!(block(&c, "/h/.config/nvim/lua"), None);
        // 不在任何 root 里：隐藏规则不适用（已有关联在范围收紧之后照常用）
        let c = ctx(&["/data"], &[]);
        assert_eq!(block(&c, "/h/.cache/x"), None);
    }

    #[test]
    fn the_deny_table_blocks_equal_inside_and_containing_directories() {
        let c = ctx(&["/h"], &[]);
        for p in [
            "/h/.ssh",
            "/h/.ssh/keys",
            "/h/Library",
            "/h/Library/LaunchAgents",
            "/h/.config",
            "/h/.config/nvim",
            "/h/.local/share",
        ] {
            assert_eq!(block(&c, p), Some(LinkBlock::Sensitive), "{p}");
        }
        // `~/Library` 不是隐藏目录，仍然按拒绝表
        assert!(!c.block_of(Path::new("/h/Library")).unwrap().enterable);
        // 包含拒绝项的上级：拒绝，但可以浏览进去
        let c2 = ctx(&["/"], &[]);
        let b = c2.block_of(Path::new("/h")).unwrap();
        assert_eq!(b.block, LinkBlock::Workspace, "/h contains the workspace root and the data dir: reported first");
        let b = c2.block_of(Path::new("/usr")).unwrap();
        assert_eq!((b.block, b.enterable), (LinkBlock::Sensitive, false));
        let b = c2.block_of(Path::new("/usr/local/x")).unwrap();
        assert_eq!(b.block, LinkBlock::Sensitive);
        // 系统目录
        for p in ["/etc", "/etc/hosts.d", "/private/etc", "/System/Library", "/opt/x", "/var/lib/x", "/root/proj"] {
            assert_eq!(block(&c2, p), Some(LinkBlock::Sensitive), "{p}");
        }
        assert_eq!(block(&c2, "/srv/me/proj"), None);
        assert_eq!(block(&c2, "/"), Some(LinkBlock::FilesystemRoot));
    }

    #[test]
    fn an_explicit_root_inside_a_denied_directory_allows_only_what_is_below_it() {
        // `link_roots = ["~", "~/.config/nvim"]`：~/.config/nvim 可关联，~/.config 仍不可
        let c = ctx(&["/h", "/h/.config/nvim"], &[]);
        assert_eq!(block(&c, "/h/.config/nvim"), None);
        assert_eq!(block(&c, "/h/.config/nvim/lua"), None);
        assert_eq!(block(&c, "/h/.config"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/.config/other"), Some(LinkBlock::Sensitive));
        // `link_roots = ["~/.config"]`：~/.config 之下可以，更深的拒绝项（systemd、autostart）仍然生效，
        // 而 ~/.config 本身包含它们所以不行
        let c = ctx(&["/h/.config"], &[]);
        assert_eq!(block(&c, "/h/.config/nvim"), None);
        assert_eq!(block(&c, "/h/.config/systemd"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/.config/systemd/user"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/.config/autostart"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/.config"), Some(LinkBlock::Sensitive), "it contains the denied systemd directory");
        // 放行 ~/.config 不放行 ~/.ssh，也不放行上级目录
        assert_eq!(block(&c, "/h/.ssh"), Some(LinkBlock::Sensitive));
        // 例外只对「root 之下」生效：root 本身在拒绝项里，但路径在 root 之外就不算
        let c = ctx(&["/h", "/h/.config/nvim"], &[]);
        assert_eq!(block(&c, "/h/.config/nvim2"), Some(LinkBlock::Sensitive));
        // 系统临时目录下显式给的 root 照常可用（macOS 的 /private/var/folders/…）
        let c = ctx(&["/private/var/folders/xx/T/external"], &[]);
        assert_eq!(block(&c, "/private/var/folders/xx/T/external"), None);
        assert_eq!(block(&c, "/private/var/folders/xx/T/external/proj"), None);
        assert_eq!(block(&c, "/private/var/folders/xx/T"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/private/var/folders/xx/T/external-other"), Some(LinkBlock::Sensitive));
        // root 恰好等于拒绝项（Linux 的 root 用户：HOME = /root）：默认的 `~` 照常可用
        let mut c = ctx(&["/root"], &[]);
        c.deny = deny_table(Some(Path::new("/root")));
        c.root = PathBuf::from("/root/anycon");
        c.data = PathBuf::from("/root/.anycon");
        assert_eq!(block(&c, "/root/work"), None);
    }

    #[test]
    fn absolute_rules_cannot_be_unlocked() {
        // 即使 link_roots 显式写了
        let c = ctx(&["/h", "/h/anycon", "/h/.anycon", "/"], &[]);
        assert_eq!(block(&c, "/h/anycon/proj"), Some(LinkBlock::Workspace));
        assert_eq!(c.block_of(Path::new("/h/anycon/proj")).unwrap().code, ErrorCode::Invalid);
        assert_eq!(block(&c, "/h/anycon"), Some(LinkBlock::Workspace));
        assert_eq!(c.block_of(Path::new("/h/anycon")).unwrap().code, ErrorCode::Forbidden);
        assert_eq!(block(&c, "/h/.anycon"), Some(LinkBlock::DataDir));
        assert_eq!(block(&c, "/h/.anycon/keys"), Some(LinkBlock::DataDir));
        assert!(!c.block_of(Path::new("/h/.anycon")).unwrap().enterable);
        assert_eq!(block(&c, "/"), Some(LinkBlock::FilesystemRoot));
        // 包含数据目录的上级
        let c = ctx(&["/h/x"], &[]);
        let mut c = c;
        c.data = PathBuf::from("/h/x/data");
        let b = c.block_of(Path::new("/h/x")).unwrap();
        assert_eq!((b.block, b.enterable), (LinkBlock::DataDir, true));
    }

    #[test]
    fn verdict_orders_range_blocks_and_duplicates() {
        let c = ctx(&["/h"], &[("proj", "/h/work/proj")]);
        assert_eq!(c.verdict(Path::new("/etc")).unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
        assert!(c.verdict(Path::new("/h/work/other")).is_ok());
        assert_eq!(c.verdict(Path::new("/h/work/proj")).unwrap_err().stable_code(), Some(ErrorCode::Exists));
        assert_eq!(c.verdict(Path::new("/h/.ssh")).unwrap_err().stable_code(), Some(ErrorCode::Forbidden));
        assert_eq!(c.verdict(Path::new("/h/anycon/p")).unwrap_err().stable_code(), Some(ErrorCode::Invalid));
        let m = c.verdict(Path::new("/h/work/.cache")).unwrap_err().message;
        assert!(m.contains("hidden") && m.contains("link_roots"), "{m}");
        let m = c.verdict(Path::new("/h/Library/LaunchAgents")).unwrap_err().message;
        assert!(m.contains("protected") && m.contains("link_roots"), "{m}");
        // project_name：已关联的、根目录下的一级文件夹
        assert_eq!(c.project_name(Path::new("/h/work/proj")).as_deref(), Some("proj"));
        assert_eq!(c.project_name(Path::new("/h/anycon/mine")).as_deref(), Some("mine"));
        assert_eq!(c.project_name(Path::new("/h/anycon/.trash")), None);
        assert_eq!(c.project_name(Path::new("/h/anycon/mine/sub")), None);
        // 被关联项目盖住的根目录文件夹不算项目
        let c = ctx(&["/h"], &[("mine", "/h/work/mine")]);
        assert_eq!(c.project_name(Path::new("/h/anycon/mine")), None);
    }

    #[test]
    fn names_are_compared_after_nfc_and_case_folding_on_insensitive_volumes() {
        let nfc_cafe = "Caf\u{e9}";
        let nfd_cafe = "Cafe\u{301}";
        assert_ne!(nfc_cafe, nfd_cafe);
        assert_eq!(name_key(nfd_cafe, false), name_key(nfc_cafe, false));
        assert_eq!(nfc(nfd_cafe), nfc_cafe);
        assert_ne!(name_key("proj", false), name_key("PROJ", false), "case-sensitive volumes keep the case");
        assert_eq!(name_key("proj", true), name_key("PROJ", true));
        assert_eq!(name_key(nfd_cafe, true), name_key("CAF\u{c9}", true));
        let mut c = ctx(&["/h"], &[("proj", "/h/work/proj")]);
        c.case_insensitive = true;
        assert_eq!(c.project_name(Path::new("/h/anycon/PROJ")), None, "shadowed by the linked project");
    }

    #[test]
    fn canonical_tables_follow_the_symlinks_of_the_home_directory() {
        let d = tempfile::tempdir().unwrap();
        let real = crate::sys::canonicalize(d.path()).unwrap().join("real-home");
        std::fs::create_dir_all(real.join(".ssh")).unwrap();
        let link = real.parent().unwrap().join("home-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // HOME 是符号链接：词法路径保留链接写法，canonical 路径按真实的 HOME 展开（存在的和还不存在的项都是）
        let t = deny_table(Some(&link));
        let ssh = t.iter().find(|d| d.canon == real.join(".ssh")).expect("~/.ssh is in the table");
        assert_eq!(ssh.lexical, link.join(".ssh"));
        assert!(
            t.iter().any(|d| d.canon == real.join(".gnupg")),
            "a denied directory that does not exist yet is still listed: {t:?}"
        );
        assert!(t.iter().filter(|d| d.lexical.starts_with(&link)).all(|d| !d.canon.starts_with(&link)), "{t:?}");
        assert!(tcc_table(Some(&link)).contains(&real.join("Documents")));
        // 没有 HOME：只有绝对路径的项
        assert!(deny_table(None).iter().any(|d| d.canon.ends_with("etc") || d.canon.ends_with("private/etc")));
        assert!(deny_table(None).iter().all(|d| d.canon.is_absolute() && d.lexical.is_absolute()));
        assert_eq!(canonicalize_lenient(&real.join("a/b/../c")), real.join("a/c"));
    }

    #[test]
    fn resolve_linked_requires_the_registered_path_and_the_current_rules() {
        let d = tempfile::tempdir().unwrap();
        let base = crate::sys::canonicalize(d.path()).unwrap();
        let proj = base.join("work/proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(base.join("data/keys")).unwrap();
        let mut c = ctx(&[base.to_str().unwrap()], &[]);
        c.root = base.join("ws");
        c.data = base.join("data");
        c.deny = vec![];
        assert_eq!(c.resolve_linked(&proj), Ok(proj.clone()));
        // 被换成指向数据目录的符号链接：canonical 不等于登记的路径
        std::fs::remove_dir_all(&proj).unwrap();
        std::os::unix::fs::symlink(base.join("data"), &proj).unwrap();
        assert_eq!(c.resolve_linked(&proj), Err(LinkedProblem::Moved));
        // 即使登记的路径本身经过符号链接的上级
        let up = base.join("up");
        std::os::unix::fs::symlink(base.join("work"), &up).unwrap();
        assert_eq!(c.resolve_linked(&up.join("proj")), Err(LinkedProblem::Moved));
        // 目录没了 / 变成文件
        std::fs::remove_file(&proj).unwrap();
        assert_eq!(c.resolve_linked(&proj), Err(LinkedProblem::Missing));
        std::fs::write(&proj, "x").unwrap();
        assert_eq!(c.resolve_linked(&proj), Err(LinkedProblem::Missing));
        // 登记的路径本身就是数据目录（比如数据目录后来被移到这里）：规则 4 复核不过
        assert_eq!(c.resolve_linked(&base.join("data/keys")), Err(LinkedProblem::NotAllowed(LinkBlock::DataDir)));
        let m = LinkedProblem::Moved.describe("p");
        assert!(m.contains("symbolic link") && !m.contains(base.to_str().unwrap()), "{m}");
    }

    #[test]
    fn the_deny_table_blocks_launch_agents_keychains_bin_and_tool_configs() {
        let c = ctx(&["/h"], &[]);
        for p in [
            "/h/Library/LaunchAgents",
            "/h/Library/LaunchAgents/com.x",
            "/h/Library/Keychains",
            "/h/bin",
            "/h/bin/tool",
            "/h/go/bin",
            "/h/.config/gh",
            "/h/.config/fish",
            "/h/.config/fish/conf.d",
        ] {
            assert_eq!(block(&c, p), Some(LinkBlock::Sensitive), "{p}");
        }
        // 它们的上级 `~/go` 包含 `~/go/bin`，也不能关联；`~/go/src/x` 可以
        assert_eq!(block(&c, "/h/go"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/go/src/x"), None);
        // 显式放行 ~/Library 不放行 ~/Library/LaunchAgents 和 Keychains
        let c = ctx(&["/h/Library"], &[]);
        assert_eq!(block(&c, "/h/Library/Application Support"), None);
        assert_eq!(block(&c, "/h/Library/LaunchAgents"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/Library/Keychains"), Some(LinkBlock::Sensitive));
        // 显式放行 ~/.config 不放行 gh / fish
        let c = ctx(&["/h/.config"], &[]);
        assert_eq!(block(&c, "/h/.config/nvim"), None);
        assert_eq!(block(&c, "/h/.config/gh"), Some(LinkBlock::Sensitive));
        assert_eq!(block(&c, "/h/.config/fish"), Some(LinkBlock::Sensitive));
        // `link_roots = ["/"]` 时 /Applications 不能关联
        let c = ctx(&["/"], &[]);
        if Path::new("/Applications").exists() {
            assert_eq!(block(&c, "/Applications"), Some(LinkBlock::Sensitive));
            assert_eq!(block(&c, "/Applications/Foo.app"), Some(LinkBlock::Sensitive));
        }
        assert!(deny_table(None).iter().any(|d| d.lexical == Path::new("/Applications")));
    }

    /// 系统别名：词法比较之前 `/var/…` 换成 `/private/var/…`，所以运维和用户写短的那个也能被显式放行的 root 罩住，
    /// 而且拒绝表按同一组别名匹配。
    #[test]
    fn system_aliases_are_normalized_before_lexical_comparison() {
        let mut c = ctx(&["/private/var/folders/xx/T/external"], &[]);
        c.deny = deny_table(None);
        c.aliases.prefixes = vec![
            (PathBuf::from("/var"), PathBuf::from("/private/var")),
            (PathBuf::from("/tmp"), PathBuf::from("/private/tmp")),
        ];
        // 用短写法探测：在显式放行的 root 之下，不被拒绝表（/private、/var）挡在词法这一层
        assert!(c.lexical_block(Path::new("/var/folders/xx/T/external/proj")).is_none());
        assert!(c.lexical_block(Path::new("/private/var/folders/xx/T/external/proj")).is_none());
        // 同一个别名下的别的位置照样被挡
        assert!(c.lexical_block(Path::new("/var/folders/xx/T/other")).is_some());
        assert!(c.lexical_block(Path::new("/tmp/x")).is_some(), "/tmp is /private/tmp, inside the denied /private");
        // normalize_aliases 只认列出的别名
        let al = &c.aliases.prefixes;
        assert_eq!(normalize_aliases(Path::new("/var/a/../b"), al), PathBuf::from("/private/var/b"));
        assert_eq!(normalize_aliases(Path::new("/tmp"), al), PathBuf::from("/private/tmp"));
        assert_eq!(normalize_aliases(Path::new("/varnish"), al), PathBuf::from("/varnish"), "not a path prefix");
        assert_eq!(normalize_aliases(Path::new("/home/x"), al), PathBuf::from("/home/x"));
        // 这台机器上的别名：要么是空的（Linux），要么每一项真的是符号链接
        for (alias, real) in system_aliases() {
            assert_eq!(crate::sys::canonicalize(&alias).unwrap(), real);
            assert_ne!(alias, real);
        }
    }

    /// 按词法路径判断，命中拒绝表 / 数据目录就拒绝，不管路径存不存在、是不是目录。
    #[test]
    fn lexical_paths_inside_denied_locations_are_refused_whether_or_not_they_exist() {
        let c = ctx(&["/h"], &[]);
        for p in [
            "/h/.ssh",
            "/h/.ssh/id_ed25519",
            "/h/.ssh/nope",
            "/h/.aws/credentials",
            "/h/Library/Keychains/login.keychain-db",
        ] {
            let b = c.lexical_block(Path::new(p)).unwrap_or_else(|| panic!("{p} should be refused"));
            assert_eq!((b.block, b.code), (LinkBlock::Sensitive, ErrorCode::Forbidden), "{p}");
        }
        // 路径里的 `..` 先被词法规整
        assert!(c.lexical_block(Path::new("/h/work/../.ssh/nope")).is_some());
        // 数据目录
        let b = c.lexical_block(Path::new("/h/.anycon/keys/node.key")).unwrap();
        assert_eq!(b.block, LinkBlock::DataDir);
        // 普通路径、或者被显式放行的 root 之下：不在这一层拒绝
        assert!(c.lexical_block(Path::new("/h/work/nope")).is_none());
        let c = ctx(&["/h", "/h/.config/nvim"], &[]);
        assert!(c.lexical_block(Path::new("/h/.config/nvim/lua/x")).is_none());
        assert!(c.lexical_block(Path::new("/h/.config/other")).is_some());
        // `canonicalize_within`：同一类路径的两种探测（存在的文件 / 不存在的名字）得到完全相同的结果
        let c = ctx(&["/h"], &[]);
        let a = c.canonicalize_within(Path::new("/h/.ssh/id_ed25519")).unwrap_err();
        let b = c.canonicalize_within(Path::new("/h/.ssh/nope")).unwrap_err();
        assert_eq!((a.stable_code(), &a.message), (b.stable_code(), &b.message));
        assert_eq!(a.stable_code(), Some(ErrorCode::Forbidden));
    }

    /// 「包含拒绝项」的消息点名一个确实存在的拒绝项。
    #[test]
    fn a_containing_directory_names_a_denied_location_that_exists() {
        let d = tempfile::tempdir().unwrap();
        let home = crate::sys::canonicalize(d.path()).unwrap().join("home");
        std::fs::create_dir_all(home.join(".kube")).unwrap(); // 只有 .kube 存在
        let mut c = ctx(&[home.to_str().unwrap()], &[]);
        c.deny = deny_table(Some(&home));
        c.root = PathBuf::from("/nowhere/anycon");
        c.data = PathBuf::from("/nowhere/.anycon");
        c.data_lexical = c.data.clone();
        let b = c.block_of(&home).unwrap();
        assert_eq!((b.block, b.enterable), (LinkBlock::Sensitive, true));
        assert!(b.message.contains(".kube"), "names the denied directory that exists: {}", b.message);
        assert!(!b.message.contains(".aws"), "{}", b.message);
        let err = c.verdict(&home).unwrap_err();
        assert!(err.message.contains(".kube"), "{}", err.message);
    }

    /// 词法比较的键——NFC 总是做，小写折叠只在不区分大小写的卷上做；范围检查先于一切，用同一个键。
    #[test]
    fn folded_comparison_uses_nfc_always_and_case_only_on_insensitive_volumes() {
        let nfc_cafe = Path::new("/h/Caf\u{e9}/x");
        let nfd_cafe = Path::new("/h/Cafe\u{301}/x");
        assert_ne!(nfc_cafe, nfd_cafe);
        assert_eq!(fold_path(nfd_cafe, false), fold_path(nfc_cafe, false));
        assert!(starts_with_folded(nfd_cafe, Path::new("/h/Caf\u{e9}"), false));
        assert!(
            !starts_with_folded(Path::new("/h/WORK/x"), Path::new("/h/work"), false),
            "case-sensitive keeps the case"
        );
        assert!(starts_with_folded(Path::new("/h/WORK/x"), Path::new("/h/work"), true));
        assert!(!starts_with_folded(Path::new("/h/workshop"), Path::new("/h/work"), true), "whole path segments only");
        let roots = LinkRoots { raw: vec![PathBuf::from("/h/work")], canon: vec![PathBuf::from("/h/work")] };
        assert!(roots.lexically_within(Path::new("/h/work/a/../b"), false));
        assert!(!roots.lexically_within(Path::new("/h/work/../other"), false));
        assert!(!roots.lexically_within(Path::new("/h/WORK/b"), false));
        assert!(roots.lexically_within(Path::new("/h/WORK/b"), true));
        // 取消 `..` 之后才比较：不能靠 `..` 溜出去
        assert!(!roots.lexically_within(Path::new("/h/work/../../etc"), true));
        // canonicalize_within：范围外的写法不管存不存在都是同一句话
        let c = ctx(&["/h/work"], &[]);
        for p in ["/etc/hosts", "/etc/nope", "/h/other", "/h/.ssh/id_x", "/nowhere/at/all"] {
            let e = c.canonicalize_within(Path::new(p)).unwrap_err();
            assert_eq!((e.stable_code(), e.message.as_str()), (Some(ErrorCode::Forbidden), OUTSIDE_ROOTS), "{p}");
        }
    }
}
