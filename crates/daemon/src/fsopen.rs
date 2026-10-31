//! 打开文件 / 目录时不让「检查之后、打开之前」换目录的窗口变成越界（ADR-33 第 5 条）。
//!
//! `fs.read` / `fs.list` 先把路径 canonicalize 再检查在边界内，然后才去打开：两步之间有人把目录换成符号链接，
//! 打开的就是边界外的东西。
//!
//! - **文件**（[`open_file_in`]）：从边界目录开始**逐段 `openat`**，每一段都带 `O_NOFOLLOW`（中间段还带 `O_DIRECTORY`）。
//!   边界目录自己按路径打开一次，再用句柄的真实路径（macOS `fcntl(F_GETPATH)`，Linux `/proc/self/fd/N`）和边界比较（目录没有硬链接，
//!   这个比较是准的）。之后每一步都相对上一步拿到的 fd，任何一段被换成符号链接都是 `ELOOP` / `ENOTDIR` -> 越界；
//!   换成别的真实目录或文件则仍然在边界之内。所以文件**不需要**再按句柄的真实路径复核，也不需要拿「打开之前按路径 `stat` 的身份」
//!   来凑——那个身份如果是换目录之后才取的，比较永远成立。
//! - **目录**（[`list_dir_verified`]）：`O_DIRECTORY | O_NOFOLLOW` 打开，按句柄的真实路径复核在边界内，用打开的 fd 枚举
//!   （`fdopendir`），不再按路径重新 `read_dir`。
//!
//! 其它平台没有取 fd 真实路径的办法时，边界目录和目录列表的复核退化成只有 `O_NOFOLLOW`。
//!
//! **硬链接**：项目里的文件在项目外另有一个硬链接时，`F_GETPATH` 可能报出外面的名字。文件走逐段 `openat`，
//! 到达它的名字都在边界之内，不看 `F_GETPATH`，所以这不是越界；硬链接本来就不是边界（见 daemon.md § 10）。
//!
//! **Windows** 没有 `openat`。文件和目录都按完整路径打开，最后一段不跟随重解析点（`FILE_FLAG_OPEN_REPARSE_POINT`），
//! 然后对**打开的句柄**复核：是符号链接 / junction 就算越界；句柄的真实路径（`GetFinalPathNameByHandleW`）不在边界之内也算越界——
//! 中间某一段在检查之后被换成指向边界外的链接，打开时会顺着它走，真实路径就落到了外面。复核的是句柄而不是路径，所以没有
//! 「复核之后又被换掉」的窗口。硬链接：真实路径是打开时用的那个名字，在边界之内。目录用打开的句柄枚举
//! （`GetFileInformationByHandleEx`），不再按路径重新列。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    fs::OpenOptions,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    os::unix::ffi::{OsStrExt, OsStringExt},
    os::unix::fs::OpenOptionsExt,
};

/// 打开 / 复核失败。
#[derive(Debug)]
pub enum OpenError {
    /// 打开之后发现句柄指向边界之外（或最后一段被换成了符号链接）。
    Escaped,
    /// 不是普通文件 / 不是目录。
    WrongKind,
    Io(io::Error),
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        // O_NOFOLLOW 遇到符号链接是 ELOOP（Linux）/ EMLINK（部分 BSD）：被换成了链接
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return OpenError::Escaped;
        }
        OpenError::Io(e)
    }
}

/// 测试钩子：在 `open` 之前调用（参数是要打开的路径）。用来在「检查之后、打开之前」换目录。
/// 钩子登记在进程范围的列表里（测试并行跑），每个钩子只应对自己的临时目录里的路径起作用；`HookGuard` 丢弃时撤销。
#[cfg(any(test, feature = "testing"))]
pub type BeforeOpen = Box<dyn Fn(&Path) + Send + Sync>;
#[cfg(any(test, feature = "testing"))]
static HOOKS: std::sync::Mutex<Vec<(u64, BeforeOpen)>> = std::sync::Mutex::new(Vec::new());
#[cfg(any(test, feature = "testing"))]
static HOOK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(any(test, feature = "testing"))]
pub struct HookGuard(u64);

#[cfg(any(test, feature = "testing"))]
impl Drop for HookGuard {
    fn drop(&mut self) {
        HOOKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _)| *id != self.0);
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn add_before_open_hook(hook: BeforeOpen) -> HookGuard {
    let id = HOOK_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    HOOKS.lock().unwrap_or_else(|e| e.into_inner()).push((id, hook));
    HookGuard(id)
}

