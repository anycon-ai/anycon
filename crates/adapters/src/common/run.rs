//! 带超时的一次性子进程调用（`--version`、`auth status`、`type -a` 这类）。
//!
//! stdout / stderr 各只保留前 [`MAX_CAPTURE_BYTES`] 字节（超出的部分读掉丢弃，不让子进程卡在写管道上），
//! 子进程放进自己的进程组（Windows：Job Object，见 [`super::proctree`]），超时或 leader 退出时整组清掉
//! （nvm / volta 的 shim 会再起一层 node）。leader 被回收之后不再对这个 pid / pgid 发信号（pid 可能已被复用），见 `child::Reaped`。
//! cwd 缺省（目录不存在）时用系统临时目录，**绝不继承 daemon 当前所在的目录**（可能是用户的家目录或项目）。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::child::Reaped;
use super::proctree::Tree;

/// 每个输出流最多保留的字节数。
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// 正常退出时的退出码；被信号杀死时为 `None`。
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// 起不来（找不到、无权限…）时的原因。
    pub spawn_error: Option<String>,
    /// stdout 或 stderr 超过 [`MAX_CAPTURE_BYTES`]，被截断了。
    pub truncated: bool,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && self.spawn_error.is_none()
    }

    /// 一句话描述失败原因，给 `AgentIssue.message` 用。
    pub fn failure_reason(&self) -> String {
        if let Some(e) = &self.spawn_error {
            return format!("failed to start: {e}");
        }
        if self.timed_out {
            return "timed out".to_string();
        }
        let tail = super::truncate_bytes(self.stderr.trim(), 200);
        match (self.code, tail.is_empty()) {
            (Some(c), true) => format!("exited with code {c}"),
            (Some(c), false) => format!("exited with code {c}: {tail}"),
            (None, _) => "killed by signal".to_string(),
        }
    }
}

/// 读到 EOF，最多留 `cap` 字节；超出的部分读掉丢弃。返回 `(内容, 是否截断)`。
async fn read_capped<R: AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    truncated = true;
                }
            }
        }
    }
    (out, truncated)
}

