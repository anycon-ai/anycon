//! 测试公共：运行 `anycon` 二进制、起一个 daemon 子进程。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

pub const BIN: &str = env!("CARGO_BIN_EXE_anycon");

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn json_lines(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}\nstderr: {}", self.stderr))
            })
            .collect()
    }
    pub fn json(&self) -> serde_json::Value {
        let v = self.json_lines();
        assert_eq!(v.len(), 1, "expected a single JSON line, got:\n{}\nstderr: {}", self.stdout, self.stderr);
        v.into_iter().next().unwrap()
    }
}

pub fn base_command(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    for (k, _) in std::env::vars() {
        if k.starts_with("ANYCON_") {
            c.env_remove(k);
        }
    }
    c.env("ANYCON_HOME", home).env("NO_COLOR", "1").env("ANYCON_LOG", "warn").kill_on_drop(true);
    c
}

/// 运行 `anycon <args>`，可选 stdin 内容；60 秒超时。
pub async fn anycon(home: &Path, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Run {
    anycon_in(home, None, args, stdin, env).await
}

/// [`anycon`]，可以指定进程的当前目录（测相对路径参数）。
pub async fn anycon_in(
    home: &Path,
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> Run {
    let mut c = base_command(home);
    if let Some(cwd) = cwd {
        c.current_dir(cwd);
    }
    c.args(args).envs(env.iter().copied()).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = c.spawn().expect("spawn anycon");
    if let Some(input) = stdin {
        let mut si = child.stdin.take().unwrap();
        si.write_all(input.as_bytes()).await.unwrap();
        // 不关闭 stdin：交互式提示读到答案后进程自己结束；关闭会让「读不到答案」的场景提前 EOF
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(55)).await;
            drop(si);
        });
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("anycon {args:?} timed out"))
        .unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub struct DaemonProc {
    pub child: Child,
    pub home: PathBuf,
}

impl DaemonProc {
    pub async fn stop(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: 给自己起的子进程发 SIGTERM。
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        let _ = tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await;
    }
}

/// `anycon daemon`（前台）作为子进程，等到 socket 可连。
pub async fn start_daemon(home: &Path, env: &[(&str, &str)]) -> DaemonProc {
    let mut c = base_command(home);
    c.arg("daemon")
        .env("ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL", "false")
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let child = c.spawn().expect("spawn daemon");
    let sock = home.join("daemon.sock");
    let t = std::time::Instant::now();
    while tokio::net::UnixStream::connect(&sock).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "daemon did not come up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    DaemonProc { child, home: home.to_path_buf() }
}

/// `anycon init`（非交互）。
pub async fn init(home: &Path, workspace: &Path, name: &str, hub: Option<&str>) {
    let ws = workspace.to_string_lossy().into_owned();
    let mut args = vec!["init", "--workspace", &ws, "--name", name, "--yes"];
    if let Some(h) = hub {
        args.push("--hub");
        args.push(h);
    }
    let r = anycon(home, &args, None, &[]).await;
    assert_eq!(r.code, 0, "init failed: {}", r.stderr);
}
