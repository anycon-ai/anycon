//! 附件落到会话工作目录（[ADR-44](../../../docs/decisions.md)，docs/spec/daemon.md § 7.10）。
//!
//! agent 不原生收的附件（所有非图片文件，以及 agent 不收的图片格式）由 daemon 复制到
//! `<会话工作目录>/.anycon/attachments/<session_id>/<文件名>`，再在交给 adapter 的文字末尾追加一段路径清单（[`note`]），
//! 所以任何 agent、任何文件类型都能用，adapter 不需要知道这件事。
//!
//! 安全约束（和 worktree 目录同一套，ADR-34 第 9 条）：
//! - `.anycon`、`attachments`、`<session_id>` 每一级都必须是真目录，符号链接 / 文件一律拒绝（agent 能写工作目录，可以把其中任何
//!   一级换成指向别处的链接，而 daemon 不在沙箱里）；新建的 `attachments` 和 `<session_id>` 是 0700、文件 0600（Windows 上是受保护的 DACL）；
//! - **unix**：从边界目录（项目目录 / scratch 目录）起逐级 `openat(O_DIRECTORY | O_NOFOLLOW)`，缺的一级 `mkdirat` 后再按 `O_NOFOLLOW`
//!   打开，文件用 `openat(dirfd, 名字, O_CREAT | O_EXCL | O_NOFOLLOW)` 建在拿到的目录 fd 里（`fsopen::open_dir_chain` /
//!   `create_new_file_at`）：没有「先检查路径、再按路径打开」的窗口。**Windows** 没有对应的相对句柄创建，仍是逐级 `lstat` 之后按路径
//!   `create_new`，检查和创建之间有一个很短的窗口（agent 恰好在这一刻把某一级换成指向别处的链接，文件会建到那里；内容和名字都是
//!   用户给的，不会覆盖已有文件），ADR-44 里写明；
//! - 重名加 ` (2)` 之类的后缀；
//! - 文件名来自客户端：控制字符等在上传时已被拒绝（`blobs::validate_name`），这里再把 `.` / `..`、Windows 不允许的字符和保留名
//!   换成安全的写法，所以不会逃出目录；
//! - 工作目录在 git 仓库里（项目会话；scratch 目录不处理）时，确保 `<git common dir>/info/exclude` 有一行排除 `.anycon/`，
//!   免得 agent 的 `git add -A` 把用户传的文件提交进去；写不进去就拒绝落盘，不留下没有被排除的文件。
//!
//! 删除（[`remove_session_dir`]）：unix 上同样从边界目录起逐级 `openat(O_NOFOLLOW)`，再相对目录 fd 递归 `unlinkat`
//! （里面的符号链接只删链接本身）；Windows 上逐级 `lstat` 之后 `remove_dir_all`。只删 `<session_id>` 这一个目录。

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::blobs::Attached;
use crate::gitops::{Git, GitError};
#[cfg(not(unix))]
use crate::sys::{restrict_to_owner, OwnerOnly};
use crate::util::human_bytes;
use crate::worktree::add_exclude_line;
#[cfg(not(unix))]
use crate::worktree::ensure_real_dirs;

/// 工作目录下放附件的相对路径（`<session_id>` 再往下一级）。
pub const ATTACH_REL: [&str; 2] = [".anycon", "attachments"];
/// 追加在文字末尾的清单标题（英文：给 agent 看）。
pub const NOTE_HEADING: &str = "[Attached files, saved in the working directory]";
/// 同名文件最多试到 ` (N)` 的 N。
const MAX_SUFFIX: u32 = 1000;
/// 文件名的字节上限（和 `blob.put_begin` 的 `name` 一致）。
const NAME_MAX: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    /// `.anycon/attachments/<session_id>` 建不出来：某一级是符号链接 / 文件，或没有权限。
    #[error("cannot prepare the attachments directory: {0}")]
    Dir(String),
    /// 写文件失败（磁盘满、目录不可写、blob 文件读不了…）。
    #[error("cannot save {name} in the working directory: {source}")]
    Write { name: String, source: io::Error },
    /// 没能确保 `info/exclude` 排除 `.anycon/`。
    #[error("cannot keep .anycon/ out of git: {0}")]
    Exclude(String),
    #[error("cannot save attachments: {0}")]
    Task(String),
}

/// 一个已经落到工作目录的附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// 相对工作目录的路径，永远用 `/` 分隔（`.anycon/attachments/<session_id>/report.pdf`）。
    pub rel: String,
    pub mime: String,
    pub bytes: u64,
}

