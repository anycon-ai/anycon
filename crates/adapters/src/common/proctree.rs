//! 子进程连同它派生的进程一起清理（agent 拉起的工具进程、shim 再起的一层 node、dev server 之类）。
//!
//! - **unix**：子进程放进自己的进程组（`process_group(0)`），清理 = `killpg(SIGKILL)`。进程组按 pid 寻址，leader 被回收之后
//!   pid 可能被复用，所以调用方要经 `child::Reaped` 决定还能不能发（见那里的说明）。
//! - **Windows**：子进程放进一个 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），清理 = `TerminateJobObject`；最后一个句柄关闭时
//!   系统也会清掉 job 里剩下的进程，所以 daemon 崩溃也不留孤儿。按句柄寻址，没有 pid 复用的问题。`spawn` 和放进 job 之间有一个
//!   很短的窗口，子进程在这期间派生的进程不在 job 里（agent 从启动到第一次派生工具进程远比这个窗口长）。放不进 job 时（极少见）
//!   退回只终止 leader。
//!   另外设 `CREATE_NO_WINDOW`：子进程有自己的、不可见的控制台，不和 daemon 共享——终端里按 Ctrl+C 不会直接打到 agent 上
//!   （unix 上进程组起同样的作用），daemon 没有控制台时（由服务拉起）也不会给每个子进程弹一个窗口。
//!
//! Windows 上没有「只发给一个进程的 SIGINT」：控制台的 Ctrl+C 事件按控制台广播，而子进程不和 daemon 共享控制台，
//! 所以 [`Tree::interrupt`] 在 Windows 上什么也不做。
//!
//! Windows 上 job 就是「进程组」：预览的端口归属（daemon 的 `preview::ports`）用 [`job_members`] 查某个 agent 的 job 里现在有哪些
//! 进程，相当于 unix 上按 pgid 归属——Windows 不给孤儿进程换父进程，父进程链断了的后台进程也还在 job 里。

use tokio::process::{Child, Command};

/// 拉起之前调用：unix 放进新进程组，Windows 不共享 daemon 的控制台。
pub fn isolate(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

/// 一个子进程及其派生进程的清理句柄。拉起之后立刻 [`Tree::attach`]。
pub struct Tree {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<std::sync::Arc<win::Job>>,
    /// 放不进 job 时的退路：leader 的进程句柄（持有句柄期间进程对象不会被复用）。
    #[cfg(windows)]
    leader: Option<std::os::windows::io::OwnedHandle>,
}

impl Tree {
    pub fn attach(child: &Child) -> Tree {
        #[cfg(unix)]
        {
            Tree { pid: child.id() }
        }
        #[cfg(windows)]
        {
            win::attach(child)
        }
    }

    /// 杀掉整棵进程树。unix 上调用方负责「leader 被回收之后不再调用」（除了回收后的那一次清理，见 `child::Reaped`）；
    /// Windows 上任何时候调用都安全。
    pub fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::sys::signal::{killpg, Signal};
            use nix::unistd::Pid;
            #[cfg(test)]
            KILL_LOG.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
            // 进程组已经空了（`ESRCH`）是正常情况
            if let Err(e) = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL) {
                if e != nix::errno::Errno::ESRCH {
                    tracing::debug!(pid, error = %e, "killpg failed");
                }
            }
        }
        #[cfg(windows)]
        win::kill(self);
    }

    /// 只对 leader 发 SIGINT（unix），让它自己收尾。Windows 上不做事（模块说明）。
    pub fn interrupt(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGINT) {
                tracing::debug!(pid, error = %e, "SIGINT failed");
            }
        }
    }
}