fn before_open(_path: &Path) {
    #[cfg(any(test, feature = "testing"))]
    for (_, h) in HOOKS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        h(_path);
    }
}

/// 测试钩子：文件 `open` 之后调用（用来在这一刻 `stat` 另一个硬链接名，复现 macOS 上 `F_GETPATH` 的行为，确认逐段 `openat` 的结果
/// 不受它影响）。
#[cfg(any(test, feature = "testing"))]
static AFTER_HOOKS: std::sync::Mutex<Vec<(u64, BeforeOpen)>> = std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "testing"))]
pub struct AfterHookGuard(u64);

#[cfg(any(test, feature = "testing"))]
impl Drop for AfterHookGuard {
    fn drop(&mut self) {
        AFTER_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _)| *id != self.0);
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn add_after_open_hook(hook: BeforeOpen) -> AfterHookGuard {
    let id = HOOK_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    AFTER_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).push((id, hook));
    AfterHookGuard(id)
}

fn after_open(_path: &Path) {
    #[cfg(any(test, feature = "testing"))]
    for (_, h) in AFTER_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        h(_path);
    }
}

/// 测试钩子：`fs.read` 解析完路径（边界检查通过）之后、开始打开之前调用。用来在「检查刚过」这一刻换目录——
/// 以前的实现在这之后还要按路径 `stat` 一次取身份，换在这里的目录会被「身份」认下来。
#[cfg(any(test, feature = "testing"))]
static RESOLVE_HOOKS: std::sync::Mutex<Vec<(u64, BeforeOpen)>> = std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "testing"))]
pub struct ResolveHookGuard(u64);

#[cfg(any(test, feature = "testing"))]
impl Drop for ResolveHookGuard {
    fn drop(&mut self) {
        RESOLVE_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _)| *id != self.0);
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn add_after_resolve_hook(hook: BeforeOpen) -> ResolveHookGuard {
    let id = HOOK_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    RESOLVE_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).push((id, hook));
    ResolveHookGuard(id)
}

/// `fs.read` 解析完路径之后调用（只有测试钩子会做事）。
pub fn after_resolve(_path: &Path) {
    #[cfg(any(test, feature = "testing"))]
    for (_, h) in RESOLVE_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        h(_path);
    }
}

/// fd 现在指向的真实路径。`Unsupported`：这个平台没有办法。
#[cfg(target_os = "macos")]
pub fn fd_real_path(f: &File) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH 往 `buf`（至少 PATH_MAX 字节）写一个以 NUL 结尾的路径；fd 在 `f` 活着期间有效。
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(target_os = "linux")]
pub fn fd_real_path(f: &File) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd()))
}

/// 句柄的真实路径（`GetFinalPathNameByHandleW`，去掉 `\\?\` 前缀，和 [`crate::sys::canonicalize`] 的写法一致）。
#[cfg(windows)]
pub fn fd_real_path(f: &File) -> io::Result<PathBuf> {
    win::final_path(f)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn fd_real_path(_f: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "cannot resolve the path of a file descriptor on this platform"))
}

/// 复核句柄：真实路径必须在 `boundary` 之内。
#[cfg(unix)]
fn verify(f: &File, boundary: &Path) -> Result<Option<PathBuf>, OpenError> {
    match fd_real_path(f) {
        Ok(real) if real.starts_with(boundary) => Ok(Some(real)),
        Ok(_) => Err(OpenError::Escaped),
        // 内核说文件已经被删了（macOS 返回 ENOENT）：当作换掉了
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(OpenError::Escaped),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(e) => Err(OpenError::Io(e)),
    }
}

