//! 一次性调用的有界管道与进程树生命周期；错误不携带子进程内容。

use std::path::Path;
use std::process::Stdio;

use anycon_agent_api::{OneshotError, OneshotRequest};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{child::Reaped, proctree::Tree};

pub(crate) const OUTPUT_LIMIT: usize = 1024 * 1024;

struct Cleanup {
    tree: Tree,
    reap: Reaped,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.reap.signal_if_alive(|| self.tree.kill());
    }
}

pub(crate) async fn read_limited(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, OneshotError> {
    let mut bytes = Vec::new();
    reader.take((OUTPUT_LIMIT + 1) as u64).read_to_end(&mut bytes).await.map_err(|_| OneshotError::Failed)?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(OneshotError::Failed);
    }
    Ok(bytes)
}

/// 调用方的总超时包住整个 future；丢弃时先清掉进程树，再由 Child 回收 leader。
pub(crate) async fn run(
    program: &Path,
    args: &[String],
    req: &OneshotRequest,
    input: &[u8],
) -> Result<Vec<u8>, OneshotError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&req.cwd)
        .env_clear()
        .envs(req.env.as_ref())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    super::proctree::isolate(&mut cmd);
    let mut child = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            // 刚写出的脚本可能短暂被并行 fork 继承写句柄；重试仍在总超时内。
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(_) => return Err(OneshotError::Spawn),
        }
    };
    let cleanup = Cleanup { tree: Tree::attach(&child), reap: Reaped::default() };
    let mut stdin = child.stdin.take().ok_or(OneshotError::Spawn)?;
    let stdout = child.stdout.take().ok_or(OneshotError::Spawn)?;
    let stderr = child.stderr.take().ok_or(OneshotError::Spawn)?;
    let write = async {
        stdin.write_all(input).await.map_err(|_| OneshotError::Failed)?;
        drop(stdin);
        Ok(())
    };
    let wait = async {
        let status = cleanup.reap.wait(&mut child).await;
        // leader 退出后只清理这一次，避免之后对被复用的 pid 发信号。
        cleanup.tree.kill();
        if status.map_err(|_| OneshotError::Failed)?.success() {
            Ok(())
        } else {
            Err(OneshotError::Failed)
        }
    };
    let (out, _, (), ()) = tokio::try_join!(read_limited(stdout), read_limited(stderr), write, wait)?;
    Ok(out)
}