/// 现在活着的进程树：`(leader 的 pid, job 里所有进程的 pid)`。只有 Windows 有（unix 返回空，按 pgid 归属即可）。
pub fn job_members() -> Vec<(u32, Vec<u32>)> {
    #[cfg(windows)]
    {
        win::members()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 测试里记下每次 [`Tree::kill`] 的 pid（进程内共享，按 pid 过滤用）。
#[cfg(all(test, unix))]
pub(crate) static KILL_LOG: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
mod win {
    use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
    use std::sync::{Arc, Mutex, Weak};

    use tokio::process::Child;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::TerminateProcess;

    use super::Tree;

    /// 活着的 job（leader pid → job）。`Tree` 持有强引用，这里只是弱引用，Tree 丢掉之后自然失效。
    static LIVE: Mutex<Vec<(u32, Weak<Job>)>> = Mutex::new(Vec::new());

    /// 一次最多列多少个进程（超出的部分不列，归属只是少认几个进程）。
    const MAX_MEMBERS: usize = 4096;

    /// 带 `KILL_ON_JOB_CLOSE` 的 Job Object。句柄不可继承，只有我们持有。
    pub(super) struct Job(OwnedHandle);

    impl Job {
        fn new() -> std::io::Result<Job> {
            // SAFETY: 两个参数都可以为空（默认安全属性、匿名 job）；返回的句柄由 OwnedHandle 接管。
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `h` 是刚创建的、我们独占的有效句柄。
            let job = Job(unsafe { OwnedHandle::from_raw_handle(h) });
            // SAFETY: 全零是这个 C 结构体的合法值；只设置 LimitFlags。
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: 传入的指针和长度描述的就是 `info`，调用期间有效。
            let ok = unsafe {
                SetInformationJobObject(
                    job.0.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }

        fn assign(&self, process: BorrowedHandle<'_>) -> std::io::Result<()> {
            // SAFETY: 两个句柄在调用期间都有效。
            if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process.as_raw_handle()) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        fn pids(&self) -> Vec<u32> {
            let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let bytes = header + MAX_MEMBERS * std::mem::size_of::<usize>();
            // usize 对齐的缓冲
            let mut buf = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
            // SAFETY: 缓冲长度如实传入；成功时开头是 JOBOBJECT_BASIC_PROCESS_ID_LIST，后面接着 pid 数组。
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    bytes as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Vec::new();
            }
            // SAFETY: 见上；NumberOfProcessIdsInList 不超过缓冲里放得下的个数。
            unsafe {
                let list = buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST;
                let n = ((*list).NumberOfProcessIdsInList as usize).min(MAX_MEMBERS);
                let first = std::ptr::addr_of!((*list).ProcessIdList) as *const usize;
                std::slice::from_raw_parts(first, n).iter().map(|p| *p as u32).collect()
            }
        }
    }

    pub(super) fn members() -> Vec<(u32, Vec<u32>)> {
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|(_, w)| w.strong_count() > 0);
        live.iter().filter_map(|(pid, w)| w.upgrade().map(|j| (*pid, j.pids()))).collect()
    }

    pub(super) fn attach(child: &Child) -> Tree {
        let Some(raw) = child.raw_handle() else { return Tree { job: None, leader: None } };
        // SAFETY: `raw` 是 `child` 持有的进程句柄，在 `child` 活着期间有效；这里只借用来复制 / 放进 job。
        let process = unsafe { BorrowedHandle::borrow_raw(raw) };
        let leader = process.try_clone_to_owned().ok();
        match Job::new().and_then(|job| job.assign(process).map(|_| job)) {
            Ok(job) => {
                let job = Arc::new(job);
                if let Some(pid) = child.id() {
                    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
                    live.retain(|(_, w)| w.strong_count() > 0);
                    live.push((pid, Arc::downgrade(&job)));
                }
                Tree { job: Some(job), leader }
            }
            Err(e) => {
                tracing::warn!(pid = ?child.id(), error = %e, "cannot put the child into a job object; only the process itself will be terminated");
                Tree { job: None, leader }
            }
        }
    }

    pub(super) fn kill(tree: &Tree) {
        if let Some(job) = &tree.job {
            // SAFETY: job 句柄由 `tree` 持有，有效。
            if unsafe { TerminateJobObject(job.0.as_raw_handle(), 1) } != 0 {
                return;
            }
            tracing::debug!(error = %std::io::Error::last_os_error(), "TerminateJobObject failed");
        }
        if let Some(p) = &tree.leader {
            // SAFETY: 进程句柄由 `tree` 持有，有效；进程已退出时调用只是失败，没有副作用。
            unsafe { TerminateProcess(p.as_raw_handle(), 1) };
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    /// `cmd /c` 再起一个长驻的 `ping`：杀掉树之后孙进程也不在了（它拿着的 stdout 管道随之关闭）。
    #[tokio::test]
    async fn kill_takes_down_grandchildren() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL & ping -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        isolate(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let tree = Tree::attach(&child);
        assert!(tree.job.is_some(), "the child went into a job object");
        let pid = child.id().unwrap();
        let members = job_members().into_iter().find(|(p, _)| *p == pid).map(|(_, m)| m).unwrap();
        assert!(members.contains(&pid), "{members:?}");
        let mut out = child.stdout.take().unwrap();
        tree.kill();
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
        assert!(!status.success());
        // 所有写端（包括孙进程继承的那份）都关了，读到 EOF
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), tokio::io::AsyncReadExt::read_to_end(&mut out, &mut buf))
            .await
            .expect("grandchild is gone, the pipe closes")
            .unwrap();
    }
}