/// 落盘和清理。`placed` 记着「哪个 blob 已经放在了哪里」：同一个 blob 再次要落盘（插话没送进去、退回队列后下一个 turn 又来；
/// 两条消息引用同一个 blob）时沿用已有的文件，不再复制出一份 ` (2)`。只在内存里：重启之后队列本来就没了。
#[derive(Default)]
pub struct WorkFiles {
    placed: Arc<Mutex<HashMap<String, String>>>,
}

impl WorkFiles {
    pub fn new() -> WorkFiles {
        WorkFiles::default()
    }

    /// 把 `files` 复制进 `<cwd>/.anycon/attachments/<session_id>/`，按输入的顺序返回它们的位置。
    /// `boundary` 是 `cwd` 所在的、不会被 agent 换掉的目录（项目目录；scratch 会话就是 `cwd` 自己）：unix 上从它起逐级打开 `cwd` 和
    /// 后面的三级，`cwd` 必须在它之下。`git` 为 `None`（scratch 会话）时不碰 `info/exclude`。任何一个失败都返回错误；
    /// 已经复制好的文件留在目录里（随会话删除，也不会被下一次重试重复复制）。
    pub async fn place(
        &self,
        git: Option<&Git>,
        boundary: &Path,
        cwd: &Path,
        session_id: &str,
        files: Vec<Attached>,
    ) -> Result<Vec<Placed>, PlaceError> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        if let Some(git) = git {
            ensure_excluded(git, cwd).await?;
        }
        let (boundary, cwd, sid, placed) =
            (boundary.to_path_buf(), cwd.to_path_buf(), session_id.to_string(), self.placed.clone());
        tokio::task::spawn_blocking(move || place_blocking(&placed, &boundary, &cwd, &sid, &files))
            .await
            .map_err(|e| PlaceError::Task(e.to_string()))?
    }

    /// 会话删除时忘掉它的记录（`blob_files` 是会话的 blob 在 `blobs/` 里的文件名 `<blob_id>.<ext>`）。
    pub fn forget(&self, blob_files: &[String]) {
        let mut m = self.placed.lock().unwrap();
        for f in blob_files {
            m.remove(f.split('.').next().unwrap_or(f));
        }
    }
}

fn place_blocking(
    placed: &Mutex<HashMap<String, String>>,
    boundary: &Path,
    cwd: &Path,
    session_id: &str,
    files: &[Attached],
) -> Result<Vec<Placed>, PlaceError> {
    let mut out = Vec::with_capacity(files.len());
    let mut dest: Option<Dest> = None;
    for f in files {
        let r = &f.reference;
        // 已经放过：文件还在、大小对得上就沿用（只读的 `lstat`，不写任何东西）
        let known = placed.lock().unwrap().get(&r.blob_id).cloned();
        if let Some(rel) = known {
            let p = cwd.join(&rel);
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && m.len() == r.bytes) {
                out.push(Placed { rel, mime: r.mime.clone(), bytes: r.bytes });
                continue;
            }
        }
        let dest = match &dest {
            Some(d) => d,
            None => dest.insert(Dest::open(boundary, cwd, session_id)?),
        };
        let name = copy_unique(dest, &r.name, &f.path, r.bytes)
            .map_err(|source| PlaceError::Write { name: r.name.clone(), source })?;
        let rel = format!("{}/{}/{session_id}/{name}", ATTACH_REL[0], ATTACH_REL[1]);
        placed.lock().unwrap().insert(r.blob_id.clone(), rel.clone());
        out.push(Placed { rel, mime: r.mime.clone(), bytes: r.bytes });
    }
    Ok(out)
}

/// 放附件的目录 `<cwd>/.anycon/attachments/<session_id>`。unix 上持有它的目录 fd：之后建文件都相对这个 fd，路径上的某一级
/// 之后被换成什么都不再影响文件落在哪里；Windows 上只有路径（见模块说明里的窗口）。
struct Dest {
    /// 目录的路径：报错、测试钩子、Windows 上建文件用。
    path: PathBuf,
    #[cfg(unix)]
    dir: File,
}

