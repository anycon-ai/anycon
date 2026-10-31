//! `anycon passwd`：节点口令（ADR-36，docs/spec/cli.md § 3.2）。走本机 socket，不经 hub。
//!
//! - 不带动作：设置 / 修改。交互输入两次、不回显；不是终端时必须带 `--stdin`（否则退出码 2）。口令**不接受命令行参数**，
//!   免得进 shell 历史和进程列表；
//! - `--clear [--yes]`：清除；`--enable` / `--disable`：启用 / 停用口令解锁（被锁定时 `--enable` 先打印最近的失败记录，
//!   确认后解除锁定、失败计数清零）；`--status [--json]`：状态。
//!
//! 每次改动 daemon 都会通知所有已配对设备并记审计；口令和它的派生值都不进日志。

use std::io::{BufRead, IsTerminal, Write};

use anycon_protocol::{
    methods, AuditEntry, EnrollPasswordState, NodeAuditResult, NodeEnrollStatusResult, NodeSetEnrollEnabledParams,
    NodeSetEnrollPasswordParams,
};
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

use crate::cli::PasswdArgs;
use crate::commands::App;
use crate::error::{CliError, Result};
use crate::output::{ago, sanitize_label};

/// 读口令用的缓冲上限（字节）：daemon 最多收 1024 字节，这里留一倍余量；再多的输入被丢掉。
const SECRET_BUF_BYTES: usize = 2048;

/// 逐字节编辑一行口令的缓冲：容量一次分配、**从不缩容也不重新分配**（重新分配会在旧位置留下一份没清零的拷贝），
/// 退格、Ctrl+U 退掉的字节当场清零，整个缓冲 Drop 时再整体清零。
struct SecretBuf {
    bytes: Zeroizing<Vec<u8>>,
    len: usize,
}

impl SecretBuf {
    fn new() -> SecretBuf {
        SecretBuf { bytes: Zeroizing::new(vec![0; SECRET_BUF_BYTES]), len: 0 }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, b: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = b;
            self.len += 1;
        }
    }

    /// 退一个字符（UTF-8 续字节一起退），退掉的字节清零。
    fn backspace(&mut self) {
        while self.len > 0 {
            self.len -= 1;
            let continuation = self.bytes[self.len] & 0xC0 == 0x80;
            self.bytes[self.len] = 0;
            if !continuation {
                break;
            }
        }
    }

    /// Ctrl+U：整行清掉。
    fn clear(&mut self) {
        let len = std::mem::take(&mut self.len);
        self.bytes[..len].zeroize();
    }

    /// 输入的内容（必须是合法 UTF-8）；结果在新的 `Zeroizing<String>` 里，缓冲随后由 Drop 清零。
    fn into_string(self) -> std::result::Result<Zeroizing<String>, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes[..self.len]).map(|s| Zeroizing::new(s.to_owned()))
    }

    /// 测试用：缓冲的全部内容（含已退掉的位置，应该是零）。
    #[cfg(test)]
    fn raw(&self) -> &[u8] {
        &self.bytes
    }
}

