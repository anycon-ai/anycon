//! 平台差异的小工具：unix 和 Windows 各一份实现，调用方不写 `cfg`。
//!
//! - **只给属主访问**：unix 是权限位（目录 0700、文件 0600）。Windows 没有权限位，[`restrict_to_owner`] 给目录 / 文件设一份
//!   受保护的 DACL（不继承上级），只有当前用户、SYSTEM 和 Administrators 能访问，目录的这份 DACL 由里面新建的东西继承——
//!   所以 [`OwnerOnly::owner_only`] 在 Windows 上不做事：数据目录在 daemon 启动和 `anycon init` 时已经限制过，里面的文件继承它
//! - **规范路径**：Windows 上 `std::fs::canonicalize` 返回 `\\?\C:\…` 形式，交给 git、agent、客户端都不合适；
//!   [`canonicalize`] 在能去掉前缀时去掉（`dunce`）
//! - **文件身份** `(设备, inode)`：Windows 上是卷序列号和文件索引（标准库的 `Metadata` 里还没有，要另开一次句柄）
//! - 主机名、磁盘剩余空间、按偏移读写、把 git 输出的路径字节转成路径

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// 规范路径（解析符号链接、`.`、`..`）。Windows 上尽量不带 `\\?\` 前缀。
pub fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(p)
}

/// Windows：去掉 `\\?\` 前缀（能去掉时）。其它平台原样。
pub fn simplified(p: &Path) -> &Path {
    dunce::simplified(p)
}

/// 只让属主访问 `p`：unix 上目录 0700、文件 0600；Windows 上设受保护的 DACL（当前用户、SYSTEM、Administrators；目录的会被
/// 里面新建的东西继承）。
pub fn restrict_to_owner(p: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if std::fs::metadata(p)?.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        win::restrict_to_owner(p)
    }
}

/// 新建文件时的权限。
pub trait OwnerOnly {
    /// unix：新建的文件是 `mode`（如 0o600、0o644）。Windows 上不做事：文件继承所在目录的 DACL（模块说明）。
    fn unix_mode(&mut self, mode: u32) -> &mut Self;
    /// 只有属主能读写：`unix_mode(0o600)`。
    fn owner_only(&mut self) -> &mut Self {
        self.unix_mode(0o600)
    }
}

impl OwnerOnly for OpenOptions {
    #[cfg(unix)]
    fn unix_mode(&mut self, mode: u32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(self, mode)
    }
    #[cfg(windows)]
    fn unix_mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
}

/// 打开 `path`，最后一段是符号链接时失败：unix 用 `O_NOFOLLOW`；Windows 不跟随重解析点（`FILE_FLAG_OPEN_REPARSE_POINT`），
/// 打开之后发现是符号链接 / junction 就报错（这时打开的是链接本身，没有碰到它指向的东西）。会覆盖 `opts` 里别的 custom flags。
pub fn open_no_follow(opts: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        std::os::unix::fs::OpenOptionsExt::custom_flags(opts, libc::O_NOFOLLOW).open(path)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let f = std::os::windows::fs::OpenOptionsExt::custom_flags(opts, FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
        if f.metadata()?.file_type().is_symlink() {
            return Err(io::Error::other(format!("{} is a symbolic link", path.display())));
        }
        Ok(f)
    }
}

/// 权限位的低 9 位（`doctor` 检查用）。Windows 没有权限位，返回 `None`。
pub fn mode_bits(md: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(md.permissions().mode() & 0o777)
    }
    #[cfg(windows)]
    {
        let _ = md;
        None
    }
}

/// 设权限位（复制文件时保留可执行位之类）。Windows 上不做事。
pub fn set_mode_bits(p: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = (p, mode);
        Ok(())
    }
}

/// 是不是「可执行」的普通文件：unix 看任意一个 x 位；Windows 没有 x 位，普通文件都算（是否能执行由扩展名决定，这里不管）。
pub fn is_executable(md: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.is_file() && md.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        md.is_file()
    }
}

/// `md`（取自 `path`）的身份 `(设备, inode)`。unix 直接从 `md` 取；Windows 的 `Metadata` 里没有，按 `path` 再开一次句柄取
/// 卷序列号和文件索引，`follow` 要和取 `md` 时一致（`metadata` = true，`symlink_metadata` = false）。取不到时 `None`。
pub fn file_identity(md: &Metadata, path: &Path, follow: bool) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = (path, follow);
        Some((md.dev(), md.ino()))
    }
    #[cfg(windows)]
    {
        let _ = md;
        win::file_identity(path, follow).ok()
    }
}

/// `path` 的身份（跟随符号链接）。
pub fn identity_of(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        win::file_identity(path, true).ok()
    }
}

/// 打开的文件的身份（Windows 上同样按句柄取，不再按路径）。
pub fn handle_identity(f: &File) -> io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        win::handle_identity(f)
    }
}