impl Dest {
    /// 逐级确认 `.anycon`、`attachments`、`<session_id>` 是真目录，没有就建（后两级 0700，`.anycon` 和 worktree 共用、按平时的权限）。
    #[cfg(unix)]
    fn open(boundary: &Path, cwd: &Path, session_id: &str) -> Result<Dest, PlaceError> {
        use std::ffi::OsStr;
        let create: [(&OsStr, Option<u32>); 3] = [
            (OsStr::new(ATTACH_REL[0]), Some(0o777)),
            (OsStr::new(ATTACH_REL[1]), Some(0o700)),
            (OsStr::new(session_id), Some(0o700)),
        ];
        let dir =
            crate::fsopen::open_dir_chain(boundary, cwd, &create).map_err(|e| PlaceError::Dir(chain_message(&e)))?;
        Ok(Dest { path: cwd.join(ATTACH_REL[0]).join(ATTACH_REL[1]).join(session_id), dir })
    }

    #[cfg(not(unix))]
    fn open(_boundary: &Path, cwd: &Path, session_id: &str) -> Result<Dest, PlaceError> {
        let path = ensure_real_dirs(cwd, &[ATTACH_REL[0], ATTACH_REL[1], session_id], &|p, i| {
            // `.anycon` 自己和 worktree 共用，按平时的权限；放附件的两级只有属主能进
            if i >= 1 {
                restrict_to_owner(p)
            } else {
                Ok(())
            }
        })
        .map_err(PlaceError::Dir)?;
        Ok(Dest { path })
    }

    /// 新建文件 `name`（0600）：已经有任何东西（包括符号链接）都失败，不会覆盖、不会写穿链接。
    fn create_new(&self, name: &str) -> io::Result<File> {
        // 测试钩子：在这一刻换目录（unix 上不影响结果：文件落在 fd 指的那个目录里）
        crate::fsopen::before_open(&self.path.join(name));
        #[cfg(unix)]
        {
            crate::fsopen::create_new_file_at(&self.dir, std::ffi::OsStr::new(name), 0o600)
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new().write(true).create_new(true).owner_only().open(self.path.join(name))
        }
    }

    /// 删掉刚建的文件（复制失败时）。unix 上相对目录 fd，Windows 上按路径。
    fn remove_file(&self, name: &str) {
        #[cfg(unix)]
        {
            let _ = crate::fsopen::unlink_file_at(&self.dir, std::ffi::OsStr::new(name));
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(self.path.join(name));
        }
    }
}

/// [`crate::fsopen::open_dir_chain`] 的失败说成人话（带出问题的那一级的路径）。
#[cfg(unix)]
fn chain_message(e: &crate::fsopen::ChainError) -> String {
    use crate::fsopen::OpenError;
    match &e.error {
        OpenError::Escaped => {
            format!("{} is a symbolic link (or was replaced by one); refusing to write through it", e.at.display())
        }
        OpenError::Io(io) if io.raw_os_error() == Some(libc::ENOTDIR) => {
            format!("{} exists and is not a directory", e.at.display())
        }
        OpenError::Io(io) => format!("cannot open {}: {io}", e.at.display()),
        OpenError::WrongKind => format!("{} is not a directory", e.at.display()),
    }
}

/// 把 `src` 复制成 `dest` 里的一个新文件，名字取自 `wanted`，重名依次试 `<主名> (2).<扩展名>`…；返回实际用的文件名。
fn copy_unique(dest: &Dest, wanted: &str, src: &Path, expect_bytes: u64) -> io::Result<String> {
    let base = safe_file_name(wanted, cfg!(windows));
    for n in 1..=MAX_SUFFIX {
        let name = if n == 1 { base.clone() } else { with_suffix(&base, n) };
        let mut out = match dest.create_new(&name) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let copied = File::open(src).and_then(|mut input| io::copy(&mut input, &mut out)).and_then(|n| {
            if n == expect_bytes {
                Ok(())
            } else {
                Err(io::Error::other(format!("the stored attachment has {n} bytes, expected {expect_bytes}")))
            }
        });
        if let Err(e) = copied {
            drop(out);
            dest.remove_file(&name);
            return Err(e);
        }
        return Ok(name);
    }
    Err(io::Error::other(format!("there are already {MAX_SUFFIX} files with this name")))
}

