//! `anyconw.exe`：不带控制台窗口地运行同目录下的 `anycon.exe`，参数原样传过去，退出码原样返回（docs/decisions.md ADR-42）。
//!
//! 计划任务直接运行控制台程序会弹出一个控制台窗口，关掉它 daemon 就停了。这个程序是 GUI 子系统的（自己没有控制台），用
//! `CREATE_NO_WINDOW` 拉起 `anycon.exe`：后者有一个看不见的控制台，它再拉起的 git、agent 等控制台程序沿用这个控制台，也不弹窗。
//! `anycon.exe` 放进一个 `KILL_ON_JOB_CLOSE` 的 Job Object：本进程被结束（`schtasks /End`）时它跟着结束，它的 agent 也随它的
//! job 一起被清掉。
//!
//! 没有控制台，出错时没处打印：找不到 / 起不来 `anycon.exe` 时以退出码 2 结束，计划任务的「上次运行结果」里看得到。
//! 其它平台上没有用处，只打印一句话退出。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("anyconw is only used on Windows (it starts anycon.exe without a console window); run `anycon` instead");
    std::process::exit(2);
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    pub(super) fn run() -> i32 {
        let Ok(me) = std::env::current_exe() else { return 2 };
        let mut cmd = Command::new(me.with_file_name("anycon.exe"));
        cmd.args(std::env::args_os().skip(1))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let Ok(mut child) = cmd.spawn() else { return 2 };
        // 放不进 job 也照样等：只是本进程被强制结束时 anycon.exe 不会跟着结束
        let _job = kill_on_close_job().and_then(|job| {
            // SAFETY: 两个句柄在调用期间都有效。
            let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } != 0;
            ok.then_some(job)
        });
        match child.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        }
    }

    fn kill_on_close_job() -> Option<OwnedHandle> {
        // SAFETY: 默认安全属性、匿名 job；返回的句柄由 OwnedHandle 接管。
        let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if h.is_null() {
            return None;
        }
        // SAFETY: 刚创建的、我们独占的句柄。
        let job = unsafe { OwnedHandle::from_raw_handle(h) };
        // SAFETY: 全零是这个 C 结构体的合法值；只设置 LimitFlags。
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 指针和长度描述的就是 `info`。
        let ok = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        (ok != 0).then_some(job)
    }
}
