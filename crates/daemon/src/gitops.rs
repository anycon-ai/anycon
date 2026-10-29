//! daemon 自己跑的 `git` / `gh`（ADR-34，docs/spec/daemon.md § 10.1）。
//!
//! 所有调用：
//! - 带 [`GIT_SAFE_CONFIG`]（不执行仓库配置的 `core.fsmonitor`，也不跑它的 hook）；
//! - 环境 = daemon 环境（`env_clear` + 拷贝），再去掉会把 git 指向别处的变量（`GIT_DIR`、`GIT_WORK_TREE`、`GIT_INDEX_FILE`…）
//!   和 `GIT_EXTERNAL_DIFF`，加上 `GIT_TERMINAL_PROMPT=0`、`GIT_EDITOR=:`、`GIT_PAGER=cat`、`GIT_OPTIONAL_LOCKS=0`、`LC_ALL=C`
//!   （错误信息是英文，客户端和测试按固定文字归类）；
//! - 有超时，超时杀整个进程组（`git fetch` 会拉起 `ssh` 之类的子进程）；输出有上限，超过就停止读取并杀掉。
//!
//! diff 一律由调用方加 `--no-ext-diff --no-textconv --no-color`。本模块还放着几个纯函数的解析器（`status --porcelain=v2`、
//! `diff --numstat -z` / `--name-status -z`、`gh pr view --json`），它们有各自的单元测试。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::common::proctree;
use anycon_protocol::{DiffFileStatus, PullRequestInfo, PullRequestState};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::workspace::GIT_SAFE_CONFIG;

/// 默认的 stdout 上限。
pub const DEFAULT_STDOUT_CAP: usize = 8 * 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;

/// 会把 git 指向别的仓库 / 别的程序的环境变量：daemon 可能是在某个 git hook 或 `git rebase -x` 里被拉起来的。
pub(crate) const GIT_ENV_STRIP: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CEILING_DIRECTORIES",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
];

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot run {program}: {source}")]
    Spawn { program: &'static str, source: std::io::Error },
    #[error("{program} timed out after {secs}s")]
    Timeout { program: &'static str, secs: u64 },
}

impl GitError {
    /// 程序不存在（`git` / `gh` 没装）。
    pub fn is_not_found(&self) -> bool {
        matches!(self, GitError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
    pub fn is_timeout(&self) -> bool {
        matches!(self, GitError::Timeout { .. })
    }
}

/// 一次子进程调用的结果。
#[derive(Debug)]
pub struct GitOut {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// stdout 超过上限被截断了（进程已经被杀掉，退出码没有意义）。
    pub truncated: bool,
}

impl GitOut {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.truncated
    }
    /// stdout 的文本，去掉首尾空白。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
    /// stderr 的尾部（最多 `max` 字节，按字符边界），去掉首尾空白。给错误消息用。
    pub fn err_tail(&self, max: usize) -> String {
        tail_text(&self.stderr, max)
    }
    /// stderr 的尾部；stderr 为空时退回 stdout 的尾部（`git merge` 把冲突说明写在 stdout）。
    pub fn failure_text(&self, max: usize) -> String {
        let e = self.err_tail(max);
        if e.is_empty() {
            tail_text(&self.stdout, max)
        } else {
            e
        }
    }
}

/// 字节串的尾部 `max` 字节（落在字符边界上）的文本，去掉首尾空白。
pub fn tail_text(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", s[start..].trim_start())
}

/// 读到 `cap` 字节为止。返回 (内容, 是否还有更多)。缓冲放在堆上（`read_buf` 读进 `Vec` 的空闲容量）：
/// 栈上的数组会被放进每个 await 它的 future 里，调试构建里嵌套几层就是几十 KB 的栈。
async fn read_capped<R: AsyncRead + Unpin>(r: Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let Some(mut r) = r else { return (Vec::new(), false) };
    let mut out: Vec<u8> = Vec::new();
    loop {
        out.reserve(16 * 1024);
        match r.read_buf(&mut out).await {
            Ok(0) | Err(_) => return (out, false),
            Ok(_) if out.len() > cap => {
                out.truncate(cap);
                return (out, true);
            }
            Ok(_) => {}
        }
    }
}

/// 一次子进程调用的描述。
struct ProcSpec<'a> {
    program: &'static str,
    args: &'a [String],
    cwd: &'a Path,
    extra_env: &'a [(String, String)],
    timeout: Duration,
    stdout_cap: usize,
    /// 带 [`GIT_SAFE_CONFIG`]（只有 git 要）。
    safe_config: bool,
}

/// 交给 `Command::new` 的程序：Windows 上先按 daemon 环境的 `PATH` 找到它的完整路径——标准库在子进程环境的 `PATH` 里
/// 找不到时会退回 daemon 进程自己的 `PATH`，daemon 环境里明明没有 `git` 也照样拉得起来。找不到时得到一个必然失败的名字
/// （`NotFound`）。其它平台原样返回，由 `Command` 按子进程环境的 `PATH` 找。
pub fn command_program(program: &str, env: &HashMap<String, String>) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        let cwd = anycon_adapters::common::neutral_cwd();
        if let Some(p) = anycon_adapters::common::bin::which_all(program, path, &cwd).into_iter().next() {
            return p;
        }
        return std::path::PathBuf::from(format!("{program}-not-found-on-the-daemon-path"));
    }
    std::path::PathBuf::from(program)
}

