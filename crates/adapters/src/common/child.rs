//! 「stdin / stdout 都是换行分隔 JSON」的长驻子进程封装。
//!
//! - stdout 逐行读出，经 mpsc 交给调用方（不解析，非 `{` 开头的行也原样给，由上层决定忽略）。
//!   **单行有上限**（[`SpawnSpec::max_line_bytes`]，默认 [`DEFAULT_MAX_LINE_BYTES`] = 8 MiB）：超过就判定为协议错误，
//!   杀掉整个进程组、关闭行通道，调用方用 [`JsonlChild::line_overflowed`] 区分「超长」和「正常退出」
//! - 另有 [`JsonlChild::spawn_events`]：超长的行**不杀进程**，而是整行丢弃，只把「长度 + 前 4 KiB」作为 [`LineEvent::Oversize`] 交给调用方，
//!   由调用方按「这是什么消息」决定要不要当协议错误（Codex 的 `turn/diff/updated`、补丁的整份 diff 可能超过 8 MiB，丢掉就好；
//!   待答复的请求或某个请求的响应超长才是真的出错）
//! - stdin 通过内部写任务串行写入，调用方随时 `send_json`；`close_stdin` 触发 EOF
//! - stderr 只保留末尾一小段用于报错，不进日志之外的任何地方（单行也有上限，超出部分丢弃）
//! - 优雅关闭：关 stdin → 短暂等待 → SIGINT → 宽限期（默认 5 秒）→ SIGKILL（整个进程组）。Windows 上没有 SIGINT 这一步
//!   （[`super::proctree`]），关 stdin 之后等满两段时间再终止整棵进程树
//!
//! 子进程放进**自己的进程组**（Windows：Job Object，见 [`super::proctree`]）：SIGKILL 时连同它拉起的工具进程一起清掉；
//! SIGINT 只发给进程本身，让 agent 自己收尾。daemon 崩溃时 stdin 管道断开，agent 读到 EOF 也会退出。
//!
//! **进程组总是会被清掉**：leader 退出（不管是自己退、被我们杀、还是崩溃）的那一刻，等待任务立刻对整个进程组
//! `killpg(SIGKILL)` 一次，所以 Bash 工具派生的后台进程（dev server 之类）不会在 claude 退出后残留；
//! `kill()` / `Drop` / `shutdown()` 在 leader 还没被回收时同样 `killpg`。leader 被回收之后不会再对这个 pid / pgid 发信号——
//! pid 可能被系统复用，再发有误杀别的进程组的风险。「查是否已回收」和「发信号」之间不留空档：见 `Reaped`（本文件内部）。
//! Windows 的 job 按句柄寻址，没有 pid 复用的问题，同一套调用顺序照样成立。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use super::proctree::Tree;

/// stdout 单行上限的默认值（不含换行）。stream-json 里一个 `tool_result` 可能带很大的文件内容，所以给得宽；
/// 但必须有上限，否则一个坏掉 / 被攻破的 CLI 能让 daemon 内存无限增长。
pub const DEFAULT_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// stderr 单行上限：只用来报错，超出的部分直接丢弃。
const STDERR_LINE_MAX: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// 子进程的完整环境（不继承 daemon 自己的）。
    pub env: HashMap<String, String>,
    /// stdout 单行上限（字节，不含换行）。超出 = 协议错误：杀进程组并关闭行通道。通常用 [`DEFAULT_MAX_LINE_BYTES`]。
    pub max_line_bytes: usize,
}

/// 超长的行交给调用方的「头」有多长。
pub const OVERSIZE_PREFIX_BYTES: usize = 4096;

/// [`JsonlChild::spawn_events`] 交给调用方的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    Line(String),
    /// 这一行超过了 `max_line_bytes`，已经整行丢弃（进程还活着）。`len` 是整行的字节数（不含换行），
    /// `prefix` 是它的前 [`OVERSIZE_PREFIX_BYTES`] 字节（按 UTF-8 宽松解码）。
    Oversize {
        len: usize,
        prefix: String,
    },
}