/// 放进工作目录的文件名：在已经合法的展示名（无 `/` `\`、控制字符、双向控制符）基础上，`.` / `..` 换成 `file`；
/// `strict`（Windows）还要把 `< > : " | ? *` 换成 `_`、去掉结尾的 `.` 和空格、给保留设备名（`CON`、`NUL`、`COM1`…）加 `_` 前缀
/// （`a:b` 在 NTFS 上会写进备用数据流，保留名会打开设备）。结果不超过 255 字节（在字符边界上截断）。
pub fn safe_file_name(name: &str, strict: bool) -> String {
    let mut s: String = name.to_string();
    if strict {
        s = s.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c }).collect();
        s = s.trim_end_matches(['.', ' ']).to_string();
        let stem = s.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if reserved {
            s.insert(0, '_');
        }
    }
    if s.is_empty() || s == "." || s == ".." {
        s = "file".to_string();
    }
    truncate_name(&s, NAME_MAX)
}

/// 在扩展名之前加 ` (n)`：`report.pdf` → `report (2).pdf`；没有扩展名（或只有点前缀的 `.env`）加在最后。总长不超过 255 字节。
fn with_suffix(name: &str, n: u32) -> String {
    let suffix = format!(" ({n})");
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let room = NAME_MAX.saturating_sub(suffix.len() + ext.len());
    format!("{}{suffix}{ext}", truncate_name(stem, room))
}

fn truncate_name(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// 交给 adapter 的文字末尾追加的一段：标题加每个文件一行 `- <相对路径> (<mime>, <大小>)`。
/// `after_text` 为 false（消息没有文字）时不带开头的空行。
pub fn note(placed: &[Placed], after_text: bool) -> String {
    if placed.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    if after_text {
        s.push_str("\n\n");
    }
    s.push_str(NOTE_HEADING);
    for p in placed {
        s.push_str(&format!("\n- {} ({}, {})", p.rel, p.mime, human_bytes(p.bytes)));
    }
    s
}

/// 要落盘的附件：不在 agent 原生收的 mime 里的。
pub fn needs_placing(mime: &str, native: &[String]) -> bool {
    !native.iter().any(|m| m == mime)
}

// ---------------------------------------------------------------- info/exclude

/// 工作目录在 git 仓库里时，确保 `<git common dir>/info/exclude` 里有排除 `<cwd>/.anycon/` 的一行；不在仓库里（或没有 `git`）什么都不做。
/// 工作目录是仓库顶层时这一行就是 worktree 用的 `/.anycon/`；是仓库的子目录时带上相对仓库顶层的前缀（`/sub/dir/.anycon/`）。
async fn ensure_excluded(git: &Git, cwd: &Path) -> Result<(), PlaceError> {
    let out = match git.at(cwd).args(["rev-parse", "--git-common-dir", "--show-prefix"]).secs(10).run().await {
        Ok(o) => o,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e @ GitError::Timeout { .. }) => return Err(PlaceError::Exclude(e.to_string())),
        Err(e) => return Err(PlaceError::Exclude(e.to_string())),
    };
    // 不是仓库（退出码 128）：没有什么可排除的
    if !out.success() {
        return Ok(());
    }
    let text = out.text();
    let mut lines = text.lines();
    let Some(common) = lines.next().filter(|l| !l.is_empty()) else { return Ok(()) };
    let prefix = lines.next().unwrap_or("");
    let common = if Path::new(common).is_absolute() { PathBuf::from(common) } else { cwd.join(common) };
    add_exclude_line(common, exclude_line(prefix)).await.map_err(PlaceError::Exclude)
}