/// 从 `offset` 起读满 `buf`。
#[cfg(unix)]
pub fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, offset)
}

/// 从 `offset` 起读满 `buf`。`seek_read` 会移动文件指针；调用方都只按偏移读写，不依赖文件指针。
#[cfg(windows)]
pub fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(f, buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 从 `offset` 起写完 `buf`。
#[cfg(unix)]
pub fn write_all_at(f: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, offset)
}

/// 从 `offset` 起写完 `buf`（同 [`read_exact_at`]，会移动文件指针）。
#[cfg(windows)]
pub fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(f, buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// git 输出（`-z`）里的一个路径。unix 上任意字节都是合法路径；Windows 上 git 输出 UTF-8，不是 UTF-8 的返回 `None`。
pub fn path_from_bytes(raw: &[u8]) -> Option<&Path> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(Path::new(std::ffi::OsStr::from_bytes(raw)))
    }
    #[cfg(windows)]
    {
        std::str::from_utf8(raw).ok().map(Path::new)
    }
}

/// 主机名；取不到时 `None`。
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf 足够大，gethostname 只写入 buf。
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..end]).trim().to_string())
    }
    #[cfg(windows)]
    {
        win::hostname()
    }
}

/// `p` 所在文件系统上当前用户可用的字节数。
pub fn disk_free(p: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` 是以 NUL 结尾的有效 C 字符串；`s` 由 statvfs 写入。
        unsafe {
            let mut s: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut s) == 0 {
                #[allow(clippy::unnecessary_cast)]
                return Some(s.f_bavail as u64 * s.f_frsize as u64);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        win::disk_free(p)
    }
}

/// 当前用户能不能进入并列出目录 `p`。unix 用 `access(2)`，只看权限位、不打开目录（打开 `~/Documents` 之类会在 macOS 上
/// 触发隐私授权弹窗）；Windows 没有这类弹窗，直接试着列一次。
pub fn can_read_dir(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
        // SAFETY: `c` 是以 NUL 结尾的有效 C 字符串；access 只读取它。
        unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::X_OK) == 0 }
    }
    #[cfg(windows)]
    {
        std::fs::read_dir(p).is_ok()
    }
}

#[cfg(windows)]
pub mod win {
    //! Windows API 的薄封装。

    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// 以 NUL 结尾的 UTF-16。
    pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 当前进程用户的 SID（`S-1-5-21-…`）。
    pub fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: 伪句柄 GetCurrentProcess() 总是有效；`token` 由调用写入。
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 刚打开的令牌句柄，由 OwnedHandle 负责关闭。
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut len = 0u32;
        // SAFETY: 第一次调用只取所需长度（缓冲为空）。
        unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, std::ptr::null_mut(), 0, &mut len) };
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(e);
        }
        // u64 对齐的缓冲，放得下 TOKEN_USER 和后面的 SID
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: 缓冲长度至少 `len` 字节。
        if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 成功返回后缓冲开头是 TOKEN_USER，里面的 SID 指针指向同一个缓冲。
        let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
        let mut s: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` 有效；成功时 `s` 是 LocalAlloc 分配的、以 NUL 结尾的宽字符串，用完 LocalFree。
        if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `s` 以 NUL 结尾。
        let out = unsafe {
            let n = (0..).take_while(|&i| *s.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
        };
        // SAFETY: `s` 由 ConvertSidToStringSidW 分配。
        unsafe { LocalFree(s.cast()) };
        Ok(out)
    }

    /// SDDL 转成的安全描述符（LocalAlloc 分配，drop 时释放）。
    pub(crate) struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl SecurityDescriptor {
        pub(crate) fn from_sddl(sddl: &str) -> io::Result<SecurityDescriptor> {
            let w = wide(OsStr::new(sddl));
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `w` 以 NUL 结尾；成功时 `sd` 由系统分配。
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(SecurityDescriptor(sd))
        }

        pub(crate) fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
            self.0
        }

        fn dacl(&self) -> io::Result<*mut ACL> {
            let (mut present, mut defaulted) = (0, 0);
            let mut dacl: *mut ACL = std::ptr::null_mut();
            // SAFETY: `self.0` 是有效的安全描述符；返回的 DACL 指针指向它内部，生命周期随它。
            if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(dacl)
        }
    }

    // SAFETY: 一块由系统分配、创建之后只读的内存，只在 drop 时释放一次。
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: 由 ConvertStringSecurityDescriptorToSecurityDescriptorW 分配。
            unsafe { LocalFree(self.0) };
        }
    }

    /// 只有 `sid`、SYSTEM 和 Administrators 能访问的受保护 DACL（目录带继承标志）。
    pub(crate) fn owner_only_sddl(sid: &str, dir: bool) -> String {
        let inherit = if dir { "OICI" } else { "" };
        format!("D:P(A;{inherit};FA;;;{sid})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)")
    }

    pub(super) fn restrict_to_owner(p: &Path) -> io::Result<()> {
        let dir = std::fs::metadata(p)?.is_dir();
        let sd = SecurityDescriptor::from_sddl(&owner_only_sddl(&current_user_sid()?, dir))?;
        let dacl = sd.dacl()?;
        let w = wide(p.as_os_str());
        // SAFETY: `w` 以 NUL 结尾；`dacl` 在 `sd` 活着期间有效。SetNamedSecurityInfoW 会把可继承的项传给已有的子项。
        let rc = unsafe {
            SetNamedSecurityInfoW(
                w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    /// 只读属性地打开（目录也能开）；`follow = false` 时不跟随最后一段的重解析点（符号链接、junction）。
    pub(crate) fn open_attrs(path: &Path, follow: bool) -> io::Result<File> {
        let w = wide(path.as_os_str());
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        // SAFETY: `w` 以 NUL 结尾；其余参数是常量或空指针。
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 刚打开的有效句柄，由 File 接管。
        Ok(unsafe { File::from_raw_handle(h) })
    }

    pub(super) fn handle_identity(f: &File) -> io::Result<(u64, u64)> {
        // SAFETY: 全零是这个 C 结构体的合法值，由调用写入。
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: 句柄在 `f` 活着期间有效。
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }

    pub(super) fn file_identity(path: &Path, follow: bool) -> io::Result<(u64, u64)> {
        handle_identity(&open_attrs(path, follow)?)
    }

    pub(super) fn hostname() -> Option<String> {
        let mut buf = vec![0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `len` 是缓冲的长度（以 u16 计），成功时写入的长度（不含 NUL）回填到 `len`。
        if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) } == 0 {
            return std::env::var("COMPUTERNAME").ok();
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub(super) fn disk_free(p: &Path) -> Option<u64> {
        let w = wide(p.as_os_str());
        let mut avail = 0u64;
        // SAFETY: `w` 以 NUL 结尾；只要第一个输出。
        if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return None;
        }
        Some(avail)
    }

    /// 宽字符缓冲（不含 NUL）转成路径。
    pub(crate) fn path_from_wide(w: &[u16]) -> PathBuf {
        PathBuf::from(std::ffi::OsString::from_wide(w))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sddl_restricts_to_owner_system_admins() {
            assert_eq!(
                owner_only_sddl("S-1-5-21-1", true),
                "D:P(A;OICI;FA;;;S-1-5-21-1)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
            );
            assert!(!owner_only_sddl("S-1-5-21-1", false).contains("OICI"));
        }

        #[test]
        fn current_user_sid_and_owner_only_dir() {
            let sid = current_user_sid().unwrap();
            assert!(sid.starts_with("S-1-"), "{sid}");
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path().join("private");
            std::fs::create_dir(&d).unwrap();
            restrict_to_owner(&d).unwrap();
            std::fs::write(d.join("f"), "x").unwrap();
            assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "x");
        }

        #[test]
        fn identity_matches_for_two_spellings_of_the_same_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path().join("Dir");
            std::fs::create_dir(&d).unwrap();
            let a = file_identity(&d, true).unwrap();
            let b = file_identity(&tmp.path().join("dir"), true).unwrap();
            assert_eq!(a, b);
            std::fs::create_dir(tmp.path().join("other")).unwrap();
            assert_ne!(a, file_identity(&tmp.path().join("other"), true).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_and_write_at_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        write_all_at(&f, b"world", 6).unwrap();
        write_all_at(&f, b"hello ", 0).unwrap();
        let mut buf = [0u8; 5];
        read_exact_at(&f, &mut buf, 6).unwrap();
        assert_eq!(&buf, b"world");
        assert!(read_exact_at(&f, &mut [0u8; 4], 9).is_err(), "past the end");
    }

    #[test]
    fn canonical_paths_have_no_verbatim_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let c = canonicalize(tmp.path()).unwrap();
        assert!(!c.to_string_lossy().starts_with(r"\\?\"), "{}", c.display());
        assert!(c.is_absolute());
    }

    #[test]
    fn identity_is_stable_and_distinguishes_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        assert_eq!(identity_of(&a), identity_of(&a));
        assert_ne!(identity_of(&a), identity_of(&b));
        let md = std::fs::metadata(&a).unwrap();
        assert_eq!(file_identity(&md, &a, true), identity_of(&a));
        let f = File::open(&a).ok();
        if let Some(f) = f {
            assert_eq!(handle_identity(&f).ok(), identity_of(&a));
        }
    }

    #[test]
    fn hostname_is_known() {
        assert!(hostname().is_some_and(|h| !h.is_empty()));
    }
}