/// 编辑一行口令：`read_byte` 逐字节给输入（`None` = 输入结束）。退格、Ctrl+U 可用，Ctrl+C 是中断（退出码 130），
/// 空行上的 `eof` 字节（终端 Ctrl+D，Windows 控制台 Ctrl+Z）是输入结束。`vt_escapes`：终端会把方向键等发成 `ESC [ …`
/// 转义序列，要整段吃掉；Windows 控制台不发（只有单独的 ESC），不能往后多读。
fn edit_secret_line(
    mut read_byte: impl FnMut() -> Result<Option<u8>>,
    eof: u8,
    vt_escapes: bool,
) -> Result<Zeroizing<String>> {
    let mut bytes = SecretBuf::new();
    let result = loop {
        let Some(b) = read_byte()? else {
            if bytes.is_empty() {
                break Err(CliError::general("end of input"));
            }
            break Ok(());
        };
        match b {
            b'\n' | b'\r' => break Ok(()),
            3 => break Err(CliError::interrupted()),
            c if c == eof && bytes.is_empty() => break Err(CliError::general("end of input")),
            0x7f | 0x08 => bytes.backspace(),
            0x15 => bytes.clear(),
            0x1b if vt_escapes => {
                // 方向键之类的转义序列：吃到最后一个字节
                if let Some(b'[' | b'O') = read_byte()? {
                    while let Some(c) = read_byte()? {
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
            }
            c if c < 0x20 => {}
            c => bytes.push(c),
        }
    };
    eprintln!();
    result?;
    bytes.into_string().map_err(|e| CliError::usage(format!("the password is not valid UTF-8: {e}")))
}

/// 不回显地读一行口令（`fd` 必须是终端）。原始模式逐字节读（编辑规则见 [`edit_secret_line`]）。
/// 不管怎么返回，终端都会恢复原来的设置。
#[cfg(unix)]
pub fn read_secret_from(fd: i32, prompt: &str) -> Result<Zeroizing<String>> {
    // SAFETY: tcgetattr 只写入我们提供的结构体。
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        return Err(CliError::usage("standard input is not a terminal; pass the password with --stdin"));
    }
    struct Restore {
        fd: i32,
        orig: libc::termios,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: 用原来 tcgetattr 拿到的设置恢复。
            unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig) };
        }
    }
    let mut raw = orig;
    raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    let _restore = Restore { fd, orig };
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    // SAFETY: 同上。
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
        return Err(CliError::general("cannot turn off terminal echo"));
    }
    let read_byte = || -> Result<Option<u8>> {
        let mut b = [0u8; 1];
        loop {
            // SAFETY: 读一个字节到我们的缓冲区。
            let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
            match n {
                1 => return Ok(Some(b[0])),
                0 => return Ok(None),
                _ => {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(CliError::general("cannot read from the terminal"));
                }
            }
        }
    };
    edit_secret_line(read_byte, 4, true)
}

/// Windows 控制台：关掉回显、行输入和 Ctrl+C 处理（Ctrl+C 作为 0x03 读进来），用 `ReadConsoleW` 逐个读 UTF-16 字符，
/// 转成 UTF-8 字节交给 [`edit_secret_line`]。不管怎么返回，控制台模式都会恢复。
#[cfg(windows)]
fn read_secret_console(prompt: &str) -> Result<Zeroizing<String>> {
    use std::collections::VecDeque;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, ReadConsoleW, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
    };

    // SAFETY: 取本进程的标准输入句柄，不转移所有权。
    let h: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut orig: CONSOLE_MODE = 0;
    // SAFETY: 只写入 `orig`；不是控制台时失败。
    if unsafe { GetConsoleMode(h, &mut orig) } == 0 {
        return Err(CliError::usage("standard input is not a terminal; pass the password with --stdin"));
    }
    struct Restore(HANDLE, CONSOLE_MODE);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: 用原来的模式恢复。
            unsafe { SetConsoleMode(self.0, self.1) };
        }
    }
    let _restore = Restore(h, orig);
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    // SAFETY: 同上。
    if unsafe { SetConsoleMode(h, orig & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT)) } == 0 {
        return Err(CliError::general("cannot turn off console echo"));
    }
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut high: Option<u16> = None;
    let read_byte = || -> Result<Option<u8>> {
        loop {
            if let Some(b) = pending.pop_front() {
                return Ok(Some(b));
            }
            let mut unit = [0u16; 1];
            let mut n = 0u32;
            // SAFETY: 读一个 UTF-16 单元到 `unit`。
            if unsafe { ReadConsoleW(h, unit.as_mut_ptr().cast(), 1, &mut n, std::ptr::null()) } == 0 {
                return Err(CliError::general("cannot read from the console"));
            }
            if n == 0 {
                return Ok(None);
            }
            let u = unit[0];
            let units: Vec<u16> = match (high.take(), u) {
                (None, 0xD800..=0xDBFF) => {
                    high = Some(u);
                    continue;
                }
                (Some(h), 0xDC00..=0xDFFF) => vec![h, u],
                (_, u) => vec![u],
            };
            let mut buf = [0u8; 4];
            for c in char::decode_utf16(units).filter_map(|c| c.ok()) {
                pending.extend(c.encode_utf8(&mut buf).bytes());
            }
            buf.zeroize();
        }
    };
    let out = edit_secret_line(read_byte, 0x1a, false);
    pending.iter_mut().for_each(|b| *b = 0);
    out
}

/// 在标准输入（终端）上不回显地读一行。
pub fn read_secret(prompt: &str) -> Result<Zeroizing<String>> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::usage("standard input is not a terminal"));
    }
    #[cfg(unix)]
    {
        read_secret_from(libc::STDIN_FILENO, prompt)
    }
    #[cfg(windows)]
    {
        read_secret_console(prompt)
    }
}