/// stdout 行的去向：普通（超长就杀进程）或事件（超长整行丢弃并通知）。
enum LineSink {
    Plain(mpsc::Sender<String>),
    Events(mpsc::Sender<LineEvent>),
}

impl LineSink {
    async fn line(&self, s: String) -> bool {
        match self {
            LineSink::Plain(tx) => tx.send(s).await.is_ok(),
            LineSink::Events(tx) => tx.send(LineEvent::Line(s)).await.is_ok(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// 正常退出码；被信号杀死时为 `None`。
    pub code: Option<i32>,
    /// 终止它的信号编号（unix）。
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(s)) => format!("signal {s}"),
            (None, None) => "unknown exit".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShutdownTimings {
    /// 关 stdin 之后先等多久，进程自己退了就不必发信号。
    pub after_stdin_close: Duration,
    /// SIGINT 之后的宽限期，到点 SIGKILL。
    pub grace: Duration,
}

impl Default for ShutdownTimings {
    fn default() -> Self {
        Self { after_stdin_close: Duration::from_millis(300), grace: Duration::from_secs(5) }
    }
}

const STDERR_TAIL_BYTES: usize = 4096;

#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > STDERR_TAIL_BYTES && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len() + 1;
            }
        }
    }

    fn text(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// 读一行的结果。
enum LineRead {
    Eof,
    /// `buf` 里是一整行（不含 `\n`）。
    Line,
    /// 行已经超过上限；`buf` 里是已经读到的前缀，剩下的还没消费。
    TooLong,
}

/// 按行读，最多把 `max` 字节读进 `buf`（调用方负责 `clear`）。不会为超长的行分配超过 `max + 一个读缓冲` 的内存。
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    rd: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<LineRead> {
    loop {
        let chunk = rd.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() { LineRead::Eof } else { LineRead::Line });
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => {
                if buf.len() + i > max {
                    return too_long(rd, buf, i).await;
                }
                buf.extend_from_slice(&chunk[..i]);
                rd.consume(i + 1);
                return Ok(LineRead::Line);
            }
            None => {
                let n = chunk.len();
                if buf.len() + n > max {
                    return too_long(rd, buf, n).await;
                }
                buf.extend_from_slice(chunk);
                rd.consume(n);
            }
        }
    }
}

/// 超长：把这一行的前 [`OVERSIZE_PREFIX_BYTES`] 字节留在 `buf` 里（当前块里的也算），其余不动。
async fn too_long<R: AsyncBufRead + Unpin>(
    rd: &mut R,
    buf: &mut Vec<u8>,
    chunk_len: usize,
) -> std::io::Result<LineRead> {
    if buf.len() < OVERSIZE_PREFIX_BYTES {
        let chunk = rd.fill_buf().await?;
        let take = (OVERSIZE_PREFIX_BYTES - buf.len()).min(chunk_len).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        rd.consume(take);
    }
    Ok(LineRead::TooLong)
}

/// 丢弃当前行剩下的部分（直到换行或 EOF）。返回 `(丢掉的字节数（不含换行）, 是否已经 EOF)`。
async fn skip_rest_of_line<R: AsyncBufRead + Unpin>(rd: &mut R) -> std::io::Result<(usize, bool)> {
    let mut skipped = 0;
    loop {
        let chunk = rd.fill_buf().await?;
        if chunk.is_empty() {
            return Ok((skipped, true));
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => {
                skipped += i;
                rd.consume(i + 1);
                return Ok((skipped, false));
            }
            None => {
                let n = chunk.len();
                skipped += n;
                rd.consume(n);
            }
        }
    }
}

pub struct JsonlChild {
    pid: Option<u32>,
    stdin_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    exit_rx: watch::Receiver<Option<ExitInfo>>,
    stderr_tail: Arc<Mutex<StderrTail>>,
    /// leader 是否已被回收（回收后等待任务清一次进程组）。之后不再对这个 pid / pgid 发信号，免得 pid 被复用后误杀。
    reap: Arc<Reaped>,
    tree: Arc<Tree>,
    /// stdout 出现了超过上限的行。
    overflow: Arc<AtomicBool>,
}