/// 拉起 `spec.program`，在独立进程组里跑（Windows：Job Object，见 `anycon_adapters::common::proctree`），带超时和输出上限。
async fn run_process(spec: ProcSpec<'_>, env: &HashMap<String, String>) -> Result<GitOut, GitError> {
    let ProcSpec { program, args, cwd, extra_env, timeout, stdout_cap, safe_config } = spec;
    let mut cmd = Command::new(command_program(program, env));
    if safe_config {
        cmd.args(GIT_SAFE_CONFIG);
    }
    cmd.args(args)
        .env_clear()
        .envs(env.iter().filter(|(k, _)| !GIT_ENV_STRIP.contains(&k.as_str())))
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", ":")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .envs(extra_env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    proctree::isolate(&mut cmd);
    let mut child = cmd.spawn().map_err(|source| GitError::Spawn { program, source })?;
    let tree = proctree::Tree::attach(&child);
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let work = async {
        let ((out, truncated), (err, _)) =
            tokio::join!(read_capped(stdout, stdout_cap), read_capped(stderr, STDERR_CAP));
        if truncated {
            tree.kill();
        }
        let status = child.wait().await;
        (out, err, truncated, status)
    };
    match tokio::time::timeout(timeout, work).await {
        Ok((stdout, stderr, truncated, status)) => {
            let status = status.map_err(|source| GitError::Spawn { program, source })?;
            Ok(GitOut { code: status.code(), stdout, stderr, truncated })
        }
        Err(_) => {
            tree.kill();
            Err(GitError::Timeout { program, secs: timeout.as_secs() })
        }
    }
}

/// daemon 的 git 运行器：持有 daemon 环境。便宜，可以随便 `clone`。
#[derive(Clone)]
pub struct Git {
    env: Arc<HashMap<String, String>>,
}

impl Git {
    pub fn new(env: Arc<HashMap<String, String>>) -> Git {
        Git { env }
    }

    pub fn env(&self) -> &Arc<HashMap<String, String>> {
        &self.env
    }

    /// 在 `cwd` 里跑一条 git。
    pub fn at<'a>(&'a self, cwd: &'a Path) -> GitCmd<'a> {
        GitCmd {
            git: self,
            cwd,
            args: Vec::new(),
            timeout: Duration::from_secs(15),
            cap: DEFAULT_STDOUT_CAP,
            extra_env: Vec::new(),
        }
    }