/// `openat(dir, name, flags)`；`dir` 为 `None` 时 `name` 是绝对路径（`AT_FDCWD`）。
#[cfg(unix)]
fn openat(dir: Option<&File>, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let c = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let dirfd = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
    // SAFETY: `c` 是以 NUL 结尾的有效 C 字符串；`dirfd` 要么是 AT_FDCWD，要么是 `dir` 持有的、仍然打开的 fd。
    let fd = unsafe { libc::openat(dirfd, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` 是刚由 `openat` 返回的、我们独占的有效 fd。
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// 这一段（`name`，在 `dir` 里）现在是不是符号链接。
#[cfg(unix)]
fn is_symlink_at(dir: Option<&File>, name: &OsStr) -> bool {
    let Ok(c) = CString::new(name.as_bytes()) else { return false };
    let dirfd = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
    // SAFETY: `st` 由 `fstatat` 写入；其余参数同 `openat`。
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstatat(dirfd, c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) == 0
            && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }
}

/// 以目录方式打开一段：`O_DIRECTORY | O_NOFOLLOW`。macOS 对指向目录的符号链接返回 `ENOTDIR` 而不是 `ELOOP`，是链接就当作被换了。
#[cfg(unix)]
fn open_dir_segment(dir: Option<&File>, name: &OsStr) -> Result<File, OpenError> {
    match openat(dir, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) {
        Ok(f) => Ok(f),
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) && is_symlink_at(dir, name) => Err(OpenError::Escaped),
        Err(e) => Err(e.into()),
    }
}

/// 打开一个普通文件读：从 `boundary` 开始逐段 `openat`（`O_NOFOLLOW`，最后一段还带 `O_NONBLOCK`：换成 FIFO 之类也不会卡住），
/// `fstat` 确认是普通文件。`path` 必须在 `boundary` 之下（`fs.read` 解析出来的 canonical 路径）。模块说明解释了为什么这样就不需要
/// 再复核句柄的真实路径。Windows 的做法也见模块说明。
pub fn open_file_in(path: &Path, boundary: &Path) -> Result<File, OpenError> {
    let segs = segments_within(path, boundary)?;
    if segs.is_empty() {
        return Err(OpenError::WrongKind);
    }
    before_open(path);
    #[cfg(unix)]
    let f = open_file_segments(boundary, &segs)?;
    #[cfg(windows)]
    let f = win::open_verified(path, boundary, false)?;
    if !f.metadata()?.is_file() {
        return Err(OpenError::WrongKind);
    }
    after_open(path);
    Ok(f)
}

/// `path` 相对 `boundary` 的各段；不在边界之下、或有 `..` 之类的段：越界。
fn segments_within<'a>(path: &'a Path, boundary: &Path) -> Result<Vec<&'a OsStr>, OpenError> {
    let rel = path.strip_prefix(boundary).map_err(|_| OpenError::Escaped)?;
    let mut segs: Vec<&OsStr> = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(s) => segs.push(s),
            _ => return Err(OpenError::Escaped),
        }
    }
    Ok(segs)
}

/// 从 `boundary` 起逐段 `openat`，最后一段以文件方式打开（`segs` 非空）。
#[cfg(unix)]
fn open_file_segments(boundary: &Path, segs: &[&OsStr]) -> Result<File, OpenError> {
    let Some((last, dirs)) = segs.split_last() else { return Err(OpenError::WrongKind) };
    // 边界目录自己：按路径打开一次，用句柄的真实路径核对（目录没有硬链接，这个比较是准的）；
    // 路径的上级被换成指向别处的符号链接时，句柄的真实路径对不上
    let mut cur = open_dir_segment(None, boundary.as_os_str())?;
    match fd_real_path(&cur) {
        Ok(real) if real == boundary => {}
        Ok(_) => return Err(OpenError::Escaped),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenError::Escaped),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
        Err(e) => return Err(OpenError::Io(e)),
    }
    for seg in dirs {
        cur = open_dir_segment(Some(&cur), seg)?;
    }
    Ok(openat(Some(&cur), last, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?)
}

/// 打开一个目录并列出里面的名字（不含 `.` `..`）。返回复核过的真实路径（调用方用它拼各个条目的路径）和名字。
/// 枚举用打开的 fd（`fdopendir`；Windows：打开的句柄），不会因为路径又被换掉而列到别处。
#[cfg(windows)]
pub fn list_dir_verified(path: &Path, boundary: &Path) -> Result<(PathBuf, Vec<OsString>), OpenError> {
    before_open(path);
    let f = win::open_verified(path, boundary, true)?;
    let real = fd_real_path(&f)?;
    Ok((real, win::list_names(&f)?))
}

/// 打开一个目录并列出里面的名字（不含 `.` `..`）。返回复核过的真实路径（调用方用它拼各个条目的路径）和名字。
/// 枚举用打开的 fd（`fdopendir`），不会因为路径又被换掉而列到别处。
#[cfg(unix)]
pub fn list_dir_verified(path: &Path, boundary: &Path) -> Result<(PathBuf, Vec<OsString>), OpenError> {
    before_open(path);
    let f = match OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path) {
        Ok(f) => f,
        // `O_DIRECTORY | O_NOFOLLOW` 遇到指向目录的符号链接：macOS 是 ENOTDIR，不是 ELOOP；是链接就当作被换了
        Err(e)
            if e.raw_os_error() == Some(libc::ENOTDIR)
                && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) =>
        {
            return Err(OpenError::Escaped)
        }
        Err(e) => return Err(e.into()),
    };
    let real = verify(&f, boundary)?.unwrap_or_else(|| path.to_path_buf());
    let fd = f.into_raw_fd();
    // SAFETY: `fd` 是刚拿到所有权的、打开的目录 fd；`fdopendir` 成功后目录流接管它（`closedir` 时一起关），
    // 失败时我们自己关。`readdir` 返回的指针在下一次 `readdir` / `closedir` 之前有效，这里立刻拷贝名字。
    unsafe {
        let dirp = libc::fdopendir(fd);
        if dirp.is_null() {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e.into());
        }
        let mut names = Vec::new();
        loop {
            let ent = libc::readdir(dirp);
            if ent.is_null() {
                break;
            }
            let name = CStr::from_ptr((*ent).d_name.as_ptr()).to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
        libc::closedir(dirp);
        Ok((real, names))
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileFullDirectoryInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_GENERIC_READ,
        FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        SYNCHRONIZE, VOLUME_NAME_DOS,
    };

    use super::OpenError;
    use crate::sys::win::{path_from_wide, wide};

    /// 按路径打开（最后一段不跟随重解析点），再复核句柄：不是符号链接 / junction、种类对（`dir`）、真实路径在 `boundary` 之内。
    pub(super) fn open_verified(path: &Path, boundary: &Path, dir: bool) -> Result<File, OpenError> {
        let w = wide(path.as_os_str());
        let access = if dir { FILE_LIST_DIRECTORY | SYNCHRONIZE } else { FILE_GENERIC_READ };
        // SAFETY: `w` 以 NUL 结尾；其余参数是常量或空指针。BACKUP_SEMANTICS 让目录也能打开（文件那边随后按种类拒绝）。
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(OpenError::Io(io::Error::last_os_error()));
        }
        // SAFETY: 刚打开的有效句柄，由 File 接管。
        let f = unsafe { File::from_raw_handle(h) };
        let md = f.metadata()?;
        if md.file_type().is_symlink() {
            return Err(OpenError::Escaped);
        }
        if dir && !md.is_dir() {
            return Err(OpenError::Io(io::Error::new(io::ErrorKind::NotADirectory, "not a directory")));
        }
        match final_path(&f) {
            Ok(real) if real.starts_with(boundary) => Ok(f),
            Ok(_) => Err(OpenError::Escaped),
            Err(e) => Err(OpenError::Io(e)),
        }
    }

    pub(super) fn final_path(f: &File) -> io::Result<PathBuf> {
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: 句柄在 `f` 活着期间有效；缓冲长度如实传入。
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    f.as_raw_handle(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            } as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            // 缓冲不够时返回所需长度（含 NUL）
            if n >= buf.len() {
                buf.resize(n + 1, 0);
                continue;
            }
            let p = path_from_wide(&buf[..n]);
            return Ok(crate::sys::simplified(&p).to_path_buf());
        }
    }

    /// 用打开的目录句柄枚举名字（不含 `.` `..`）。
    pub(super) fn list_names(f: &File) -> io::Result<Vec<OsString>> {
        // u64 对齐的 64 KiB 缓冲
        let mut buf = vec![0u64; 8 * 1024];
        let mut names = Vec::new();
        loop {
            // SAFETY: 句柄以 FILE_LIST_DIRECTORY 打开；缓冲长度如实传入。第一次调用开始枚举，之后每次接着上一次。
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    f.as_raw_handle(),
                    FileFullDirectoryInfo,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    return Ok(names);
                }
                return Err(e);
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                // SAFETY: 系统在缓冲里写了一串 FILE_FULL_DIR_INFO，每一项按 NextEntryOffset 串起来（8 字节对齐），
                // 文件名紧跟在结构体的 FileName 字段处，长度 FileNameLength 字节。
                let (next, name) = unsafe {
                    let info = base.add(off) as *const FILE_FULL_DIR_INFO;
                    let len = (*info).FileNameLength as usize / 2;
                    let name = std::slice::from_raw_parts(std::ptr::addr_of!((*info).FileName) as *const u16, len);
                    ((*info).NextEntryOffset as usize, name)
                };
                let n = OsString::from(path_from_wide(name));
                if n != "." && n != ".." {
                    names.push(n);
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(p: &Path) -> PathBuf {
        crate::sys::canonicalize(p).unwrap()
    }

    #[test]
    fn a_file_inside_the_boundary_opens_and_reports_its_real_path() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj/sub")).unwrap();
        std::fs::write(root.join("proj/sub/a.txt"), "hello").unwrap();
        let mut f = open_file_in(&root.join("proj/sub/a.txt"), &root.join("proj")).unwrap();
        let mut s = String::new();
        io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "hello");
        if let Ok(real) = fd_real_path(&f) {
            assert_eq!(real, root.join("proj/sub/a.txt"));
        }
        // 目录
        let (real, mut names) = list_dir_verified(&root.join("proj"), &root.join("proj")).unwrap();
        names.sort();
        assert_eq!((real, names), (root.join("proj"), vec![OsString::from("sub")]));
        assert!(matches!(open_file_in(&root.join("proj/sub"), &root.join("proj")), Err(OpenError::WrongKind)));
        assert!(matches!(open_file_in(&root.join("proj"), &root.join("proj")), Err(OpenError::WrongKind)));
        assert!(matches!(list_dir_verified(&root.join("proj/sub/a.txt"), &root.join("proj")), Err(OpenError::Io(_))));
        assert!(matches!(open_file_in(&root.join("proj/none"), &root.join("proj")), Err(OpenError::Io(_))));
        // 不在边界之下的路径不打开
        assert!(matches!(open_file_in(&root.join("elsewhere.txt"), &root.join("proj")), Err(OpenError::Escaped)));
    }

    /// `mklink /J`：建 junction 不需要特权。
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "mklink /J failed");
    }

    /// Windows：最后一段是 junction 就拒绝；中间一段是指向边界外的 junction，句柄的真实路径落在外面，同样拒绝；
    /// 中间一段指向边界之内的，读到的仍然是边界里的文件，允许。
    #[cfg(windows)]
    #[test]
    fn junctions_are_judged_by_where_the_handle_really_is() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj").join("real")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside").join("secret.txt"), "outside-content").unwrap();
        std::fs::write(root.join("proj").join("real").join("f.txt"), "inside").unwrap();
        let boundary = root.join("proj");
        junction(&boundary.join("out"), &root.join("outside"));
        junction(&boundary.join("alias"), &boundary.join("real"));
        assert!(matches!(open_file_in(&boundary.join("out").join("secret.txt"), &boundary), Err(OpenError::Escaped)));
        assert!(matches!(list_dir_verified(&boundary.join("out"), &boundary), Err(OpenError::Escaped)));
        assert!(matches!(list_dir_verified(&boundary.join("alias"), &boundary), Err(OpenError::Escaped)));
        let mut f = open_file_in(&boundary.join("alias").join("f.txt"), &boundary).unwrap();
        let mut s = String::new();
        io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "inside");
        // 整个边界目录被换成指向别处的 junction：真实路径不在边界之内
        std::fs::remove_dir_all(root.join("proj").join("real")).unwrap();
        junction(&boundary.join("real"), &root.join("outside"));
        assert!(matches!(open_file_in(&boundary.join("real").join("secret.txt"), &boundary), Err(OpenError::Escaped)));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_swapped_for_a_symlink_after_the_check_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj/dir")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret.txt"), "outside-content").unwrap();
        std::fs::write(root.join("proj/dir/secret.txt"), "harmless").unwrap();
        std::fs::write(root.join("proj/f.txt"), "harmless").unwrap();
        let boundary = root.join("proj");
        // 最后一段是符号链接（canonicalize 之后被换成了链接）：O_NOFOLLOW 拒绝
        std::fs::remove_file(root.join("proj/f.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("outside/secret.txt"), root.join("proj/f.txt")).unwrap();
        assert!(matches!(open_file_in(&root.join("proj/f.txt"), &boundary), Err(OpenError::Escaped)));
        // 中间一段是符号链接：逐段 `openat` 带 `O_NOFOLLOW`，在这一段就被拒绝（不靠事后核对句柄的真实路径）
        std::fs::remove_dir_all(root.join("proj/dir")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("proj/dir")).unwrap();
        let r = open_file_in(&root.join("proj/dir/secret.txt"), &boundary);
        assert!(matches!(r, Err(OpenError::Escaped)), "{r:?}");
        // 边界目录自己的上级被换成链接：句柄的真实路径对不上边界
        std::fs::create_dir_all(root.join("up/proj")).unwrap();
        std::fs::write(root.join("up/proj/f.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join("elsewhere/proj")).unwrap();
        std::fs::write(root.join("elsewhere/proj/f.txt"), "y").unwrap();
        std::fs::rename(root.join("up"), root.join("up.real")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("up")).unwrap();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let r = open_file_in(&root.join("up/proj/f.txt"), &root.join("up/proj"));
            assert!(matches!(r, Err(OpenError::Escaped)), "{r:?}");
        }
        // 目录：最后一段是链接
        assert!(matches!(list_dir_verified(&root.join("proj/dir"), &boundary), Err(OpenError::Escaped)));
        // 目录：中间一段是链接
        std::fs::create_dir_all(root.join("proj/real")).unwrap();
        std::fs::create_dir_all(root.join("outside/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("proj/real/escape")).unwrap();
        let r = list_dir_verified(&root.join("proj/real/escape/inner"), &boundary);
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(matches!(r, Err(OpenError::Escaped)), "{r:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_hook_can_swap_the_path_between_the_check_and_the_open() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj/dir")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("proj/dir/a.txt"), "inside").unwrap();
        std::fs::write(root.join("outside/a.txt"), "outside").unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        let target = proj.join("dir/a.txt");
        // 检查时 `dir` 是普通目录；钩子在 open 之前把它换成指向 outside 的链接（竞态）
        let (t2, proj2, outside2) = (target.clone(), proj.clone(), outside.clone());
        let guard = add_before_open_hook(Box::new(move |p: &Path| {
            if p == t2 {
                std::fs::remove_dir_all(proj2.join("dir")).unwrap();
                std::os::unix::fs::symlink(&outside2, proj2.join("dir")).unwrap();
            }
        }));
        let r = open_file_in(&target, &proj);
        drop(guard);
        assert!(matches!(r, Err(OpenError::Escaped)), "the swapped path must be refused, got {r:?}");
    }

    /// 项目里的文件在项目外另有一个硬链接；`open` 之后别处 `stat` 了外面那个名字，macOS 上同一个 fd 的 `F_GETPATH`
    /// 就变成外面的路径。逐段 `openat` 到达它的名字都在边界之内，不看 `F_GETPATH`，所以照常读到；换成别的文件（符号链接）仍然拒绝。
    #[cfg(unix)]
    #[test]
    fn a_hard_link_outside_the_boundary_does_not_make_the_file_look_escaped() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("proj/a.txt"), "shared content").unwrap();
        std::fs::hard_link(root.join("proj/a.txt"), root.join("outside/b.txt")).unwrap();
        std::fs::write(root.join("outside/other.txt"), "someone else's file").unwrap();
        let (a, b, other) = (root.join("proj/a.txt"), root.join("outside/b.txt"), root.join("outside/other.txt"));
        let proj = root.join("proj");
        // 打开 a 之后，另一个进程 stat 了外面的 b（复现 F_GETPATH 返回最近一次查找的名字）
        let b2 = b.clone();
        let a2 = a.clone();
        let guard = add_after_open_hook(Box::new(move |p: &Path| {
            if p == a2 {
                let _ = std::fs::metadata(&b2);
            }
        }));
        let mut f = open_file_in(&a, &proj).expect("the same file, reached through its other name");
        drop(guard);
        let mut s = String::new();
        io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "shared content");
        // 另一个文件被换到同一个路径上（符号链接）：O_NOFOLLOW 拒绝
        std::fs::remove_file(&a).unwrap();
        std::os::unix::fs::symlink(&other, &a).unwrap();
        let r = open_file_in(&a, &proj);
        assert!(matches!(r, Err(OpenError::Escaped)), "a different file must still be refused: {r:?}");
    }

    /// 目录在「检查刚过」这一刻被换成指向边界外的链接。以前按路径 `stat` 取身份再比较：换在取身份之前，
    /// 身份就是外面那个文件的，比较永远成立。现在没有身份比较，逐段 `openat` 在被换的那一段就拒绝。
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_right_after_the_check_is_refused_whatever_its_identity() {
        let d = tempfile::tempdir().unwrap();
        let root = canon(d.path());
        std::fs::create_dir_all(root.join("proj/dir")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("proj/dir/a.txt"), "inside").unwrap();
        std::fs::write(root.join("outside/a.txt"), "outside").unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        std::fs::remove_dir_all(proj.join("dir")).unwrap();
        std::os::unix::fs::symlink(&outside, proj.join("dir")).unwrap();
        // 路径现在经过链接解析到 outside/a.txt：按路径 stat 得到的身份就是外面那个文件的
        let r = open_file_in(&proj.join("dir/a.txt"), &proj);
        assert!(matches!(r, Err(OpenError::Escaped)), "{r:?}");
    }
}