/// leader 的回收状态。回收（`waitpid`）之后 pid 随时可能被系统复用，所以所有对 pid / pgid 发信号的地方都得先过这里。
///
/// 回收发生在 `Child::wait()` 被 poll 的那一刻。[`Reaped::wait`] 把每次 poll 放进一把锁里，并在 poll 返回 `Ready`
/// 的同一个临界区里置标志；[`Reaped::signal_if_alive`] 同样先拿这把锁、再查标志、再发信号。
/// 于是「查标志」和「发信号」之间 leader 不可能被回收：要么回收先发生（标志已置，不发），要么信号先发完（回收在锁释放之后），
/// 不存在「标志还是假、pid 其实已回收」的窗口。锁只在一次非阻塞的 poll / 一次 `kill(2)` 期间持有，不跨 `.await`。
///
/// 唯一剩下的窗口在等待任务里：回收之后对进程组做的那一次清理 `killpg`（要清掉 leader 留下的后台进程，只能在
/// leader 退出之后做；macOS 没有 `waitid(WNOWAIT)` 可以先清后收）。进程组里还有幸存者时 pgid 不会被复用；
/// 组已经空了的话，要误杀必须在回收到这一行之间的几微秒里让 pid 空间整整转一圈。
#[derive(Default)]
pub(crate) struct Reaped {
    lock: Mutex<()>,
    reaped: AtomicBool,
}

impl Reaped {
    /// 等 `child` 退出（回收）。回收的那次 poll 在锁内完成并置标志。
    pub(crate) async fn wait(&self, child: &mut Child) -> std::io::Result<ExitStatus> {
        let wait = child.wait();
        tokio::pin!(wait);
        std::future::poll_fn(|cx| {
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            let polled = wait.as_mut().poll(cx);
            if polled.is_ready() {
                self.reaped.store(true, Ordering::SeqCst);
            }
            polled
        })
        .await
    }

    /// leader 是否已被回收（只给测试观察；要发信号请用 [`Self::signal_if_alive`]，它在锁内查）。
    #[cfg(all(test, unix))]
    pub(crate) fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::SeqCst)
    }

    /// leader 还没被回收时执行 `send`（发信号）并返回 `true`；已回收则什么也不做、返回 `false`。
    /// `send` 在锁内执行，期间 leader 不会被回收，所以它拿到的 pid 一定还是我们的。
    pub(crate) fn signal_if_alive(&self, send: impl FnOnce()) -> bool {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if self.reaped.load(Ordering::SeqCst) {
            return false;
        }
        send();
        true
    }
}

impl JsonlChild {
    /// 起进程。返回 `(句柄, stdout 行接收端)`；接收端在 stdout 关闭（进程退出）后结束。
    /// stdout 单行超过 `max_line_bytes` = 协议错误：杀进程组、关通道（见 [`Self::line_overflowed`]）。
    pub fn spawn(spec: &SpawnSpec) -> std::io::Result<(Self, mpsc::Receiver<String>)> {
        let (line_tx, line_rx) = mpsc::channel::<String>(1024);
        let child = Self::spawn_inner(spec, LineSink::Plain(line_tx))?;
        Ok((child, line_rx))
    }

    /// 和 [`Self::spawn`] 一样，但 stdout 的超长行**不杀进程**：整行丢弃，以 [`LineEvent::Oversize`] 通知调用方
    /// （进程继续跑，[`Self::line_overflowed`] 保持 false）。调用方自己决定哪些超长行是协议错误（然后 `kill()`）。
    pub fn spawn_events(spec: &SpawnSpec) -> std::io::Result<(Self, mpsc::Receiver<LineEvent>)> {
        let (line_tx, line_rx) = mpsc::channel::<LineEvent>(1024);
        let child = Self::spawn_inner(spec, LineSink::Events(line_tx))?;
        Ok((child, line_rx))
    }

    fn spawn_inner(spec: &SpawnSpec, sink: LineSink) -> std::io::Result<Self> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        super::proctree::isolate(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let tree = Arc::new(Tree::attach(&child));
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // 写任务
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(buf) = stdin_rx.recv().await {
                if stdin.write_all(&buf).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // drop(stdin) -> EOF
        });