/// 从标准输入读一行（`--stdin`）：去掉行尾的 `\n` / `\r\n`，其余原样。
pub fn read_stdin_line() -> Result<Zeroizing<String>> {
    // 先留够容量：一行口令通常不会让缓冲重新分配（重新分配会在旧位置留下一份没清零的拷贝）
    let mut line = Zeroizing::new(String::with_capacity(SECRET_BUF_BYTES));
    let n = std::io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        return Err(CliError::usage("--stdin: no password on standard input"));
    }
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    Ok(line)
}

/// 问一句 y/N（终端上）。
pub fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} (y/N): ");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// 设置口令前的本地检查（和 daemon 同一条规则：NFC 之后至少 8 个字符）。
fn check_length(pw: &str) -> Result<()> {
    anycon_crypto::validate_enroll_password(pw)
        .map_err(|_| CliError::usage("the password must have at least 8 characters"))
}

pub async fn passwd(app: &App, a: PasswdArgs) -> Result<u8> {
    if a.status {
        return status(app).await;
    }
    if a.clear {
        return clear(app, a.yes).await;
    }
    if a.enable {
        return enable(app, a.yes).await;
    }
    if a.disable {
        return disable(app).await;
    }
    set(app, a.stdin).await
}

async fn set(app: &App, from_stdin: bool) -> Result<u8> {
    // 先收口令、再连 daemon：不是终端又没带 --stdin 是参数问题（2），和 daemon 在不在无关
    let pw = if from_stdin {
        read_stdin_line()?
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::usage(
                "standard input is not a terminal; pass the password on standard input with --stdin (it is never taken from the command line)",
            ));
        }
        let first = read_secret("New node password (at least 8 characters): ")?;
        check_length(&first)?;
        let again = read_secret("Repeat the password: ")?;
        if *first != *again {
            return Err(CliError::usage("the two passwords do not match"));
        }
        first
    };
    check_length(&pw)?;
    let conn = app.local_conn().await?;
    conn.call_raw(methods::NODE_SET_ENROLL_PASSWORD, NodeSetEnrollPasswordParams { password: Some((*pw).clone()) })
        .await?;
    if app.json() {
        app.out.json_line(&json!({ "password": "set" }));
    } else {
        println!("node password set");
        println!(
            "With it you can unlock this node from a new device when no paired device is at hand (`anycon connect <hub-url> --via password`, or the unlock page in the web UI)."
        );
    }
    Ok(0)
}

async fn clear(app: &App, yes: bool) -> Result<u8> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::usage("this asks for confirmation; use --yes in scripts"));
        }
        if !confirm("Remove the node password? Password unlock will stop working") {
            return Err(CliError::general("aborted"));
        }
    }
    let conn = app.local_conn().await?;
    conn.call_raw(methods::NODE_SET_ENROLL_PASSWORD, NodeSetEnrollPasswordParams { password: None }).await?;
    if app.json() {
        app.out.json_line(&json!({ "password": "cleared" }));
    } else {
        println!("node password removed");
    }
    Ok(0)
}

async fn disable(app: &App) -> Result<u8> {
    let conn = app.local_conn().await?;
    conn.call_raw(
        methods::NODE_SET_ENROLL_ENABLED,
        NodeSetEnrollEnabledParams { approval: None, password: Some(false) },
    )
    .await?;
    if app.json() {
        app.out.json_line(&json!({ "enroll": { "password": false } }));
    } else {
        println!(
            "password unlock is turned off (the password itself is kept; `anycon passwd --enable` turns it back on)"
        );
    }
    Ok(0)
}

/// 最近的口令失败记录（`node.audit` 里的 `enroll.failed`）：时间、自报的设备名（未经认证）。
fn recent_failures(entries: &[AuditEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.action == "enroll.failed" && e.detail.get("method").and_then(Value::as_str) == Some("password"))
        .filter(|e| e.detail.get("failures").is_some())
        .take(8)
        .map(|e| {
            let name = e.device_name.trim_end_matches(" (unverified)");
            format!("  {}  {} (self-reported)", ago(e.ts_ms), sanitize_label(name, 64, "unknown device"))
        })
        .collect()
}