    /// 在 `cwd` 里跑一条 `gh`（用 daemon 环境）。
    pub async fn gh(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<GitOut, GitError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let extra = [
            ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
            ("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ];
        let spec = ProcSpec {
            program: "gh",
            args: &args,
            cwd,
            extra_env: &extra,
            timeout,
            stdout_cap: 1024 * 1024,
            safe_config: false,
        };
        run_process(spec, &self.env).await
    }
}

/// 一条待执行的 git 命令（链式设置参数）。
pub struct GitCmd<'a> {
    git: &'a Git,
    cwd: &'a Path,
    args: Vec<String>,
    timeout: Duration,
    cap: usize,
    extra_env: Vec<(String, String)>,
}

impl GitCmd<'_> {
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_string()));
        self
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
    pub fn secs(self, s: u64) -> Self {
        self.timeout(Duration::from_secs(s))
    }
    /// stdout 上限（字节）。
    pub fn cap(mut self, bytes: usize) -> Self {
        self.cap = bytes;
        self
    }
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.extra_env.push((k.to_string(), v.to_string()));
        self
    }
    pub async fn run(self) -> Result<GitOut, GitError> {
        let spec = ProcSpec {
            program: "git",
            args: &self.args,
            cwd: self.cwd,
            extra_env: &self.extra_env,
            timeout: self.timeout,
            stdout_cap: self.cap,
            safe_config: true,
        };
        run_process(spec, &self.git.env).await
    }
}

// ---------------------------------------------------------------- 解析器

/// `git status --porcelain=v2 --branch -z` 的摘要。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusV2 {
    /// HEAD 的提交；还没有提交时为空。
    pub oid: Option<String>,
    /// 当前分支；detached 时为空。
    pub head: Option<String>,
    pub upstream: Option<String>,
    /// 相对上游：本地多出来的 / 少了的提交数（有上游时）。
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    /// 已暂存 + 未暂存 + 未跟踪的文件数（冲突的算一个）。
    pub uncommitted: u32,
    pub conflicts: u32,
}

/// 解析 `git status --porcelain=v2 --branch -z` 的输出。
pub fn parse_status_v2(raw: &[u8]) -> StatusV2 {
    let mut st = StatusV2::default();
    let mut tokens = raw.split(|b| *b == 0);
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(tok);
        if let Some(h) = line.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                st.oid = (v != "(initial)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                st.head = (v != "(detached)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                // "+3 -1"
                let mut it = v.split_whitespace();
                st.ahead = it.next().and_then(|a| a.strip_prefix('+')).and_then(|a| a.parse().ok());
                st.behind = it.next().and_then(|b| b.strip_prefix('-')).and_then(|b| b.parse().ok());
            }
            continue;
        }
        match line.as_bytes().first() {
            Some(b'1') | Some(b'?') => st.uncommitted += 1,
            Some(b'2') => {
                st.uncommitted += 1;
                // 改名 / 复制的记录后面跟着一个单独的原路径字段
                tokens.next();
            }
            Some(b'u') => {
                st.uncommitted += 1;
                st.conflicts += 1;
            }
            _ => {}
        }
    }
    st
}

/// `git diff --numstat -z` 的一项。`additions` / `deletions` 为 `None` = 二进制。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub old_path: Option<String>,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

/// 解析 `git diff --numstat -z`：`<增>\t<删>\t<路径>\0`；改名是 `<增>\t<删>\t\0<旧路径>\0<新路径>\0`。
pub fn parse_numstat_z(raw: &[u8]) -> Vec<NumstatEntry> {
    let mut out = Vec::new();
    let mut tokens = raw.split(|b| *b == 0);
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(tok);
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(rest)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let num = |s: &str| if s == "-" { None } else { s.parse::<u32>().ok() };
        let (additions, deletions) = (num(a), num(d));
        if rest.is_empty() {
            let old = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
            let new = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
            if let (Some(old), Some(new)) = (old, new) {
                out.push(NumstatEntry { path: new, old_path: Some(old), additions, deletions });
            }
        } else {
            out.push(NumstatEntry { path: rest.to_string(), old_path: None, additions, deletions });
        }
    }
    out
}