        // stdout 读任务：单行有上限。普通模式超长 = 协议错误（杀进程组、关通道）；事件模式整行丢弃并通知
        let reap = Arc::new(Reaped::default());
        let overflow = Arc::new(AtomicBool::new(false));
        {
            let max_line = spec.max_line_bytes.max(1);
            let overflow = overflow.clone();
            let reap = reap.clone();
            let tree = tree.clone();
            tokio::spawn(async move {
                let mut rd = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match read_capped_line(&mut rd, &mut buf, max_line).await {
                        Ok(LineRead::Eof) | Err(_) => break,
                        Ok(LineRead::Line) => {
                            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                                buf.pop();
                            }
                            let line = String::from_utf8_lossy(&buf).into_owned();
                            if !sink.line(line).await {
                                break;
                            }
                        }
                        Ok(LineRead::TooLong) => match &sink {
                            LineSink::Events(tx) => {
                                let (rest, eof) = skip_rest_of_line(&mut rd).await.unwrap_or((0, true));
                                let len = buf.len() + rest;
                                tracing::warn!(pid = ?pid, len, max_line, "child wrote a stdout line over the limit, dropping it");
                                let prefix = String::from_utf8_lossy(&buf).into_owned();
                                if tx.send(LineEvent::Oversize { len, prefix }).await.is_err() || eof {
                                    break;
                                }
                            }
                            LineSink::Plain(_) => {
                                tracing::warn!(pid = ?pid, max_line, "child wrote a stdout line over the limit, killing it");
                                overflow.store(true, Ordering::SeqCst);
                                reap.signal_if_alive(|| tree.kill());
                                break;
                            }
                        },
                    }
                }
            });
        }

        // stderr 读任务：只留尾巴，单行超出上限的部分丢弃
        let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut rd = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                let (truncated, eof) = match read_capped_line(&mut rd, &mut buf, STDERR_LINE_MAX).await {
                    Ok(LineRead::Eof) | Err(_) => break,
                    Ok(LineRead::Line) => (false, false),
                    Ok(LineRead::TooLong) => {
                        (true, skip_rest_of_line(&mut rd).await.map(|(_, eof)| eof).unwrap_or(true))
                    }
                };
                let mut line = String::from_utf8_lossy(&buf).trim_end().to_string();
                if truncated {
                    line.push_str(" …[line truncated]");
                }
                if !line.is_empty() {
                    if let Ok(mut t) = tail.lock() {
                        t.push(line);
                    }
                }
                if eof {
                    break;
                }
            }
        });

        // 等待任务：leader 一退出就清掉整个进程组（工具派生的后台进程不留），然后才公布退出信息
        let (exit_tx, exit_rx) = watch::channel(None);
        {
            let reap = reap.clone();
            let tree = tree.clone();
            tokio::spawn(async move {
                let info = match reap.wait(&mut child).await {
                    Ok(status) => {
                        #[cfg(unix)]
                        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                        #[cfg(not(unix))]
                        let signal = None;
                        ExitInfo { code: status.code(), signal }
                    }
                    Err(_) => ExitInfo { code: None, signal: None },
                };
                // leader 已回收（标志已在 `reap.wait` 里置上，别人不会再发信号）：清这一次进程组，免得留后台进程
                tree.kill();
                let _ = exit_tx.send(Some(info));
            });
        }

        Ok(Self { pid, stdin_tx: Mutex::new(Some(stdin_tx)), exit_rx, stderr_tail, reap, tree, overflow })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 写一行（自动补换行）。stdin 已关或进程已退出时返回 `false`。
    pub fn send_line(&self, line: &str) -> bool {
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        self.stdin_tx.lock().ok().and_then(|g| g.as_ref().map(|tx| tx.send(buf).is_ok())).unwrap_or_default()
    }

    pub fn send_json(&self, v: &Value) -> bool {
        self.send_line(&v.to_string())
    }

    /// 关 stdin（子进程读到 EOF）。已排队的行会先写完。
    pub fn close_stdin(&self) {
        if let Ok(mut g) = self.stdin_tx.lock() {
            g.take();
        }
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        *self.exit_rx.borrow()
    }

    pub fn has_exited(&self) -> bool {
        self.exit_info().is_some()
    }

    /// 等进程退出，最多 `timeout`。
    pub async fn wait_exit(&self, timeout: Duration) -> Option<ExitInfo> {
        let mut rx = self.exit_rx.clone();
        let waited = tokio::time::timeout(timeout, rx.wait_for(|v| v.is_some())).await;
        match waited {
            Ok(Ok(v)) => *v,
            _ => self.exit_info(),
        }
    }

    /// 无限期等进程退出。
    pub async fn wait_exit_forever(&self) -> ExitInfo {
        let mut rx = self.exit_rx.clone();
        let waited = rx.wait_for(|v| v.is_some()).await;
        match waited {
            Ok(v) => (*v).unwrap_or(ExitInfo { code: None, signal: None }),
            Err(_) => ExitInfo { code: None, signal: None },
        }
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().map(|t| t.text()).unwrap_or_default()
    }

    /// SIGINT 只发给进程本身。Windows 上不做事（见 [`super::proctree`]）。
    /// 已回收就不发（`has_exited()` 要等回收之后、清理进程组、再发布退出信息，窗口更宽）。
    pub fn interrupt(&self) {
        self.reap.signal_if_alive(|| self.tree.interrupt());
    }

    /// 取消没有得到回应时的兜底：unix 发 SIGINT（[`Self::interrupt`]）；Windows 没有只发给一个进程的中断，
    /// 直接终止整棵进程树（会话下次按 resume 接上）。
    pub fn interrupt_or_kill(&self) {
        if cfg!(windows) {
            self.kill();
        } else {
            self.interrupt();
        }
    }

    /// SIGKILL 整个进程组（Windows：终止整个 job）。leader 刚退出、还没被回收时照发（工具进程还在的情况正是要清的）；
    /// leader 一旦被回收（等待任务随即清过一次进程组）就不再发，避免 pid 复用误杀。
    pub fn kill(&self) {
        self.reap.signal_if_alive(|| self.tree.kill());
    }

    /// stdout 是否出现过超过 `max_line_bytes` 的行（此时进程已被杀，行通道随后关闭）。
    pub fn line_overflowed(&self) -> bool {
        self.overflow.load(Ordering::SeqCst)
    }

    /// 关 stdin → 等 `after_stdin_close` → SIGINT → 等 `grace` → SIGKILL。返回最终退出信息。
    pub async fn shutdown(&self, t: ShutdownTimings) -> ExitInfo {
        self.close_stdin();
        if let Some(i) = self.wait_exit(t.after_stdin_close).await {
            return i;
        }
        self.interrupt();
        if let Some(i) = self.wait_exit(t.grace).await {
            return i;
        }
        tracing::warn!(pid = ?self.pid, "child ignored SIGINT, sending SIGKILL");
        self.kill();
        match self.wait_exit(Duration::from_secs(3)).await {
            Some(i) => i,
            None => ExitInfo { code: None, signal: None },
        }
    }
}