async fn enable(app: &App, yes: bool) -> Result<u8> {
    let conn = app.local_conn().await?;
    let st: NodeEnrollStatusResult = conn.call(methods::NODE_ENROLL_STATUS, json!({})).await?;
    if st.password.state == EnrollPasswordState::Locked {
        // 被锁定：先给人看是谁在猜，确认是自己输错的再解除
        let audit: NodeAuditResult = conn.call(methods::NODE_AUDIT, json!({ "limit": 300 })).await?;
        let lines = recent_failures(&audit.entries);
        if !app.json() {
            println!(
                "Password unlock is locked after {} failed attempts in a row. Recent failed attempts:",
                st.password.failures
            );
            if lines.is_empty() {
                println!("  (none in the audit log)");
            }
            for l in &lines {
                println!("{l}");
            }
        }
        if !yes {
            if !std::io::stdin().is_terminal() {
                return Err(CliError::usage(
                    "the password unlock is locked; look at the failed attempts (`anycon audit`) and re-run with --yes to unlock it",
                ));
            }
            if !confirm("Were these you? Unlock and reset the failure count") {
                return Err(CliError::general("aborted"));
            }
        }
    }
    conn.call_raw(
        methods::NODE_SET_ENROLL_ENABLED,
        NodeSetEnrollEnabledParams { approval: None, password: Some(true) },
    )
    .await?;
    if app.json() {
        app.out.json_line(
            &json!({ "enroll": { "password": true }, "unlocked": st.password.state == EnrollPasswordState::Locked }),
        );
    } else if st.password.state == EnrollPasswordState::Locked {
        println!("password unlock is on again; the failure count was reset");
    } else {
        println!("password unlock is turned on");
    }
    Ok(0)
}

/// `--status` 的人类可读输出。
pub fn render_status(st: &NodeEnrollStatusResult) -> String {
    let p = &st.password;
    let mut out = String::new();
    out.push_str(&format!(
        "node password:   {}\n",
        if p.set {
            format!(
                "set{}{}",
                p.set_at_ms.map(|t| format!(" {}", ago(t))).unwrap_or_default(),
                p.set_by.as_ref().map(|b| format!(", by {}", sanitize_label(b, 40, "?"))).unwrap_or_default()
            )
        } else {
            "not set".to_string()
        }
    ));
    let state = match p.state {
        EnrollPasswordState::Enabled => "enabled",
        EnrollPasswordState::Disabled => "disabled",
        EnrollPasswordState::Locked => "locked (re-enable with `anycon passwd --enable`)",
    };
    out.push_str(&format!("password unlock: {state}\n"));
    out.push_str(&format!("failed attempts: {} in a row", p.failures));
    if let Some(ms) = p.retry_after_ms {
        out.push_str(&format!(" (the next attempt is allowed in {} s)", ms.div_ceil(1000)));
    }
    out.push('\n');
    out.push_str(&format!("scan / join code: {}\n", if st.approval { "on" } else { "off" }));
    out
}