/// 运行 `program args…`：清空继承的环境，只用 `env`；stdin 为空；超时即杀（整个进程组）。
pub async fn run_capture(
    program: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    cwd: &Path,
    timeout: Duration,
) -> RunOutput {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    super::proctree::isolate(&mut cmd);
    if cwd.is_dir() {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(super::neutral_cwd());
    }
    // ETXTBSY：刚写完的脚本可能被别的线程 fork 时短暂继承了写句柄（测试里常见），稍等重试。
    let mut attempts = 0;
    let mut child = loop {
        match cmd.spawn() {
            Ok(c) => break c,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return RunOutput { spawn_error: Some(e.to_string()), ..Default::default() },
        }
    };
    let tree = Tree::attach(&child);
    let (stdout, stderr) = (child.stdout.take().expect("piped stdout"), child.stderr.take().expect("piped stderr"));
    let reap = Reaped::default();
    let work = async {
        // leader 一退出就清掉进程组，免得后台进程拿着管道不放让读取一直不结束。
        // 这是回收之后唯一的一次 killpg；`reap.wait` 在回收的同一刻置标志，超时分支据此不再发信号
        let wait = async {
            let status = reap.wait(&mut child).await;
            tree.kill();
            status
        };
        tokio::join!(read_capped(stdout, MAX_CAPTURE_BYTES), read_capped(stderr, MAX_CAPTURE_BYTES), wait)
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(((out, out_cut), (err, err_cut), Ok(status))) => RunOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            timed_out: false,
            spawn_error: None,
            truncated: out_cut || err_cut,
        },
        Ok((_, _, Err(e))) => RunOutput { spawn_error: Some(e.to_string()), ..Default::default() },
        Err(_) => {
            // 超时：future 已被丢弃。leader 还没被回收就整组杀掉（kill_on_drop 只杀 leader）；
            // 已经回收了（读管道的一侧被逃出进程组的后台进程拖住的情形）就不再发信号——
            // 回收时已经清过一次进程组，这时 pid 可能已被别的进程复用
            reap.signal_if_alive(|| tree.kill());
            RunOutput { timed_out: true, ..Default::default() }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())])
    }

    #[tokio::test]
    async fn captures_stdout_and_code() {
        let out = run_capture(
            Path::new("/bin/sh"),
            &["-c".into(), "echo hi; echo oops >&2; exit 3".into()],
            &env(),
            Path::new("/"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout.trim(), "hi");
        assert_eq!(out.stderr.trim(), "oops");
        assert!(!out.success());
        assert!(out.failure_reason().contains("oops"));
    }

    #[tokio::test]
    async fn times_out() {
        let out = run_capture(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 5".into()],
            &env(),
            Path::new("/"),
            Duration::from_millis(100),
        )
        .await;
        assert!(out.timed_out);
        assert_eq!(out.failure_reason(), "timed out");
    }

    #[tokio::test]
    async fn no_killpg_on_timeout_once_the_leader_was_reaped() {
        // leader 立刻退出（被回收，回收时清一次进程组），但它留下的后台进程自己 `setsid` 逃出了进程组，
        // 还攥着 stdout 管道，所以读取一直不结束，直到超时。超时分支此时不能再对这个（可能已被复用的）pid 发 killpg。
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("leader.pid");
        let ready = tmp.path().join("escaped");
        // 等后台进程 `setsid` 完（写出 ready 文件）再让 leader 退出，免得它还在组里时被回收时那次 killpg 一并杀掉
        let script = format!(
            "echo $$ > '{pid}'; perl -e 'use POSIX; POSIX::setsid(); open(F, \">\", $ARGV[0]); close F; sleep 3' '{ready}' & \
             while [ ! -e '{ready}' ]; do sleep 0.05; done; exit 0",
            pid = pidfile.display(),
            ready = ready.display()
        );
        let out = run_capture(
            Path::new("/bin/sh"),
            &["-c".into(), script],
            &HashMap::from([("PATH".to_string(), "/usr/bin:/bin:/usr/local/bin".to_string())]),
            Path::new("/"),
            Duration::from_millis(1500),
        )
        .await;
        assert!(out.timed_out, "{out:?}");
        let pid: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let n = crate::common::proctree::KILL_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|p| **p == pid)
            .count();
        assert_eq!(n, 1, "only the cleanup right after the reaping may killpg pid {pid}");
    }

    #[tokio::test]
    async fn timeout_with_a_live_leader_still_kills_the_group_once() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("leader.pid");
        let script = format!("echo $$ > '{}'; sleep 5", pidfile.display());
        let out = run_capture(
            Path::new("/bin/sh"),
            &["-c".into(), script],
            &env(),
            Path::new("/"),
            Duration::from_millis(400),
        )
        .await;
        assert!(out.timed_out);
        let pid: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let n = crate::common::proctree::KILL_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|p| **p == pid)
            .count();
        assert_eq!(n, 1, "the leader was alive at the deadline: exactly one killpg");
    }

    #[tokio::test]
    async fn missing_binary() {
        let out = run_capture(Path::new("/no/such/bin"), &[], &env(), Path::new("/"), Duration::from_secs(1)).await;
        assert!(out.spawn_error.is_some());
        assert!(out.failure_reason().starts_with("failed to start"));
    }

    #[tokio::test]
    async fn env_is_not_inherited() {
        std::env::set_var("ANYCON_TEST_LEAK", "1");
        let out = run_capture(
            Path::new("/bin/sh"),
            &["-c".into(), "echo \"[$ANYCON_TEST_LEAK]\"".into()],
            &env(),
            Path::new("/"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(out.stdout.trim(), "[]");
    }
}