/// `info/exclude` 里排除 `<仓库顶层>/<prefix>.anycon/` 的一行。`prefix` 是 `git rev-parse --show-prefix`（空或以 `/` 结尾）；
/// gitignore 的通配符和转义字符在前缀里要加反斜杠。
pub fn exclude_line(prefix: &str) -> String {
    let mut s = String::from("/");
    for c in prefix.chars() {
        if matches!(c, '\\' | '*' | '?' | '[') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push_str(".anycon/");
    s
}

// ---------------------------------------------------------------- 删除

/// 删 `<cwd>/.anycon/attachments/<session_id>/`（只删这个目录）。`boundary` 同 [`WorkFiles::place`]。
/// unix：从 `boundary` 起逐级 `openat(O_NOFOLLOW)` 到 `attachments`，再相对目录 fd 删 `<session_id>` 和里面的一切
/// （里面的符号链接只删链接本身）；任何一级是符号链接或不是目录就一个都不删。Windows：逐级 `lstat` 之后 `remove_dir_all`。
/// 目录不存在不算错。返回是否真的删了东西。
#[cfg(unix)]
pub fn remove_session_dir(boundary: &Path, cwd: &Path, session_id: &str) -> io::Result<bool> {
    use std::ffi::OsStr;
    let create: [(&OsStr, Option<u32>); 2] = [(OsStr::new(ATTACH_REL[0]), None), (OsStr::new(ATTACH_REL[1]), None)];
    let parent = match crate::fsopen::open_dir_chain(boundary, cwd, &create) {
        Ok(f) => f,
        Err(e) if e.is_not_found() => return Ok(false),
        Err(e) => return Err(io::Error::other(chain_message(&e))),
    };
    crate::fsopen::remove_dir_at(&parent, OsStr::new(session_id))
}

#[cfg(not(unix))]
pub fn remove_session_dir(_boundary: &Path, cwd: &Path, session_id: &str) -> io::Result<bool> {
    let mut cur = cwd.to_path_buf();
    for seg in [ATTACH_REL[0], ATTACH_REL[1], session_id] {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::other(format!("{} is not a plain directory; leaving it alone", cur.display())))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    std::fs::remove_dir_all(&cur)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::AttachmentRef;

    fn attached(dir: &Path, id: &str, name: &str, mime: &str, bytes: &[u8]) -> Attached {
        let path = dir.join(format!("{id}.bin"));
        std::fs::write(&path, bytes).unwrap();
        Attached {
            reference: AttachmentRef {
                blob_id: id.into(),
                name: name.into(),
                mime: mime.into(),
                bytes: bytes.len() as u64,
                width: None,
                height: None,
            },
            path,
        }
    }

    #[test]
    fn safe_names_cannot_escape_the_directory() {
        for (input, want) in
            [("report.pdf", "report.pdf"), ("..", "file"), (".", "file"), ("", "file"), ("..a", "..a"), ("a..", "a..")]
        {
            assert_eq!(safe_file_name(input, false), want, "{input:?}");
        }
        // 字节上限：在字符边界上截断
        let long = "图".repeat(100);
        let s = safe_file_name(&long, false);
        assert!(s.len() <= 255 && s.chars().all(|c| c == '图'), "{}", s.len());
    }

    #[test]
    fn windows_names_lose_the_characters_ntfs_would_misread() {
        for (input, want) in [
            ("a:b.txt", "a_b.txt"),
            ("what?.txt", "what_.txt"),
            ("trailing.", "trailing"),
            ("trailing ", "trailing"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("com1.log", "_com1.log"),
            ("COM0", "COM0"),
            ("console.txt", "console.txt"),
            ("...", "file"),
        ] {
            assert_eq!(safe_file_name(input, true), want, "{input:?}");
        }
        // 非严格模式一个字符都不改
        assert_eq!(safe_file_name("a:b?.txt", false), "a:b?.txt");
    }

    #[test]
    fn suffixes_go_before_the_extension_and_keep_the_name_within_the_limit() {
        assert_eq!(with_suffix("report.pdf", 2), "report (2).pdf");
        assert_eq!(with_suffix("archive.tar.gz", 3), "archive.tar (3).gz");
        assert_eq!(with_suffix("README", 2), "README (2)");
        assert_eq!(with_suffix(".env", 2), ".env (2)");
        let long = format!("{}.txt", "x".repeat(251));
        let s = with_suffix(&long, 12);
        assert_eq!(s.len(), 255);
        assert!(s.ends_with(" (12).txt"));
    }

    #[test]
    fn the_note_lists_each_file_with_its_type_and_size() {
        let p = |rel: &str, mime: &str, bytes: u64| Placed { rel: rel.into(), mime: mime.into(), bytes };
        assert_eq!(note(&[], true), "");
        assert_eq!(
            note(&[p(".anycon/attachments/sess_x/report.pdf", "application/pdf", 122_880)], true),
            "\n\n[Attached files, saved in the working directory]\n- .anycon/attachments/sess_x/report.pdf (application/pdf, 120.0 KiB)"
        );
        assert_eq!(
            note(&[p("a", "text/plain", 12), p("b (2)", "image/png", 3 * 1024 * 1024)], false),
            "[Attached files, saved in the working directory]\n- a (text/plain, 12 B)\n- b (2) (image/png, 3.0 MiB)"
        );
    }

    #[test]
    fn exclude_lines_are_anchored_to_the_repository_top() {
        assert_eq!(exclude_line(""), "/.anycon/");
        assert_eq!(exclude_line("sub/dir/"), "/sub/dir/.anycon/");
        assert_eq!(exclude_line("a*b/[x]/"), "/a\\*b/\\[x]/.anycon/");
    }

    /// 不读开发机的 git 配置：全局配置指向临时目录里的空文件。
    fn git_config(tmp: &Path) -> PathBuf {
        let p = tmp.join("gitconfig");
        std::fs::write(&p, b"").unwrap();
        p
    }

    fn git_env(config: &Path) -> Arc<HashMap<String, String>> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        env.insert("GIT_CONFIG_GLOBAL".into(), config.to_string_lossy().into_owned());
        Arc::new(env)
    }

    fn git(dir: &Path, config: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 工作目录是仓库的子目录：排除的是这个子目录里的 `.anycon/`（带相对仓库顶层的前缀），`git status` 看不到它；
    /// 不在仓库里的目录什么都不写。
    #[tokio::test]
    async fn the_exclude_line_follows_the_working_directory_inside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let config = git_config(tmp.path());
        let repo = tmp.path().join("repo");
        let sub = repo.join("pkg one").join("app");
        std::fs::create_dir_all(&sub).unwrap();
        git(&repo, &config, &["init", "-q"]);
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        let wf = WorkFiles::new();
        let g = Git::new(git_env(&config));
        let placed = wf
            .place(Some(&g), &sub, &sub, "sess_1", vec![attached(&blobs, "a", "a.txt", "text/plain", b"hi")])
            .await
            .unwrap();
        assert_eq!(placed[0].rel, ".anycon/attachments/sess_1/a.txt");
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l == "/pkg one/app/.anycon/"), "{exclude}");
        assert_eq!(git(&repo, &config, &["status", "--porcelain", "-uall"]), "");
        // 同一个目录再来一次：不重复写
        wf.place(Some(&g), &sub, &sub, "sess_2", vec![attached(&blobs, "b", "b.txt", "text/plain", b"hi")])
            .await
            .unwrap();
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "/pkg one/app/.anycon/").count(), 1);

        // 不是仓库：照常落盘，不报错，也不会凭空建出 `.git`
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        wf.place(Some(&g), &plain, &plain, "sess_1", vec![attached(&blobs, "c", "c.txt", "text/plain", b"hi")])
            .await
            .unwrap();
        assert!(plain.join(".anycon/attachments/sess_1/c.txt").is_file() && !plain.join(".git").exists());
    }

    #[test]
    fn removing_a_session_directory_refuses_links_and_leaves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(cwd.join(".anycon/attachments/sess_1")).unwrap();
        std::fs::create_dir_all(cwd.join(".anycon/attachments/sess_2")).unwrap();
        std::fs::write(cwd.join(".anycon/attachments/sess_1/a"), b"x").unwrap();
        assert!(remove_session_dir(&cwd, &cwd, "sess_1").unwrap());
        assert!(!cwd.join(".anycon/attachments/sess_1").exists() && cwd.join(".anycon/attachments/sess_2").is_dir());
        // 没有的目录不算错
        assert!(!remove_session_dir(&cwd, &cwd, "sess_1").unwrap());
        assert!(!remove_session_dir(&tmp.path().join("nowhere"), &tmp.path().join("nowhere"), "sess_1").unwrap());
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(outside.join("sess_3")).unwrap();
            std::os::unix::fs::symlink(outside.join("sess_3"), cwd.join(".anycon/attachments/sess_3")).unwrap();
            assert!(remove_session_dir(&cwd, &cwd, "sess_3").is_err());
            assert!(outside.join("sess_3").is_dir());
        }
    }

    #[test]
    fn placing_copies_files_privately_and_numbers_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        let map = Mutex::new(HashMap::new());
        let files = vec![
            attached(&blobs, "a", "report.pdf", "application/pdf", b"%PDF-1"),
            attached(&blobs, "b", "report.pdf", "application/pdf", b"%PDF-2"),
            // 名字是 `..`（上传时允许）：落成一个叫 `file` 的普通文件
            attached(&blobs, "c", "..", "text/plain", b"x"),
        ];
        let placed = place_blocking(&map, &cwd, &cwd, "sess_1", &files).unwrap();
        let rels: Vec<&str> = placed.iter().map(|p| p.rel.as_str()).collect();
        assert_eq!(
            rels,
            [
                ".anycon/attachments/sess_1/report.pdf",
                ".anycon/attachments/sess_1/report (2).pdf",
                ".anycon/attachments/sess_1/file"
            ]
        );
        assert_eq!(std::fs::read(cwd.join(rels[0])).unwrap(), b"%PDF-1");
        assert_eq!(std::fs::read(cwd.join(rels[1])).unwrap(), b"%PDF-2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&cwd.join(".anycon/attachments")), 0o700);
            assert_eq!(mode(&cwd.join(".anycon/attachments/sess_1")), 0o700);
            assert_eq!(mode(&cwd.join(rels[0])), 0o600);
        }
    }

    // ---- 检查和打开之间的窗口（unix：相对 fd 的逐级 `O_NOFOLLOW` 打开，没有这个窗口） ----

    /// 注册一个钩子：路径正好是 `target` 时（只触发一次）执行 `f`；`root` 之外的路径不理（钩子是进程范围的，测试并行跑）。
    /// 返回守卫和「钩子是不是触发过」。
    #[cfg(unix)]
    fn swap_hook(
        root: &Path,
        target: PathBuf,
        f: impl Fn() + Send + Sync + 'static,
    ) -> (crate::fsopen::HookGuard, Arc<std::sync::atomic::AtomicBool>) {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (root, flag) = (root.to_path_buf(), fired.clone());
        let guard = crate::fsopen::add_before_open_hook(Box::new(move |p: &Path| {
            if p.starts_with(&root) && p == target && !flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
                f();
            }
        }));
        (guard, fired)
    }

    /// 把 `path` 换成指向 `outside` 的符号链接（原来的目录整个挪走，像 agent 在沙箱里能做的那样）。
    #[cfg(unix)]
    fn replace_with_link(path: &Path, outside: &Path) {
        let moved = path.with_file_name(format!("{}.moved", path.file_name().unwrap().to_string_lossy()));
        std::fs::rename(path, &moved).unwrap();
        std::os::unix::fs::symlink(outside, path).unwrap();
    }

    /// 目录树里所有文件（相对路径）。
    #[cfg(unix)]
    fn walk(dir: &Path) -> Vec<PathBuf> {
        fn go(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if e.file_type().unwrap().is_dir() {
                    go(base, &p, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().to_path_buf());
                }
            }
        }
        let mut out = Vec::new();
        go(dir, dir, &mut out);
        out
    }

    /// `.anycon`、`attachments`、`<session_id>` 中任何一级在「决定打开它」和真正 `openat` 之间被换成符号链接：`O_NOFOLLOW` 拒绝，
    /// 链接指向的目录里什么都没有。路径版的实现（先 `lstat` 再 `create_new(dir.join(名字))`）在这里会把文件写进链接的目标。
    #[cfg(unix)]
    #[test]
    fn a_level_swapped_for_a_link_before_it_is_opened_is_refused_and_nothing_lands_behind_the_link() {
        for level in 0..3 {
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path().join("work");
            let dirs = cwd.join(".anycon/attachments/sess_1");
            std::fs::create_dir_all(&dirs).unwrap();
            let blobs = tmp.path().join("blobs");
            std::fs::create_dir(&blobs).unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(outside.join("sess_1")).unwrap();
            let target = [cwd.join(".anycon"), cwd.join(".anycon/attachments"), dirs.clone()][level].clone();
            let (t2, o2) = (target.clone(), outside.clone());
            let (_guard, fired) = swap_hook(tmp.path(), target, move || replace_with_link(&t2, &o2));
            let map = Mutex::new(HashMap::new());
            let files = vec![attached(&blobs, "a", "a.txt", "text/plain", b"secret")];
            let err = place_blocking(&map, &cwd, &cwd, "sess_1", &files).expect_err("the swap is refused");
            assert!(fired.load(std::sync::atomic::Ordering::SeqCst), "level {level}: the swap happened");
            assert!(matches!(&err, PlaceError::Dir(m) if m.contains("symbolic link")), "level {level}: {err}");
            let landed = walk(&outside);
            assert!(landed.is_empty(), "level {level}: nothing may land behind the link: {landed:?}");
            assert!(map.lock().unwrap().is_empty());
        }
    }

    /// 目录已经按 fd 打开之后，路径上的某一级才被换成符号链接：文件落在 fd 指的那个（被挪走的）目录里，不会跟着路径走到链接的目标。
    #[cfg(unix)]
    #[test]
    fn a_link_swapped_in_after_the_directories_are_open_does_not_redirect_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("sess_1")).unwrap();
        let attachments = cwd.join(".anycon/attachments");
        let file_path = attachments.join("sess_1/a.txt");
        let (a2, o2) = (attachments.clone(), outside.clone());
        let (_guard, fired) = swap_hook(tmp.path(), file_path, move || replace_with_link(&a2, &o2));
        let map = Mutex::new(HashMap::new());
        let files = vec![attached(&blobs, "a", "a.txt", "text/plain", b"hello")];
        let placed = place_blocking(&map, &cwd, &cwd, "sess_1", &files).unwrap();
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(placed[0].rel, ".anycon/attachments/sess_1/a.txt");
        assert!(walk(&outside).is_empty(), "the link's target stays empty: {:?}", walk(&outside));
        assert_eq!(std::fs::read(cwd.join(".anycon/attachments.moved/sess_1/a.txt")).unwrap(), b"hello");
    }

    /// worktree 会话：`cwd` 在边界（项目目录）之下几级，这几级也逐级 `O_NOFOLLOW`——换成链接就拒绝。
    #[cfg(unix)]
    #[test]
    fn the_levels_between_the_boundary_and_the_working_directory_are_not_followed_either() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let wt = project.join(".anycon/worktrees/feature");
        std::fs::create_dir_all(&wt).unwrap();
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let worktrees = project.join(".anycon/worktrees");
        let (w2, o2) = (worktrees.clone(), outside.clone());
        let (_guard, fired) = swap_hook(tmp.path(), worktrees, move || replace_with_link(&w2, &o2));
        let map = Mutex::new(HashMap::new());
        let files = vec![attached(&blobs, "a", "a.txt", "text/plain", b"x")];
        let err = place_blocking(&map, &project, &wt, "sess_1", &files).expect_err("refused");
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(&err, PlaceError::Dir(m) if m.contains("symbolic link")), "{err}");
        assert!(walk(&outside).is_empty());
    }

    /// 删除同样不跟随被换掉的一级；里面的符号链接只删链接本身，不碰它指向的东西。
    #[cfg(unix)]
    #[test]
    fn removing_does_not_follow_a_swapped_level_nor_the_links_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        let sid = cwd.join(".anycon/attachments/sess_1");
        std::fs::create_dir_all(sid.join("sub/deeper")).unwrap();
        std::fs::write(sid.join("sub/deeper/f"), b"x").unwrap();
        let precious = tmp.path().join("precious");
        std::fs::create_dir_all(precious.join("keep")).unwrap();
        std::fs::write(precious.join("keep/file"), b"keep me").unwrap();
        // 链接指向的目录里恰好有个同名的 `sess_1`：被跟随的话它会被删掉
        std::fs::create_dir_all(precious.join("sess_1")).unwrap();
        std::fs::write(precious.join("sess_1/important"), b"keep me too").unwrap();
        std::os::unix::fs::symlink(&precious, sid.join("link-to-dir")).unwrap();
        std::os::unix::fs::symlink(precious.join("keep/file"), sid.join("link-to-file")).unwrap();
        // 先换掉一级：一个都不删
        {
            let attachments = cwd.join(".anycon/attachments");
            let (a2, o2) = (attachments.clone(), precious.clone());
            let (_guard, fired) = swap_hook(tmp.path(), attachments, move || replace_with_link(&a2, &o2));
            assert!(remove_session_dir(&cwd, &cwd, "sess_1").is_err());
            assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(std::fs::read(precious.join("keep/file")).unwrap(), b"keep me");
            assert_eq!(std::fs::read(precious.join("sess_1/important")).unwrap(), b"keep me too");
        }
        // 把原来的目录放回来，正常删：链接被删掉，目标完好
        std::fs::remove_file(cwd.join(".anycon/attachments")).unwrap();
        std::fs::rename(cwd.join(".anycon/attachments.moved"), cwd.join(".anycon/attachments")).unwrap();
        assert!(remove_session_dir(&cwd, &cwd, "sess_1").unwrap());
        assert!(!sid.exists());
        assert_eq!(std::fs::read(precious.join("keep/file")).unwrap(), b"keep me");
        assert!(cwd.join(".anycon/attachments").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn removing_refuses_a_directory_nested_deeper_than_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        let mut deep = cwd.join(".anycon/attachments/sess_1");
        for _ in 0..(crate::fsopen::REMOVE_MAX_DEPTH + 2) {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        assert!(remove_session_dir(&cwd, &cwd, "sess_1").is_err());
        assert!(
            cwd.join(".anycon/attachments/sess_1").exists(),
            "the directory itself is kept when the tree is too deep"
        );
    }
}