async fn status(app: &App) -> Result<u8> {
    let conn = app.local_conn().await?;
    let st: NodeEnrollStatusResult = conn.call(methods::NODE_ENROLL_STATUS, json!({})).await?;
    if app.json() {
        app.out.json_line(&st);
    } else {
        print!("{}", render_status(&st));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::EnrollPasswordStatus;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;

    /// 一对伪终端 (master, slave)。
    #[cfg(unix)]
    fn pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let (mut m, mut s) = (0i32, 0i32);
        // SAFETY: openpty 写两个 fd。
        let rc =
            unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        // SAFETY: 刚拿到的 fd，所有权交给 File。
        unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s)) }
    }

    #[cfg(unix)]
    fn echo_on(fd: i32) -> bool {
        // SAFETY: tcgetattr 写我们的结构体。
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(fd, &mut t) };
        assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        t.c_lflag & libc::ECHO != 0
    }

    #[cfg(unix)]
    /// 在 pty 上读一个口令：`typed` 是用户敲的字节。返回 (读到的结果, master 上看到的回显, 读完后终端回显是不是又开了)。
    fn type_into_prompt(typed: &'static [u8]) -> (Result<Zeroizing<String>>, Vec<u8>, bool) {
        use std::io::{Read, Write};
        let (mut master, slave) = pty();
        let fd = slave.as_raw_fd();
        assert!(echo_on(fd), "a fresh terminal echoes");
        let feeder = std::thread::spawn(move || {
            // 等它把回显关掉（TCSAFLUSH 会丢掉之前写进去的输入），再敲
            let t = std::time::Instant::now();
            while echo_on(fd) {
                assert!(t.elapsed() < std::time::Duration::from_secs(5), "echo was never turned off");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            master.write_all(typed).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            // 非阻塞读 master：应该什么回显都没有（提示符是写到 stderr 的，不在 pty 里）
            // SAFETY: 设置非阻塞。
            unsafe {
                let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
            let mut echoed = Vec::new();
            let mut buf = [0u8; 256];
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                echoed.extend_from_slice(&buf[..n]);
            }
            (echoed, master)
        });
        let r = read_secret_from(fd, "");
        // master 要活到查完回显之后：Linux 上 master 一关 slave 就被挂断，tcgetattr 返回 EIO
        let (echoed, master) = feeder.join().unwrap();
        let restored = echo_on(fd);
        drop(master);
        drop(slave);
        (r, echoed, restored)
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_is_read_without_echo_with_editing_keys_and_the_terminal_is_restored() {
        // 退格删一个字符（多字节字符整个删），Ctrl+U 清行，方向键被吃掉
        let (r, echoed, restored) = type_into_prompt("hunter3\x7f2é\x7f!\r".as_bytes());
        assert_eq!(&*r.unwrap(), "hunter2!");
        assert!(echoed.is_empty(), "nothing is echoed back: {echoed:?}");
        assert!(restored, "the terminal settings are restored");
        let (r, _, _) = type_into_prompt(b"wrong\x15right pass\n");
        assert_eq!(&*r.unwrap(), "right pass");
        let (r, _, _) = type_into_prompt(b"ab\x1b[Acd\n");
        assert_eq!(&*r.unwrap(), "abcd", "arrow keys are not part of the password");
        let (r, _, _) = type_into_prompt("密码密码密码密码\n".as_bytes());
        assert_eq!(&*r.unwrap(), "密码密码密码密码");
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_c_and_end_of_input_stop_the_prompt_and_still_restore_the_terminal() {
        let (r, _, restored) = type_into_prompt(b"half\x03");
        let e = r.unwrap_err();
        assert_eq!(e.exit, crate::error::EXIT_INTERRUPTED);
        assert!(restored);
        let (r, _, restored) = type_into_prompt(b"\x04");
        assert!(r.is_err() && restored);
    }

    /// 退格、Ctrl+U 退掉的字节当场清零，缓冲不缩容也不重新分配（旧位置不会留下没清零的拷贝）。
    #[test]
    fn bytes_taken_back_while_editing_a_secret_are_zeroed_on_the_spot() {
        let mut b = SecretBuf::new();
        let addr = b.raw().as_ptr();
        for c in b"hunter3" {
            b.push(*c);
        }
        b.backspace();
        assert_eq!(&b.raw()[..8], b"hunter\0\0", "the erased `3` is gone from memory");
        // 多字节字符整个退，续字节也清零
        for c in "é".as_bytes() {
            b.push(*c);
        }
        assert_eq!(&b.raw()[..8], "hunteré".as_bytes());
        b.backspace();
        assert!(b.raw()[6..].iter().all(|x| *x == 0), "both bytes of the erased character are zeroed");
        // Ctrl+U：整行清零
        b.clear();
        assert!(b.raw().iter().all(|x| *x == 0) && b.is_empty());
        // 多出来的输入被丢掉，缓冲没有搬家
        for _ in 0..SECRET_BUF_BYTES + 100 {
            b.push(b'x');
        }
        assert_eq!(b.raw().len(), SECRET_BUF_BYTES);
        assert_eq!(b.raw().as_ptr(), addr, "the buffer never reallocates");
        assert_eq!(b.into_string().unwrap().len(), SECRET_BUF_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn something_that_is_not_a_terminal_is_a_usage_error() {
        let f = std::fs::File::open("/dev/null").unwrap();
        let e = read_secret_from(f.as_raw_fd(), "x").unwrap_err();
        assert_eq!(e.exit, crate::error::EXIT_USAGE);
    }

    /// 编辑规则本身（不经过终端）：退格、Ctrl+U、Ctrl+C、空行上的 EOF 字节、转义序列。
    #[test]
    fn line_editing_rules() {
        let feed = |input: &'static [u8], eof: u8, vt: bool| {
            let mut it = input.iter().copied();
            edit_secret_line(move || Ok(it.next()), eof, vt)
        };
        assert_eq!(feed(b"ab\x7fc\r", 4, true).unwrap().as_str(), "ac");
        assert_eq!(feed(b"abc\x15xy\n", 4, true).unwrap().as_str(), "xy");
        assert_eq!(feed("p\u{e4}ss\x08\r".as_bytes(), 4, true).unwrap().as_str(), "p\u{e4}s");
        assert_eq!(
            feed(b"a\x1b[Db\r", 4, true).unwrap().as_str(),
            "ab",
            "a terminal escape sequence is swallowed whole"
        );
        assert_eq!(feed(b"a\x1bb\r", 0x1a, false).unwrap().as_str(), "ab", "a console sends a lone ESC");
        assert_eq!(feed(b"ab\x03", 4, true).unwrap_err().exit, crate::error::EXIT_INTERRUPTED);
        assert!(feed(b"\x04", 4, true).is_err());
        assert!(feed(b"\x1a", 0x1a, false).is_err());
        assert_eq!(
            feed(b"a\x1a\r", 4, true).unwrap().as_str(),
            "a",
            "another platform's EOF byte is just a control character"
        );
        assert!(feed(b"", 4, true).is_err());
    }

    #[test]
    fn the_local_length_check_matches_the_daemon_rule() {
        assert!(check_length("1234567").is_err());
        assert!(check_length("12345678").is_ok());
        assert!(check_length("一二三四五六七八").is_ok());
        assert!(check_length("一二三四五六七").is_err());
    }

    fn status_of(p: EnrollPasswordStatus, approval: bool) -> NodeEnrollStatusResult {
        NodeEnrollStatusResult { approval, password: p }
    }

    #[test]
    fn the_status_text_covers_set_unset_backoff_locked_and_disabled() {
        let unset = status_of(
            EnrollPasswordStatus {
                set: false,
                state: EnrollPasswordState::Enabled,
                failures: 0,
                retry_after_ms: None,
                set_at_ms: None,
                set_by: None,
            },
            true,
        );
        let t = render_status(&unset);
        assert!(t.contains("not set") && t.contains("enabled") && t.contains("0 in a row") && t.contains("on"), "{t}");
        let backing_off = status_of(
            EnrollPasswordStatus {
                set: true,
                state: EnrollPasswordState::Enabled,
                failures: 4,
                retry_after_ms: Some(90_500),
                set_at_ms: Some(anycon_daemon::util::now_ms()),
                set_by: Some("local".into()),
            },
            false,
        );
        let t = render_status(&backing_off);
        assert!(t.contains("set just now, by local") && t.contains("4 in a row") && t.contains("91 s"), "{t}");
        assert!(t.contains("scan / join code: off"), "{t}");
        let locked = status_of(
            EnrollPasswordStatus { state: EnrollPasswordState::Locked, failures: 10, ..backing_off.password.clone() },
            true,
        );
        assert!(render_status(&locked).contains("locked (re-enable with `anycon passwd --enable`)"));
        let disabled =
            status_of(EnrollPasswordStatus { state: EnrollPasswordState::Disabled, ..backing_off.password }, true);
        assert!(render_status(&disabled).contains("disabled"));
    }

    #[test]
    fn recent_failures_list_the_password_guesses_with_the_self_reported_device() {
        let e = |action: &str, name: &str, detail: Value| AuditEntry {
            ts_ms: anycon_daemon::util::now_ms(),
            device_id: "d".into(),
            device_name: name.into(),
            action: action.into(),
            target: None,
            detail,
        };
        let entries = vec![
            e(
                "enroll.failed",
                "Chrome (unverified)",
                json!({ "method": "password", "reason": "wrong_secret", "failures": 3 }),
            ),
            // 预检拒绝（退避挡回去的）不是猜测
            e("enroll.failed", "Chrome (unverified)", json!({ "method": "password", "reason": "backoff" })),
            e("enroll.failed", "Phone (unverified)", json!({ "method": "code", "reason": "wrong_secret" })),
            e("session.create", "x", json!({})),
            e("enroll.failed", "Evil\u{1b}[2Jname (unverified)", json!({ "method": "password", "failures": 2 })),
        ];
        let lines = recent_failures(&entries);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Chrome") && lines[0].contains("self-reported"), "{lines:?}");
        assert!(!lines[1].contains('\u{1b}'), "{lines:?}");
    }
}