impl Drop for JsonlChild {
    fn drop(&mut self) {
        // 句柄没走 shutdown 就丢了：别留孤儿。
        self.close_stdin();
        self.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spec(script: &str) -> SpawnSpec {
        SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }

    fn pid_alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    /// 等 `pid` 消失（它可能先是僵尸，要等 init 回收）。
    async fn wait_gone(pid: i32) -> bool {
        let start = std::time::Instant::now();
        while pid_alive(pid) {
            if start.elapsed() > Duration::from_secs(5) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    #[tokio::test]
    async fn echoes_lines_and_exits_on_stdin_eof() {
        let (child, mut rx) = JsonlChild::spawn(&spec("while read l; do echo \"got:$l\"; done")).unwrap();
        assert!(child.send_json(&serde_json::json!({"a": 1})));
        assert_eq!(rx.recv().await.unwrap(), "got:{\"a\":1}");
        let info = child.shutdown(ShutdownTimings::default()).await;
        assert_eq!(info.code, Some(0));
        assert!(rx.recv().await.is_none());
        assert!(!child.send_line("x"));
    }

    #[tokio::test]
    async fn sigint_when_stdin_close_is_ignored() {
        // 忽略 EOF（不读 stdin），但 SIGINT 默认会杀掉 sleep 所在的 sh
        let (child, _rx) = JsonlChild::spawn(&spec("exec sleep 30")).unwrap();
        let started = std::time::Instant::now();
        let info = child
            .shutdown(ShutdownTimings { after_stdin_close: Duration::from_millis(50), grace: Duration::from_secs(5) })
            .await;
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(info.signal, Some(2));
    }

    #[tokio::test]
    async fn sigkill_after_grace_when_sigint_ignored() {
        let (child, mut rx) =
            JsonlChild::spawn(&spec("trap '' INT; echo ready; while true; do sleep 0.1; done")).unwrap();
        assert_eq!(rx.recv().await.unwrap(), "ready");
        let info = child
            .shutdown(ShutdownTimings {
                after_stdin_close: Duration::from_millis(50),
                grace: Duration::from_millis(300),
            })
            .await;
        assert_eq!(info.signal, Some(9));
    }

    #[tokio::test]
    async fn stderr_tail_and_exit_code() {
        let (child, mut rx) = JsonlChild::spawn(&spec("echo 'boom: something broke' >&2; exit 7")).unwrap();
        assert!(rx.recv().await.is_none());
        let info = child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert_eq!(info.code, Some(7));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(child.stderr_tail().contains("something broke"));
    }

    #[tokio::test]
    async fn non_json_lines_pass_through_and_crlf_trimmed() {
        let (child, mut rx) = JsonlChild::spawn(&spec("printf 'banner\\r\\n{\"x\":1}\\n'")).unwrap();
        assert_eq!(rx.recv().await.unwrap(), "banner");
        assert_eq!(rx.recv().await.unwrap(), "{\"x\":1}");
        assert!(rx.recv().await.is_none());
        child.wait_exit(Duration::from_secs(5)).await.unwrap();
    }

    #[tokio::test]
    async fn spawn_failure_is_io_error() {
        let mut s = spec("true");
        s.program = PathBuf::from("/no/such/program");
        assert!(JsonlChild::spawn(&s).is_err());
    }

    #[tokio::test]
    async fn overlong_stdout_line_is_a_protocol_error_that_kills_the_group() {
        let mut sp = spec("echo ok; head -c 5000 /dev/zero | tr '\\0' a; echo; echo after; sleep 30");
        sp.max_line_bytes = 1000;
        let (child, mut rx) = JsonlChild::spawn(&sp).unwrap();
        assert_eq!(rx.recv().await.unwrap(), "ok");
        // 超长的行不会交给调用方，通道直接关闭；后面的 `after` 也不会出现
        assert!(rx.recv().await.is_none());
        assert!(child.line_overflowed());
        let info = child.wait_exit(Duration::from_secs(5)).await.expect("the child was killed");
        assert_eq!(info.signal, Some(9));
    }

    #[tokio::test]
    async fn events_mode_drops_an_overlong_line_and_keeps_going() {
        let mut sp = spec("echo ok; printf '{\"method\":\"big/one\",\"params\":\"'; head -c 20000 /dev/zero | tr '\\0' a; echo '\"}'; echo after; echo '{\"x\":1}'");
        sp.max_line_bytes = 1000;
        let (child, mut rx) = JsonlChild::spawn_events(&sp).unwrap();
        assert_eq!(rx.recv().await.unwrap(), LineEvent::Line("ok".into()));
        match rx.recv().await.unwrap() {
            LineEvent::Oversize { len, prefix } => {
                // 整行 = 引号前的 28 字节 + 20000 个 a + 结尾 `"}`
                assert_eq!(len, "{\"method\":\"big/one\",\"params\":\"".len() + 20000 + 2);
                assert_eq!(prefix.len(), OVERSIZE_PREFIX_BYTES);
                assert!(prefix.starts_with("{\"method\":\"big/one\""), "{prefix}");
            }
            other => panic!("{other:?}"),
        }
        // 后面的行照常交付，进程没被杀
        assert_eq!(rx.recv().await.unwrap(), LineEvent::Line("after".into()));
        assert_eq!(rx.recv().await.unwrap(), LineEvent::Line("{\"x\":1}".into()));
        assert!(rx.recv().await.is_none());
        let info = child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert_eq!(info.code, Some(0));
        assert!(!child.line_overflowed());
    }

    #[tokio::test]
    async fn events_mode_short_overlong_lines_keep_whatever_they_have() {
        // 比前缀上限短一点的超长行：前缀就是整行的开头
        let mut sp = spec("head -c 1500 /dev/zero | tr '\\0' b; echo; echo next");
        sp.max_line_bytes = 1000;
        let (_child, mut rx) = JsonlChild::spawn_events(&sp).unwrap();
        match rx.recv().await.unwrap() {
            LineEvent::Oversize { len, prefix } => {
                assert_eq!(len, 1500);
                assert_eq!(prefix.len(), 1500.min(OVERSIZE_PREFIX_BYTES));
                assert!(prefix.chars().all(|c| c == 'b'));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(rx.recv().await.unwrap(), LineEvent::Line("next".into()));
    }

    #[tokio::test]
    async fn line_exactly_at_the_limit_passes_and_normal_exit_is_not_overflow() {
        let mut sp = spec("head -c 1000 /dev/zero | tr '\\0' b; echo; echo next");
        sp.max_line_bytes = 1000;
        let (child, mut rx) = JsonlChild::spawn(&sp).unwrap();
        assert_eq!(rx.recv().await.unwrap().len(), 1000);
        assert_eq!(rx.recv().await.unwrap(), "next");
        assert!(rx.recv().await.is_none());
        child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert!(!child.line_overflowed());
    }

    #[tokio::test]
    async fn overlong_stderr_line_is_truncated_not_buffered() {
        let (child, mut rx) =
            JsonlChild::spawn(&spec("echo first >&2; head -c 200000 /dev/zero | tr '\\0' e >&2; echo >&2")).unwrap();
        assert!(rx.recv().await.is_none());
        child.wait_exit(Duration::from_secs(5)).await.unwrap();
        let start = std::time::Instant::now();
        while !child.stderr_tail().contains("[line truncated]") {
            assert!(start.elapsed() < Duration::from_secs(5), "stderr never finished");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let tail = child.stderr_tail();
        assert!(tail.len() < STDERR_LINE_MAX + 100, "stderr tail is bounded: {}", tail.len());
    }

    #[tokio::test]
    async fn processes_left_behind_by_the_leader_are_killed_when_it_exits() {
        // leader 打印后台进程的 pid 就退出；后台进程仍在同一个进程组里
        let (child, mut rx) = JsonlChild::spawn(&spec("sleep 300 >/dev/null 2>&1 & echo $!; exit 0")).unwrap();
        let bg: i32 = rx.recv().await.unwrap().trim().parse().unwrap();
        child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert!(wait_gone(bg).await, "background process {bg} survived its leader");
    }

    #[tokio::test]
    async fn graceful_shutdown_also_clears_the_group() {
        // leader 读到 stdin EOF 就退出，但它派生的后台进程会残留
        let (child, mut rx) =
            JsonlChild::spawn(&spec("sleep 300 >/dev/null 2>&1 & echo $!; while read l; do :; done")).unwrap();
        let bg: i32 = rx.recv().await.unwrap().trim().parse().unwrap();
        assert!(pid_alive(bg));
        let info = child.shutdown(ShutdownTimings::default()).await;
        assert_eq!(info.code, Some(0));
        assert!(wait_gone(bg).await, "background process {bg} survived shutdown");
    }

    #[tokio::test]
    async fn kill_and_drop_clear_the_group_while_the_leader_runs() {
        let (child, mut rx) = JsonlChild::spawn(&spec("sleep 300 >/dev/null 2>&1 & echo $!; wait")).unwrap();
        let bg: i32 = rx.recv().await.unwrap().trim().parse().unwrap();
        child.kill();
        let info = child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert_eq!(info.signal, Some(9));
        assert!(wait_gone(bg).await, "background process {bg} survived kill()");

        let (child, mut rx) = JsonlChild::spawn(&spec("sleep 300 >/dev/null 2>&1 & echo $!; wait")).unwrap();
        let bg: i32 = rx.recv().await.unwrap().trim().parse().unwrap();
        drop(child);
        assert!(wait_gone(bg).await, "background process {bg} survived the drop");
    }

    #[tokio::test]
    async fn no_signals_after_the_group_was_cleaned() {
        // 进程组清理过之后 kill() 是空操作（pid 可能已被复用），不会 panic 也不会报错
        let (child, mut rx) = JsonlChild::spawn(&spec("true")).unwrap();
        assert!(rx.recv().await.is_none());
        child.wait_exit(Duration::from_secs(5)).await.unwrap();
        assert!(child.reap.is_reaped());
        child.kill();
        child.interrupt();
    }

    #[tokio::test]
    async fn signals_are_decided_under_the_same_lock_as_the_reaping() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "sleep 30"]).stdin(Stdio::null()).process_group(0).kill_on_drop(true);
        let mut child = c.spawn().unwrap();
        let reap = Reaped::default();
        // 还没回收：发信号的闭包会被执行
        let mut ran = false;
        assert!(reap.signal_if_alive(|| ran = true));
        assert!(ran && !reap.is_reaped());
        child.start_kill().unwrap();
        let status = reap.wait(&mut child).await.unwrap();
        assert!(!status.success());
        // 回收之后：闭包不会被执行
        assert!(reap.is_reaped());
        assert!(!reap.signal_if_alive(|| panic!("signalled a reaped pid")));
    }

    #[tokio::test]
    async fn kill_racing_with_the_exit_never_signals_after_the_exit_is_published() {
        // 一堆瞬间退出的子进程，同时从别的任务疯狂 kill() / interrupt()：不 panic，
        // 而且一旦退出信息发布了（说明已回收），`signal_if_alive` 一定拒绝
        for _ in 0..40 {
            let (child, _rx) = JsonlChild::spawn(&spec("exit 0")).unwrap();
            let child = Arc::new(child);
            let hammer = {
                let child = child.clone();
                tokio::spawn(async move {
                    for _ in 0..200 {
                        child.kill();
                        child.interrupt();
                        tokio::task::yield_now().await;
                    }
                })
            };
            child.wait_exit(Duration::from_secs(5)).await.unwrap();
            assert!(child.reap.is_reaped());
            assert!(!child.reap.signal_if_alive(|| panic!("signalled a reaped pid")));
            hammer.await.unwrap();
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn spec(script: &str) -> SpawnSpec {
        let env: HashMap<String, String> = std::env::vars().collect();
        SpawnSpec {
            program: PathBuf::from("cmd"),
            args: vec!["/c".into(), script.into()],
            env,
            cwd: std::env::temp_dir(),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }

    #[tokio::test]
    async fn reads_lines_and_exit_code() {
        // cmd 的引号规则和 MSVC 的不一样，这里不用引号
        let (c, mut rx) = JsonlChild::spawn(&spec("echo hello& exit 3")).unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("hello"));
        let info = c.wait_exit(Duration::from_secs(10)).await.unwrap();
        assert_eq!(info.code, Some(3));
    }

    #[tokio::test]
    async fn shutdown_terminates_a_child_that_ignores_stdin_eof() {
        let (c, _rx) = JsonlChild::spawn(&spec("ping -n 60 127.0.0.1 >NUL")).unwrap();
        let t = ShutdownTimings { after_stdin_close: Duration::from_millis(200), grace: Duration::from_millis(200) };
        let info = tokio::time::timeout(Duration::from_secs(10), c.shutdown(t)).await.unwrap();
        assert_ne!(info.code, Some(0));
        assert!(c.has_exited());
    }
}