/// 解析 `git diff --name-status -z`：`<状态>\0<路径>\0`；改名 / 复制是 `R100\0<旧>\0<新>\0`。
/// 返回 路径 → (状态, 旧路径)。
pub fn parse_name_status_z(raw: &[u8]) -> HashMap<String, (DiffFileStatus, Option<String>)> {
    let mut out = HashMap::new();
    let mut tokens = raw.split(|b| *b == 0);
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let code = String::from_utf8_lossy(tok);
        let kind = code.chars().next().unwrap_or('M');
        match kind {
            'R' | 'C' => {
                let old = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
                let new = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
                if let (Some(old), Some(new)) = (old, new) {
                    let status = if kind == 'R' { DiffFileStatus::Renamed } else { DiffFileStatus::Added };
                    out.insert(new, (status, (kind == 'R').then_some(old)));
                }
            }
            _ => {
                let Some(path) = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned()) else { break };
                let status = match kind {
                    'A' => DiffFileStatus::Added,
                    'D' => DiffFileStatus::Deleted,
                    _ => DiffFileStatus::Modified,
                };
                out.insert(path, (status, None));
            }
        }
    }
    out
}

/// `gh pr view --json number,url,title,state,isDraft,baseRefName,statusCheckRollup` 的输出 → [`PullRequestInfo`]。
pub fn parse_pr_json(raw: &str) -> Result<PullRequestInfo, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("bad gh output: {e}"))?;
    let str_of = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let number = v.get("number").and_then(|x| x.as_u64()).ok_or("gh output has no PR number")?;
    let draft = v.get("isDraft").and_then(|x| x.as_bool()).unwrap_or(false);
    let state = match str_of("state").to_ascii_uppercase().as_str() {
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        _ if draft => PullRequestState::Draft,
        _ => PullRequestState::Open,
    };
    let (mut passed, mut failed, mut pending) = (0u32, 0u32, 0u32);
    for c in v.get("statusCheckRollup").and_then(|x| x.as_array()).map(Vec::as_slice).unwrap_or_default() {
        let up = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_ascii_uppercase();
        // CheckRun：status + conclusion；StatusContext：state
        let verdict = if c.get("conclusion").is_some() || c.get("status").is_some() {
            if up("status") != "COMPLETED" {
                'p'
            } else {
                match up("conclusion").as_str() {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => 's',
                    "" => 'p',
                    _ => 'f',
                }
            }
        } else {
            match up("state").as_str() {
                "SUCCESS" => 's',
                "FAILURE" | "ERROR" => 'f',
                _ => 'p',
            }
        };
        match verdict {
            's' => passed += 1,
            'f' => failed += 1,
            _ => pending += 1,
        }
    }
    Ok(PullRequestInfo {
        number,
        url: str_of("url"),
        title: str_of("title"),
        state,
        base_branch: str_of("baseRefName"),
        checks_passed: passed,
        checks_failed: failed,
        checks_pending: pending,
    })
}

/// `gh pr view` 失败时的 `pr_error` 归类（`no_remote` 由调用方在跑 gh 之前判断，这里兜底识别 gh 自己的说法）。
pub fn classify_gh_failure(stderr: &str) -> &'static str {
    let s = stderr.to_ascii_lowercase();
    if s.contains("no pull requests found") || s.contains("could not find any pull requests") {
        "no_pr"
    } else if s.contains("gh auth login")
        || s.contains("not logged in")
        || s.contains("authentication")
        || s.contains("gh_token")
        || s.contains("http 401")
    {
        "gh_not_authenticated"
    } else if s.contains("no git remotes") || s.contains("none of the git remotes") {
        "no_remote"
    } else {
        "error"
    }
}

/// 给没有被 git 跟踪的新文件拼一个 `new file` 的 unified diff（文本）。`content` 是文件内容（调用方限制了大小），
/// `exec` = 有可执行位。返回 (patch, 行数)。patch 超过 `max_bytes` 时在行边界截断，第三个值是是否截断了。
pub fn synth_new_file_patch(path: &str, content: &[u8], exec: bool, max_bytes: usize) -> (String, u32, bool) {
    let mode = if exec { "100755" } else { "100644" };
    let mut out = format!("diff --git a/{path} b/{path}\nnew file mode {mode}\n");
    if content.is_empty() {
        return (out, 0, false);
    }
    let text = String::from_utf8_lossy(content);
    let ends_nl = text.ends_with('\n');
    let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(&text).split('\n').collect();
    let n = lines.len() as u32;
    out.push_str(&format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n"));
    let mut truncated = false;
    for (i, l) in lines.iter().enumerate() {
        if out.len() + l.len() + 2 > max_bytes {
            truncated = true;
            break;
        }
        out.push('+');
        out.push_str(l);
        out.push('\n');
        if i + 1 == lines.len() && !ends_nl {
            out.push_str("\\ No newline at end of file\n");
        }
    }
    (out, n, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_v2_counts_files_and_reads_the_branch_header() {
        let raw =
            b"# branch.oid 0123abc\0# branch.head anycon/x\0# branch.upstream origin/anycon/x\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/a.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? untracked.txt\0\
! ignored.txt\0";
        let st = parse_status_v2(raw);
        assert_eq!(st.oid.as_deref(), Some("0123abc"));
        assert_eq!(st.head.as_deref(), Some("anycon/x"));
        assert_eq!(st.upstream.as_deref(), Some("origin/anycon/x"));
        assert_eq!((st.ahead, st.behind), (Some(2), Some(1)));
        // 1 + 2 + u + ? = 4；改名的 old.rs 不重复计，忽略项不计
        assert_eq!(st.uncommitted, 4);
        assert_eq!(st.conflicts, 1);
        let detached = parse_status_v2(b"# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(detached.head, None);
        assert_eq!(detached.upstream, None);
        assert_eq!(detached.uncommitted, 0);
        let initial = parse_status_v2(b"# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(initial.oid, None);
    }

    #[test]
    fn numstat_and_name_status_handle_renames_and_binaries() {
        let ns = parse_numstat_z(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0\x30\t0\t\0old name.txt\0new name.txt\0");
        assert_eq!(ns.len(), 3);
        assert_eq!((ns[0].path.as_str(), ns[0].additions, ns[0].deletions), ("src/a.rs", Some(3), Some(1)));
        assert_eq!((ns[1].additions, ns[1].deletions), (None, None));
        assert_eq!(ns[2].path, "new name.txt");
        assert_eq!(ns[2].old_path.as_deref(), Some("old name.txt"));
        let st =
            parse_name_status_z(b"M\0src/a.rs\0A\0img.png\0D\0gone.txt\0R087\0old name.txt\0new name.txt\0T\0link\0");
        assert_eq!(st["src/a.rs"].0, DiffFileStatus::Modified);
        assert_eq!(st["img.png"].0, DiffFileStatus::Added);
        assert_eq!(st["gone.txt"].0, DiffFileStatus::Deleted);
        assert_eq!(st["new name.txt"], (DiffFileStatus::Renamed, Some("old name.txt".to_string())));
        assert_eq!(st["link"].0, DiffFileStatus::Modified);
    }

    #[test]
    fn pr_json_maps_states_and_counts_checks() {
        let raw = r#"{"number":7,"url":"https://example.test/pr/7","title":"Fix it","state":"OPEN","isDraft":false,
            "baseRefName":"main","statusCheckRollup":[
              {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
              {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"},
              {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
              {"__typename":"StatusContext","state":"SUCCESS"},
              {"__typename":"StatusContext","state":"PENDING"}]}"#;
        let pr = parse_pr_json(raw).unwrap();
        assert_eq!((pr.number, pr.state), (7, PullRequestState::Open));
        assert_eq!(pr.base_branch, "main");
        assert_eq!((pr.checks_passed, pr.checks_failed, pr.checks_pending), (2, 1, 2));
        let draft = parse_pr_json(r#"{"number":1,"state":"OPEN","isDraft":true}"#).unwrap();
        assert_eq!(draft.state, PullRequestState::Draft);
        let merged = parse_pr_json(r#"{"number":1,"state":"MERGED","isDraft":true}"#).unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);
        assert_eq!(parse_pr_json(r#"{"state":"OPEN"}"#).unwrap_err(), "gh output has no PR number");
        assert!(parse_pr_json("not json").is_err());
    }

    #[test]
    fn gh_failures_are_classified() {
        assert_eq!(classify_gh_failure("no pull requests found for branch \"x\""), "no_pr");
        assert_eq!(
            classify_gh_failure("To get started with GitHub CLI, please run:  gh auth login"),
            "gh_not_authenticated"
        );
        assert_eq!(
            classify_gh_failure("none of the git remotes configured for this repository point to a known GitHub host"),
            "no_remote"
        );
        assert_eq!(classify_gh_failure("something else"), "error");
    }

    #[test]
    fn a_new_file_patch_looks_like_git_output() {
        let (p, n, cut) = synth_new_file_patch("a.txt", b"one\ntwo\n", false, 1024);
        assert_eq!(n, 2);
        assert!(!cut);
        assert_eq!(
            p,
            "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n"
        );
        let (p, _, _) = synth_new_file_patch("b", b"x", true, 1024);
        assert!(p.contains("new file mode 100755") && p.ends_with("+x\n\\ No newline at end of file\n"));
        let (p, n, _) = synth_new_file_patch("empty", b"", false, 1024);
        assert_eq!((n, p.contains("@@")), (0, false));
        let big = "line\n".repeat(1000);
        let (p, n, cut) = synth_new_file_patch("big", big.as_bytes(), false, 200);
        assert!(cut && p.len() <= 200 && n == 1000);
    }

    #[test]
    fn tails_are_cut_on_character_boundaries() {
        assert_eq!(tail_text(b"  short  ", 100), "short");
        let t = tail_text("0123456789é".as_bytes(), 3);
        assert!(t.starts_with('…') && t.ends_with('é'), "{t}");
    }

    /// 调试构建里每个 await 点的 future 都在栈上拷来拷去：运行 git 的 future 一旦带上几十 KB 的栈数组，嵌套几层就会把
    /// 2 MiB 的线程栈用光（建 worktree 的调用链上有五六层）。缓冲必须在堆上。
    #[test]
    fn running_git_does_not_put_big_buffers_in_the_future() {
        let git = Git::new(Arc::new(HashMap::new()));
        let fut = git.at(Path::new("/")).args(["--version"]).run();
        assert!(std::mem::size_of_val(&fut) < 8 * 1024, "the future is {} bytes", std::mem::size_of_val(&fut));
    }

    #[tokio::test]
    async fn commands_time_out_and_oversized_output_is_cut_without_hanging() {
        let git = Git::new(Arc::new(std::env::vars().collect()));
        let d = tempfile::tempdir().unwrap();
        let out = git.at(d.path()).args(["--version"]).secs(10).run().await.unwrap();
        assert!(out.success() && out.text().starts_with("git version"));
        // 输出超过上限：停止读取，杀进程
        let out = git.at(d.path()).args(["help", "-a"]).cap(16).secs(10).run().await.unwrap();
        assert!(out.truncated && out.stdout.len() == 16 && !out.success());
        // 找不到的程序
        let spec = ProcSpec {
            program: "anycon-no-such-program",
            args: &[],
            cwd: d.path(),
            extra_env: &[],
            timeout: Duration::from_secs(2),
            stdout_cap: 10,
            safe_config: false,
        };
        let missing = run_process(spec, &HashMap::new()).await;
        assert!(missing.unwrap_err().is_not_found());
    }
}
