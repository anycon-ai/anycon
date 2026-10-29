//! 预览的端口探测与端口策略（docs/spec/daemon.md § 19.3 / § 19.4）。
//!
//! 分三层，前两层是纯函数，各有固定样例输出的单元测试：
//!
//! 1. **解析**：`lsof -nP -iTCP -sTCP:LISTEN -Fpcn`（监听 socket）、`ps -axo pid=,ppid=,pgid=,comm=`（进程关系）、
//!    `lsof -a -p <pid,…> -d cwd -Fpn`（监听进程的工作目录）。只认绑在回环地址（`127.0.0.1` / `::1`）或通配地址上的监听
//!    socket——只绑在某个局域网地址上的服务从 `127.0.0.1` 连不上；
//! 2. **归属 / 策略**：一个监听进程属于哪个会话（agent 进程树 → 进程组 → 工作目录，三条规则），一个端口是否允许预览；
//! 3. **执行与缓存**：[`PortDetector`] 跑上面三条命令（带超时、`env_clear` 后补 daemon 的环境、`kill_on_drop`），
//!    结果缓存 2 秒（多个客户端每 5 秒轮询一次也不会反复起进程）；任何一步失败或命令不存在就降级成
//!    `detection = unavailable`（只认 `preview.allow_ports`），并记一条 warn（同一原因 10 分钟内只记一次）。
//!
//! **Windows** 没有 `lsof` / `ps`，[`SystemRunner`] 直接调系统接口拿同样的三样东西：监听 socket 和所属 pid
//! （`GetExtendedTcpTable`）、进程关系（Toolhelp 快照）；「进程组」换成 agent 的 Job Object——job 里的进程的 pgid 记成 agent 主进程的
//! pid（`anycon_adapters::common::proctree::job_members`），归属规则不变。别的进程的工作目录拿不到，规则 2（工作目录）在 Windows 上
//! 不生效：用户自己在终端里起的 dev server 要写进 `preview.allow_ports` 才能预览。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anycon_protocol::{PreviewDetection, PreviewPort, PreviewPortSource};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

/// 探测结果的缓存时间。
pub const CACHE_TTL: Duration = Duration::from_secs(2);
/// 每条探测命令的超时。
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
/// 同一原因的 warn 日志的最小间隔。
pub const WARN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// 命令输出的字节上限（机器上有成千上万个 socket 也不会把内存吃光）。
const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// 一次 `lsof -p` 最多查多少个监听进程的工作目录。
const MAX_CWD_PIDS: usize = 256;

// ---------------------------------------------------------------- 数据

/// 一个绑在回环地址或通配地址上的监听 socket。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    /// lsof 给的命令名（可能被截断）；展示时优先用 `ps` 的。
    pub command: String,
}

/// `ps` 一行：进程关系和可执行文件名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    pub pgid: u32,
    /// `ps` 的 `comm`（macOS 是完整路径，Linux 是名字）。
    pub comm: String,
}

/// 一次探测的结果。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `false`：这台机器上没有可用的探测手段（`detection = unavailable`）。
    pub available: bool,
    pub listeners: Vec<Listener>,
    pub procs: HashMap<u32, ProcInfo>,
    pub cwds: HashMap<u32, PathBuf>,
}

impl Snapshot {
    pub fn unavailable() -> Snapshot {
        Snapshot::default()
    }

    pub fn detection(&self) -> PreviewDetection {
        if self.available {
            PreviewDetection::Ok
        } else {
            PreviewDetection::Unavailable
        }
    }

    /// 监听 `port` 的 socket。
    pub fn listeners_on(&self, port: u16) -> impl Iterator<Item = &Listener> {
        self.listeners.iter().filter(move |l| l.port == port)
    }

    /// 展示用的进程名：`ps` 的可执行文件名，没有就退回 lsof 的。
    pub fn process_name(&self, l: &Listener) -> String {
        let from_ps = self.procs.get(&l.pid).map(|p| basename(&p.comm)).filter(|n| !n.is_empty());
        from_ps.unwrap_or_else(|| l.command.clone())
    }
}

/// 一个会话在预览模块眼里的样子：当前 agent 主进程（没有进程为 `None`）和工作目录（项目目录或它的 worktree）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    pub session_id: String,
    pub pid: Option<u32>,
    pub workdir: Option<PathBuf>,
}

/// 一个监听进程为什么属于某个会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub source: PreviewPortSource,
    pub session_id: String,
}

fn basename(comm: &str) -> String {
    comm.rsplit('/').next().unwrap_or(comm).trim().to_string()
}

// ---------------------------------------------------------------- 解析

/// 解析监听 socket 的地址部分（`lsof -Fn` 的 `n` 字段），只认回环和通配地址，返回端口。
///
/// 认的写法：`*:3000`、`0.0.0.0:3000`、`127.0.0.1:3000`、`[::]:3000`、`[::1]:3000`。
/// 绑在某个局域网地址（`192.168.1.5:3000`、`[fe80::1%en0]:3000`）、`127.0.0.2` 之类其他回环地址的不认。
pub fn parse_listen_name(name: &str) -> Option<u16> {
    let (host, port) = name.rsplit_once(':')?;
    let ok = matches!(host, "*" | "0.0.0.0" | "127.0.0.1" | "[::]" | "[::1]");
    if !ok {
        return None;
    }
    port.parse::<u16>().ok().filter(|p| *p != 0)
}

/// 解析 `lsof -nP -iTCP -sTCP:LISTEN -Fpcn` 的输出。`p` 开头的行换进程，`c` 是命令名，`n` 是地址；
/// macOS 还会带 `f<fd>` 行（忽略）。同一个进程在同一个端口上 IPv4 / IPv6 各监听一次，只记一条。
pub fn parse_lsof_listeners(out: &str) -> Vec<Listener> {
    let mut res: Vec<Listener> = Vec::new();
    let mut seen: HashSet<(u32, u16)> = HashSet::new();
    let mut pid: Option<u32> = None;
    let mut command = String::new();
    for line in out.lines() {
        let Some(tag) = line.chars().next() else { continue };
        let rest = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = rest.trim().parse().ok();
                command.clear();
            }
            'c' => command = rest.to_string(),
            'n' => {
                if let (Some(pid), Some(port)) = (pid, parse_listen_name(rest.trim())) {
                    if seen.insert((pid, port)) {
                        res.push(Listener { port, pid, command: command.clone() });
                    }
                }
            }
            _ => {}
        }
    }
    // 命令名在 `p` 之后的 `c` 行里出现，晚于 pid 但早于任何 `n` 行（lsof 的字段顺序），所以上面直接拷贝即可
    res
}

/// 解析 `ps -axo pid=,ppid=,pgid=,comm=` 的输出。`comm` 可能带空格（macOS 的完整路径），取三个数字之后的整段。
pub fn parse_ps(out: &str) -> HashMap<u32, ProcInfo> {
    let mut m = HashMap::new();
    for line in out.lines() {
        let mut rest = line.trim_start();
        let mut nums = [0u32; 3];
        let mut ok = true;
        for n in nums.iter_mut() {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            match rest[..end].parse::<u32>() {
                Ok(v) => *n = v,
                Err(_) => {
                    ok = false;
                    break;
                }
            }
            rest = rest[end..].trim_start();
        }
        if ok {
            m.insert(nums[0], ProcInfo { ppid: nums[1], pgid: nums[2], comm: rest.trim().to_string() });
        }
    }
    m
}

/// 解析 `lsof -a -p <pid,…> -d cwd -Fpn` 的输出：每个进程一个绝对路径。读不到（权限不够）的进程没有这一行，
/// 或者名字不是绝对路径（Linux 上是 `… (readlink: Permission denied)`），一律跳过。
pub fn parse_lsof_cwd(out: &str) -> HashMap<u32, PathBuf> {
    let mut m = HashMap::new();
    let mut pid: Option<u32> = None;
    for line in out.lines() {
        let Some(tag) = line.chars().next() else { continue };
        let rest = &line[tag.len_utf8()..];
        match tag {
            'p' => pid = rest.trim().parse().ok(),
            'n' => {
                if let Some(pid) = pid {
                    if rest.starts_with('/') && !rest.contains(" (readlink:") && !rest.contains(" (stat:") {
                        m.insert(pid, PathBuf::from(rest));
                    }
                }
            }
            _ => {}
        }
    }
    m
}

// ---------------------------------------------------------------- 归属（§ 19.3）

/// `pid` 沿父进程链（含自己）能不能走到 `target`。链上有环、断了或走到 init 就停。
pub fn chain_reaches(pid: u32, target: u32, procs: &HashMap<u32, ProcInfo>) -> bool {
    let mut cur = pid;
    for _ in 0..256 {
        if cur == target {
            return true;
        }
        match procs.get(&cur) {
            Some(p) if p.ppid != cur && p.ppid > 1 => cur = p.ppid,
            _ => return false,
        }
    }
    false
}

/// 监听进程 `pid` 是不是会话 agent 主进程 `agent_pid` 的进程树 / 进程组里的（规则 1）。
/// adapter 用 `process_group(0)` 拉起 agent，所以它派生的后台进程默认和它同一个进程组。
pub fn in_agent_tree(pid: u32, agent_pid: u32, procs: &HashMap<u32, ProcInfo>) -> bool {
    if agent_pid <= 1 {
        return false;
    }
    chain_reaches(pid, agent_pid, procs) || procs.get(&pid).is_some_and(|p| p.pgid == agent_pid)
}

/// 监听进程的工作目录在会话的工作目录里（规则 2）。按路径分量比较，`/a/bc` 不在 `/a/b` 里。
pub fn cwd_within(cwd: &Path, workdir: &Path) -> bool {
    cwd.starts_with(workdir)
}

/// 这个监听进程属于哪个会话。规则 1（进程树 / 进程组）优先于规则 2（工作目录）；
/// 同一规则里多个会话都命中时，规则 1 取 session id 最小的，规则 2 取工作目录最深的（最具体的项目）。
pub fn attribute(l: &Listener, snap: &Snapshot, sessions: &[SessionTarget]) -> Option<Attribution> {
    let by_process = sessions
        .iter()
        .filter(|s| s.pid.is_some_and(|p| in_agent_tree(l.pid, p, &snap.procs)))
        .min_by(|a, b| a.session_id.cmp(&b.session_id));
    if let Some(s) = by_process {
        return Some(Attribution { source: PreviewPortSource::Session, session_id: s.session_id.clone() });
    }
    let cwd = snap.cwds.get(&l.pid)?;
    sessions
        .iter()
        .filter_map(|s| s.workdir.as_ref().filter(|w| cwd_within(cwd, w)).map(|w| (w.components().count(), s)))
        .max_by(|(da, a), (db, b)| da.cmp(db).then_with(|| b.session_id.cmp(&a.session_id)))
        .map(|(_, s)| Attribution { source: PreviewPortSource::Workspace, session_id: s.session_id.clone() })
}

// ---------------------------------------------------------------- 策略（§ 19.3）

/// 端口策略的输入。
pub struct Policy<'a> {
    pub enabled: bool,
    pub allow_ports: &'a [u16],
    /// 本机 hub 的端口（`hub.url` 的主机是回环地址时），永远不允许。
    pub hub_port: Option<u16>,
}

/// 一个端口现在能不能预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// `preview.enabled = false`。
    Disabled,
    /// 不符合任何一条规则（或永远不允许的端口）。
    NotAllowed,
}

/// 检查一个端口。`lenient`（隧道握手时）：此刻没有进程在监听也放行——dev server 可能正在重启，请求会得到
/// `RESET { refused }`；`preview.open` 时不放行（还没有东西可以预览）。
pub fn check_port(
    port: u16,
    policy: &Policy<'_>,
    snap: &Snapshot,
    sessions: &[SessionTarget],
    lenient: bool,
) -> Verdict {
    if !policy.enabled {
        return Verdict::Disabled;
    }
    if port == 0 || policy.hub_port == Some(port) {
        return Verdict::NotAllowed;
    }
    if policy.allow_ports.contains(&port) {
        return Verdict::Allowed;
    }
    if !snap.available {
        return Verdict::NotAllowed;
    }
    // 一个端口上可能有不止一个监听者（IPv4 / IPv6 各一个、`SO_REUSEPORT` 的多个进程）：每一个都要有归属才放行，
    // 否则 daemon 先连的 `127.0.0.1` 可能落到别的进程上
    let listeners: Vec<&Listener> = snap.listeners_on(port).collect();
    if listeners.is_empty() {
        return if lenient { Verdict::Allowed } else { Verdict::NotAllowed };
    }
    if listeners.iter().all(|l| attribute(l, snap, sessions).is_some()) {
        Verdict::Allowed
    } else {
        Verdict::NotAllowed
    }
}

/// 一个端口上的全部监听者都有归属时，挑一个代表（规则 1 优先于规则 2，再取 session id 最小的 / 第一个监听者）；
/// 只要有一个监听者没有归属（别的进程、别的会话），这个端口就没有「主人」，返回 `None`。
fn port_owner<'a>(
    listeners: &[&'a Listener],
    snap: &Snapshot,
    sessions: &[SessionTarget],
) -> Option<(Attribution, &'a Listener)> {
    let mut best: Option<(Attribution, &'a Listener)> = None;
    for &l in listeners {
        let a = attribute(l, snap, sessions)?;
        let better = best.as_ref().is_none_or(|(old, _)| {
            (a.source == PreviewPortSource::Session && old.source == PreviewPortSource::Workspace)
                || (a.source == old.source && a.session_id < old.session_id)
        });
        if better {
            best = Some((a, l));
        }
    }
    best
}

/// `preview.ports` 的结果。`only_session`：只列这个会话的（外加 `allow_ports` 里的）。
/// 端口上有任何一个监听者不属于会话（混合归属）就不列出——它不能预览（[`check_port`]），除非写进 `allow_ports`。
pub fn list_ports(
    policy: &Policy<'_>,
    snap: &Snapshot,
    sessions: &[SessionTarget],
    only_session: Option<&str>,
) -> Vec<PreviewPort> {
    let mut by_port: BTreeMap<u16, PreviewPort> = BTreeMap::new();
    let scoped: Vec<SessionTarget> = match only_session {
        Some(id) => sessions.iter().filter(|s| s.session_id == id).cloned().collect(),
        None => sessions.to_vec(),
    };
    if snap.available {
        let mut ports: Vec<u16> = snap.listeners.iter().map(|l| l.port).collect();
        ports.sort_unstable();
        ports.dedup();
        for port in ports {
            if port == 0 || policy.hub_port == Some(port) {
                continue;
            }
            // 混合归属的端口（有的监听者不是会话的）不列出：它不能预览，除非写进 `allow_ports`（下面按 config 列）
            let listeners: Vec<&Listener> = snap.listeners_on(port).collect();
            let Some((a, l)) = port_owner(&listeners, snap, &scoped) else { continue };
            by_port.insert(
                port,
                PreviewPort {
                    port,
                    source: a.source,
                    session_id: Some(a.session_id),
                    pid: Some(l.pid),
                    process: Some(snap.process_name(l)).filter(|n| !n.is_empty()),
                },
            );
        }
    }
    for &port in policy.allow_ports {
        if port == 0 || policy.hub_port == Some(port) || by_port.contains_key(&port) {
            continue;
        }
        let l = snap.listeners_on(port).next();
        by_port.insert(
            port,
            PreviewPort {
                port,
                source: PreviewPortSource::Config,
                session_id: None,
                pid: l.map(|l| l.pid),
                process: l.map(|l| snap.process_name(l)).filter(|n| !n.is_empty()),
            },
        );
    }
    by_port.into_values().collect()
}

/// `hub.url` 指向本机时它的端口（回环主机：`localhost` / `127.x.x.x` / `::1` / `0.0.0.0`）；端口缺省按 scheme 取 80 / 443。
/// 本机 hub 的端口永远不允许预览（否则预览页能通过隧道访问 hub 的内部接口）。
pub fn local_hub_port(hub_url: Option<&str>) -> Option<u16> {
    let url = hub_url?.trim();
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, tail) = v6.split_once(']')?;
        (h.to_string(), tail.strip_prefix(':').map(str::to_string))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (authority.to_string(), None),
        }
    };
    let host = host.to_ascii_lowercase();
    let loopback = host == "localhost"
        || host == "::1"
        || host == "::"
        || host == "0.0.0.0"
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a.is_loopback());
    if !loopback {
        return None;
    }
    match port {
        Some(p) => p.parse::<u16>().ok(),
        None => Some(if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss") { 443 } else { 80 }),
    }
}

// ---------------------------------------------------------------- 执行与缓存

/// 跑一条命令。拆成 trait 是为了测试里换成固定输出 / 固定失败，不依赖机器上真有 `lsof`。
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// 成功（拿到了输出，不论退出码）返回 stdout；命令不存在、超时、读不了输出返回原因。
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String>;

    /// 不靠命令、直接从系统拿整份探测结果（Windows）。`None`：照常跑命令。
    async fn native_snapshot(&self) -> Option<Result<Snapshot, String>> {
        None
    }
}

/// 真的起子进程：`env_clear` 后补 daemon 的环境，`kill_on_drop`，3 秒超时，输出有上限。
pub struct SystemRunner {
    env: Arc<HashMap<String, String>>,
}

impl SystemRunner {
    pub fn new(env: Arc<HashMap<String, String>>) -> SystemRunner {
        SystemRunner { env }
    }
}

#[async_trait::async_trait]
impl CommandRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .envs(self.env.iter())
            // 错误信息和字段格式不受用户 locale 影响
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{program} is not installed")
            } else {
                format!("cannot run {program}: {e}")
            }
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| format!("cannot read the output of {program}"))?;
        let work = async {
            let mut out = Vec::new();
            let mut limited = (&mut stdout).take(OUTPUT_CAP as u64);
            limited.read_to_end(&mut out).await.map_err(|e| format!("cannot read the output of {program}: {e}"))?;
            // 读完 stdout 之后收尸；退出码不管（`lsof` 没有匹配时退出 1）
            let _ = child.wait().await;
            Ok::<_, String>(out)
        };
        match tokio::time::timeout(COMMAND_TIMEOUT, work).await {
            Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Ok(Err(e)) => Err(e),
            // 超时：future 被丢弃，`kill_on_drop` 杀掉子进程
            Err(_) => Err(format!("{program} timed out after {}s", COMMAND_TIMEOUT.as_secs())),
        }
    }

    #[cfg(windows)]
    async fn native_snapshot(&self) -> Option<Result<Snapshot, String>> {
        let snap = tokio::task::spawn_blocking(win::snapshot).await.map_err(|e| format!("port detection failed: {e}"));
        Some(snap.and_then(|r| r))
    }
}

/// 端口探测器：跑命令、解析、缓存 2 秒。多个调用方同时要结果时共用同一次探测。
pub struct PortDetector {
    runner: RwLock<Arc<dyn CommandRunner>>,
    cache: AsyncMutex<Option<(Instant, Arc<Snapshot>)>>,
    warned: Mutex<HashMap<String, Instant>>,
}

impl PortDetector {
    pub fn new(runner: Arc<dyn CommandRunner>) -> PortDetector {
        PortDetector { runner: RwLock::new(runner), cache: AsyncMutex::new(None), warned: Mutex::new(HashMap::new()) }
    }

    pub fn system(env: Arc<HashMap<String, String>>) -> PortDetector {
        PortDetector::new(Arc::new(SystemRunner::new(env)))
    }

    /// 换命令执行器并丢掉缓存（测试用）。
    pub fn set_runner(&self, runner: Arc<dyn CommandRunner>) {
        *self.runner.write().unwrap() = runner;
        if let Ok(mut c) = self.cache.try_lock() {
            *c = None;
        }
    }

    /// 最近 2 秒内的探测结果；没有就探测一次。
    pub async fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot_aged().await.0
    }

    /// [`PortDetector::snapshot`]，同时给出这份结果已经多旧了。
    pub async fn snapshot_aged(&self) -> (Arc<Snapshot>, Duration) {
        let mut cache = self.cache.lock().await;
        if let Some((at, snap)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return (snap.clone(), at.elapsed());
            }
        }
        let snap = Arc::new(self.detect().await);
        *cache = Some((Instant::now(), snap.clone()));
        (snap, Duration::ZERO)
    }

    /// 不管缓存，重新探测一次（刚被拒绝的端口可能是缓存之后才开始监听的）。
    pub async fn refresh(&self) -> Arc<Snapshot> {
        let mut cache = self.cache.lock().await;
        let snap = Arc::new(self.detect().await);
        *cache = Some((Instant::now(), snap.clone()));
        snap
    }

    /// 丢掉缓存（测试用）。
    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    async fn detect(&self) -> Snapshot {
        let runner = self.runner.read().unwrap().clone();
        match self.detect_with(runner.as_ref()).await {
            Ok(s) => s,
            Err(why) => {
                self.warn_once(&why);
                Snapshot::unavailable()
            }
        }
    }

    async fn detect_with(&self, runner: &dyn CommandRunner) -> Result<Snapshot, String> {
        if let Some(snap) = runner.native_snapshot().await {
            return snap;
        }
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lsof = runner.run("lsof", &args(&["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])).await?;
        let listeners = parse_lsof_listeners(&lsof);
        let ps = runner.run("ps", &args(&["-axo", "pid=,ppid=,pgid=,comm="])).await?;
        let procs = parse_ps(&ps);
        let mut pids: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        pids.truncate(MAX_CWD_PIDS);
        let cwds = if pids.is_empty() {
            HashMap::new()
        } else {
            let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            parse_lsof_cwd(&runner.run("lsof", &args(&["-a", "-p", &list, "-d", "cwd", "-Fpn"])).await?)
        };
        Ok(Snapshot { available: true, listeners, procs, cwds })
    }

    /// 同一原因 10 分钟内只记一次 warn。
    fn warn_once(&self, why: &str) {
        let mut warned = self.warned.lock().unwrap();
        let now = Instant::now();
        if warned.get(why).is_some_and(|at| now.duration_since(*at) < WARN_INTERVAL) {
            return;
        }
        warned.insert(why.to_string(), now);
        tracing::warn!(
            reason = why,
            "preview port detection is unavailable; only the ports in preview.allow_ports can be previewed"
        );
    }

    /// 这个原因的 warn 现在会不会被抑制（测试用）。
    #[cfg(test)]
    fn warned_recently(&self, why: &str) -> bool {
        self.warned.lock().unwrap().get(why).is_some_and(|at| at.elapsed() < WARN_INTERVAL)
    }
}

#[cfg(windows)]
mod win {
    //! Windows 的探测：监听 socket（IPv4 / IPv6）、进程表、agent 的 job。

    use std::collections::{HashMap, HashSet};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, INVALID_HANDLE_VALUE, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    use super::{parse_listen_name, Listener, ProcInfo, Snapshot};

    pub(super) fn snapshot() -> Result<Snapshot, String> {
        let mut procs = processes()?;
        // job 里的进程（agent 派生的，哪怕父进程链已经断了）的「进程组」是 agent 主进程
        for (leader, members) in anycon_adapters::common::proctree::job_members() {
            for pid in members {
                if let Some(p) = procs.get_mut(&pid) {
                    p.pgid = leader;
                }
            }
        }
        let mut listeners = Vec::new();
        let mut seen = HashSet::new();
        for (name, pid) in tcp_listeners()? {
            if let Some(port) = parse_listen_name(&name) {
                if seen.insert((pid, port)) {
                    let command = procs.get(&pid).map(|p| p.comm.clone()).unwrap_or_default();
                    listeners.push(Listener { port, pid, command });
                }
            }
        }
        Ok(Snapshot { available: true, listeners, procs, cwds: HashMap::new() })
    }

    /// 所有处于 LISTEN 的 TCP socket：`(lsof 写法的地址, pid)`，如 `127.0.0.1:3000`、`[::]:3000`。
    fn tcp_listeners() -> Result<Vec<(String, u32)>, String> {
        let mut out = Vec::new();
        let v4 = table(AF_INET as u32)?;
        if v4.len() >= std::mem::size_of::<u32>() {
            // SAFETY: 成功返回的缓冲开头是 MIB_TCPTABLE_OWNER_PID，后面接着 dwNumEntries 行。
            unsafe {
                let t = v4.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
                let rows = std::slice::from_raw_parts(
                    std::ptr::addr_of!((*t).table) as *const MIB_TCPROW_OWNER_PID,
                    (*t).dwNumEntries as usize,
                );
                for r in rows {
                    let ip = std::net::Ipv4Addr::from(u32::from_be(r.dwLocalAddr));
                    out.push((format!("{ip}:{}", u16::from_be(r.dwLocalPort as u16)), r.dwOwningPid));
                }
            }
        }
        let v6 = table(AF_INET6 as u32)?;
        if v6.len() >= std::mem::size_of::<u32>() {
            // SAFETY: 同上，IPv6 的表。
            unsafe {
                let t = v6.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID;
                let rows = std::slice::from_raw_parts(
                    std::ptr::addr_of!((*t).table) as *const MIB_TCP6ROW_OWNER_PID,
                    (*t).dwNumEntries as usize,
                );
                for r in rows {
                    let ip = std::net::Ipv6Addr::from(r.ucLocalAddr);
                    out.push((format!("[{ip}]:{}", u16::from_be(r.dwLocalPort as u16)), r.dwOwningPid));
                }
            }
        }
        Ok(out)
    }

    /// `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` 的原始缓冲（u32 对齐）。
    fn table(family: u32) -> Result<Vec<u32>, String> {
        let mut size = 0u32;
        for _ in 0..8 {
            let mut buf = vec![0u32; (size as usize).div_ceil(4).max(1)];
            // SAFETY: 缓冲长度如实传入；不够时系统把需要的长度写回 `size`。
            let rc = unsafe {
                GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0)
            };
            match rc {
                NO_ERROR => return Ok(buf),
                ERROR_INSUFFICIENT_BUFFER => continue,
                e => return Err(format!("GetExtendedTcpTable failed ({e})")),
            }
        }
        Err("GetExtendedTcpTable: the table kept growing".into())
    }

    /// 进程表：pid → (父进程, 可执行文件名)。`pgid` 先填 0，由调用方按 job 补上。
    fn processes() -> Result<HashMap<u32, ProcInfo>, String> {
        // SAFETY: 参数是常量；失败返回 INVALID_HANDLE_VALUE。
        let h = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("CreateToolhelp32Snapshot failed: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: 刚创建的快照句柄，由 OwnedHandle 负责关闭。
        let snap = unsafe { OwnedHandle::from_raw_handle(h) };
        let mut m = HashMap::new();
        // SAFETY: 全零是合法值；dwSize 必须先设好。
        let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: 快照句柄有效，`e` 由调用写入。
        let mut ok = unsafe { Process32FirstW(snap.as_raw_handle(), &mut e) } != 0;
        while ok {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            let comm = String::from_utf16_lossy(&e.szExeFile[..len]);
            m.insert(e.th32ProcessID, ProcInfo { ppid: e.th32ParentProcessID, pgid: 0, comm });
            // SAFETY: 同上。
            ok = unsafe { Process32NextW(snap.as_raw_handle(), &mut e) } != 0;
        }
        Ok(m)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sees_a_listener_of_this_process() {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let snap = snapshot().unwrap();
            assert!(snap.available);
            let me = std::process::id();
            assert!(snap.listeners.iter().any(|x| x.port == port && x.pid == me), "{:?}", snap.listeners);
            assert!(snap.procs.contains_key(&me));
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// macOS 的 `lsof -nP -iTCP -sTCP:LISTEN -Fpcn`：带 `f<fd>` 行；双栈通配监听显示成两条 `*:port`。
    const LSOF_MACOS: &str = "p394\ncrapportd\nf4\nn*:49152\nf5\nn*:49152\np479\ncnode\nf35\nn127.0.0.1:18789\nf36\nn[::1]:18789\n\
        p548\nccom.docker.backend\nf100\nn*:6379\nf101\nn*:5432\np700\ncVite\nf20\nn[::1]:5173\np701\ncpython3\nf8\nn192.168.1.5:8000\n";

    /// Linux 的 `lsof -nP -iTCP -sTCP:LISTEN -Fpcn`：没有 `f` 行；IPv4 / IPv6 通配分别是 `*:port` 和 `[::]:port`。
    const LSOF_LINUX: &str = "p1021\ncsshd\nn*:22\nn[::]:22\np2042\ncnode\nn127.0.0.1:3000\np2043\ncpostgres\nn127.0.0.1:5432\nn[::1]:5432\n\
        p2044\ncnginx\nn0.0.0.0:80\nn[fe80::1%eth0]:8080\nn127.0.0.2:9000\n";

    const PS_MACOS: &str = "    1     0     1 /sbin/launchd\n  100     1   100 /usr/libexec/logd\n\
          479     1   479 /usr/local/bin/node\n  700   650   650 /Users/me/Library/Application Support/Vite Dev/node\n\
          650   640   650 /bin/zsh\n  640     1   640 /Applications/Claude Code.app/Contents/MacOS/claude\n";

    const PS_LINUX: &str = "  PID\n 1 0 1 systemd\n2042 2000 2000 node\n2000 1999 2000 bash\n1999 1 1999 claude\n";

    #[test]
    fn listen_names_accept_loopback_and_wildcard_only() {
        for (n, p) in [
            ("*:3000", Some(3000)),
            ("0.0.0.0:3000", Some(3000)),
            ("127.0.0.1:5173", Some(5173)),
            ("[::]:80", Some(80)),
            ("[::1]:8080", Some(8080)),
            ("192.168.1.5:3000", None),
            ("10.0.0.2:3000", None),
            ("127.0.0.2:3000", None),
            ("[fe80::1%en0]:3000", None),
            ("[2001:db8::1]:3000", None),
            ("*:0", None),
            ("*:70000", None),
            ("*:x", None),
            ("garbage", None),
            ("", None),
        ] {
            assert_eq!(parse_listen_name(n), p, "{n}");
        }
    }

    #[test]
    fn lsof_output_from_macos_is_parsed_with_dedup_and_lan_exclusion() {
        let l = parse_lsof_listeners(LSOF_MACOS);
        let got: Vec<(u32, u16, &str)> = l.iter().map(|l| (l.pid, l.port, l.command.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (394, 49152, "rapportd"), // 两条 `*:49152` 只记一次
                (479, 18789, "node"),     // 127.0.0.1 和 ::1 都监听，只记一次
                (548, 6379, "com.docker.backend"),
                (548, 5432, "com.docker.backend"),
                (700, 5173, "Vite"),
                // 701 只绑在局域网地址 192.168.1.5：排除
            ]
        );
    }

    #[test]
    fn lsof_output_from_linux_is_parsed() {
        let l = parse_lsof_listeners(LSOF_LINUX);
        let got: Vec<(u32, u16)> = l.iter().map(|l| (l.pid, l.port)).collect();
        // sshd 的 *:22 和 [::]:22 合并；nginx 的 0.0.0.0:80 要，fe80 链路本地地址和 127.0.0.2 不要
        assert_eq!(got, vec![(1021, 22), (2042, 3000), (2043, 5432), (2044, 80)]);
        assert!(parse_lsof_listeners("").is_empty());
        assert!(parse_lsof_listeners("not lsof output at all\n").is_empty());
        // 缺 pid 的 `n` 行不能误认
        assert!(parse_lsof_listeners("n*:3000\n").is_empty());
    }

    #[test]
    fn ps_output_is_parsed_with_spaces_in_comm() {
        let m = parse_ps(PS_MACOS);
        assert_eq!(m[&1], ProcInfo { ppid: 0, pgid: 1, comm: "/sbin/launchd".into() });
        assert_eq!(m[&479].comm, "/usr/local/bin/node");
        assert_eq!(
            m[&700],
            ProcInfo { ppid: 650, pgid: 650, comm: "/Users/me/Library/Application Support/Vite Dev/node".into() }
        );
        assert_eq!(m[&640].comm, "/Applications/Claude Code.app/Contents/MacOS/claude");
        // 没有表头的 `pid=` 输出；Linux 样例里的表头行（`PID`）被跳过
        let l = parse_ps(PS_LINUX);
        assert_eq!(l.len(), 4);
        assert_eq!(l[&2042], ProcInfo { ppid: 2000, pgid: 2000, comm: "node".into() });
        assert!(parse_ps("").is_empty() && parse_ps("garbage line\n").is_empty());
    }

    #[test]
    fn lsof_cwd_output_is_parsed_and_unreadable_cwds_are_skipped() {
        let out = "p479\nfcwd\nn/Users/me/proj\np548\nfcwd\nn/Users/me/Library/Containers/x y\np1\nn/proc/1/cwd (readlink: Permission denied)\np9\nn(stat: Permission denied)\n";
        let m = parse_lsof_cwd(out);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&479], PathBuf::from("/Users/me/proj"));
        assert_eq!(m[&548], PathBuf::from("/Users/me/Library/Containers/x y"));
        assert!(parse_lsof_cwd("").is_empty());
    }

    fn procs(list: &[(u32, u32, u32)]) -> HashMap<u32, ProcInfo> {
        list.iter().map(|&(p, pp, pg)| (p, ProcInfo { ppid: pp, pgid: pg, comm: format!("p{p}") })).collect()
    }

    #[test]
    fn the_process_tree_rule_follows_parents_and_stops_at_init_and_cycles() {
        // 1(init) <- 100(agent) <- 200(bash) <- 300(node)
        let p = procs(&[(1, 0, 1), (100, 1, 100), (200, 100, 100), (300, 200, 300), (999, 1, 999)]);
        assert!(chain_reaches(300, 100, &p));
        assert!(chain_reaches(100, 100, &p), "the agent itself counts");
        assert!(!chain_reaches(999, 100, &p));
        assert!(!chain_reaches(300, 777, &p), "unknown target");
        // pid 1 不会被当成会话进程
        assert!(!in_agent_tree(300, 1, &p), "pid 1 is never an agent");
        // 环
        let cyc = procs(&[(10, 11, 10), (11, 10, 11)]);
        assert!(!chain_reaches(10, 12, &cyc));
        // 父进程不在 ps 输出里（刚退出）：断链
        let broken = procs(&[(300, 250, 300)]);
        assert!(!chain_reaches(300, 100, &broken));
    }

    #[test]
    fn the_process_group_rule_matches_background_children_that_were_reparented() {
        // dev server 被 reparent 到 init（父进程链断了），但还在 agent 的进程组里
        let p = procs(&[(100, 1, 100), (300, 1, 100)]);
        assert!(!chain_reaches(300, 100, &p));
        assert!(in_agent_tree(300, 100, &p));
        // 自己 setsid 出去的（进程组也不同）：不算
        let detached = procs(&[(100, 1, 100), (300, 1, 300)]);
        assert!(!in_agent_tree(300, 100, &detached));
    }

    #[test]
    fn the_working_directory_rule_compares_path_components() {
        assert!(cwd_within(Path::new("/w/proj"), Path::new("/w/proj")));
        assert!(cwd_within(Path::new("/w/proj/web/dist"), Path::new("/w/proj")));
        assert!(!cwd_within(Path::new("/w/proj-other"), Path::new("/w/proj")), "a sibling with a common prefix");
        assert!(!cwd_within(Path::new("/w"), Path::new("/w/proj")));
    }

    fn target(id: &str, pid: Option<u32>, dir: Option<&str>) -> SessionTarget {
        SessionTarget { session_id: id.into(), pid, workdir: dir.map(PathBuf::from) }
    }

    fn snap(listeners: &[(u32, u16)], procs: HashMap<u32, ProcInfo>, cwds: &[(u32, &str)]) -> Snapshot {
        Snapshot {
            available: true,
            listeners: listeners
                .iter()
                .map(|&(pid, port)| Listener { port, pid, command: format!("cmd{pid}") })
                .collect(),
            procs,
            cwds: cwds.iter().map(|&(p, d)| (p, PathBuf::from(d))).collect(),
        }
    }

    #[test]
    fn attribution_prefers_the_process_rule_and_the_most_specific_workdir() {
        let p = procs(&[(100, 1, 100), (300, 100, 100), (400, 1, 400), (500, 1, 500)]);
        let s =
            snap(&[(300, 3000), (400, 4000), (500, 5000)], p, &[(300, "/other"), (400, "/w/a/web"), (500, "/nowhere")]);
        let sessions = vec![
            target("s_b", Some(100), Some("/w/b")),
            target("s_a", None, Some("/w/a")),
            target("s_a_wt", None, Some("/w/a/web")),
        ];
        let l = |pid| s.listeners.iter().find(|l| l.pid == pid).unwrap();
        // 300 在 s_b 的 agent 进程树里，工作目录不相干也算
        assert_eq!(
            attribute(l(300), &s, &sessions),
            Some(Attribution { source: PreviewPortSource::Session, session_id: "s_b".into() })
        );
        // 400 没有会话进程，但 cwd 同时在 /w/a 和 /w/a/web 里：取更深的
        assert_eq!(
            attribute(l(400), &s, &sessions),
            Some(Attribution { source: PreviewPortSource::Workspace, session_id: "s_a_wt".into() })
        );
        // 500 哪条都不符合
        assert_eq!(attribute(l(500), &s, &sessions), None);
        // 只看某个会话时，别的会话命中的不算
        assert_eq!(attribute(l(300), &s, &sessions[1..]), None);
    }

    fn policy<'a>(enabled: bool, allow: &'a [u16], hub: Option<u16>) -> Policy<'a> {
        Policy { enabled, allow_ports: allow, hub_port: hub }
    }

    #[test]
    fn the_port_policy_follows_section_19_3() {
        let p = procs(&[(100, 1, 100), (300, 100, 100), (600, 1, 600)]);
        let s = snap(&[(300, 3000), (600, 6000), (600, 8081)], p, &[]);
        let sessions = vec![target("s1", Some(100), None)];
        let pol = policy(true, &[9000], Some(8081));
        // 会话自己起的：允许
        assert_eq!(check_port(3000, &pol, &s, &sessions, false), Verdict::Allowed);
        // 别人的服务：不允许（严格和宽松都不允许：有进程在监听但不符合）
        assert_eq!(check_port(6000, &pol, &s, &sessions, false), Verdict::NotAllowed);
        assert_eq!(check_port(6000, &pol, &s, &sessions, true), Verdict::NotAllowed);
        // 配置里的：允许，不管有没有人监听
        assert_eq!(check_port(9000, &pol, &s, &sessions, false), Verdict::Allowed);
        // 没人监听：`preview.open` 不放行，握手放行
        assert_eq!(check_port(4000, &pol, &s, &sessions, false), Verdict::NotAllowed);
        assert_eq!(check_port(4000, &pol, &s, &sessions, true), Verdict::Allowed);
        // 本机 hub 的端口、端口 0：永远不允许，写进 allow_ports 也不行
        let pol_hub = policy(true, &[8081, 0], Some(8081));
        assert_eq!(check_port(8081, &pol_hub, &s, &sessions, true), Verdict::NotAllowed);
        assert_eq!(check_port(0, &pol_hub, &s, &sessions, true), Verdict::NotAllowed);
        // 预览关着
        assert_eq!(check_port(3000, &policy(false, &[], None), &s, &sessions, true), Verdict::Disabled);
        // 探测不可用：只认 allow_ports（宽松也不放行，因为分不出有没有人监听）
        let none = Snapshot::unavailable();
        assert_eq!(check_port(3000, &pol, &none, &sessions, true), Verdict::NotAllowed);
        assert_eq!(check_port(9000, &pol, &none, &sessions, true), Verdict::Allowed);
    }

    #[test]
    fn every_listener_on_a_port_must_belong_to_a_session() {
        // 会话在 [::1]:3000（进程 300），别的进程 600 在 127.0.0.1:3000：daemon 先连 127.0.0.1 会落到 600 上
        let p = procs(&[(100, 1, 100), (300, 100, 100), (600, 1, 600), (601, 1, 601)]);
        let sessions = vec![target("s1", Some(100), None)];
        let pol = policy(true, &[], None);
        let mixed = snap(&[(300, 3000), (600, 3000), (300, 4000), (601, 5000), (600, 5000)], p.clone(), &[]);
        assert_eq!(check_port(3000, &pol, &mixed, &sessions, false), Verdict::NotAllowed);
        assert_eq!(check_port(3000, &pol, &mixed, &sessions, true), Verdict::NotAllowed, "also at handshake time");
        // 只有会话的监听者：允许
        assert_eq!(check_port(4000, &pol, &mixed, &sessions, false), Verdict::Allowed);
        // 全是别人的：不允许
        assert_eq!(check_port(5000, &pol, &mixed, &sessions, false), Verdict::NotAllowed);
        // SO_REUSEPORT：两个进程都在会话的进程树里，允许
        let both = snap(&[(300, 3000), (301, 3000)], procs(&[(100, 1, 100), (300, 100, 100), (301, 100, 100)]), &[]);
        assert_eq!(check_port(3000, &pol, &both, &sessions, false), Verdict::Allowed);
        // 一个靠进程树、一个靠工作目录（两个会话各一个）也行：每一个都有归属
        let two = vec![target("s1", Some(100), None), target("s2", None, Some("/w/b"))];
        let split = snap(
            &[(300, 3000), (400, 3000)],
            procs(&[(100, 1, 100), (300, 100, 100), (400, 1, 400)]),
            &[(400, "/w/b/web")],
        );
        assert_eq!(check_port(3000, &pol, &split, &two, false), Verdict::Allowed);
        // allow_ports 照旧直接放行，不看混合
        let cfg = policy(true, &[3000], None);
        assert_eq!(check_port(3000, &cfg, &mixed, &sessions, false), Verdict::Allowed);
    }

    #[test]
    fn a_port_with_mixed_ownership_is_not_listed_unless_it_is_configured() {
        let p = procs(&[(100, 1, 100), (300, 100, 100), (600, 1, 600), (700, 1, 700)]);
        let s = snap(&[(300, 3000), (600, 3000), (300, 4000), (700, 4000)], p, &[(700, "/w/other")]);
        let sessions = vec![target("s1", Some(100), None), target("s2", None, Some("/w/other"))];
        let pol = policy(true, &[], None);
        let ports = |only: Option<&str>, pol: &Policy<'_>| -> Vec<(u16, Option<String>)> {
            list_ports(pol, &s, &sessions, only).into_iter().map(|p| (p.port, p.session_id)).collect()
        };
        // 3000：s1 的 + 别人的 -> 不列；4000：s1 的 + s2 的，每个都有主人 -> 列出（代表取规则 1 的 s1）
        assert_eq!(ports(None, &pol), vec![(4000, Some("s1".into()))]);
        // 只看 s1：4000 上有 s2 的监听者，对 s1 来说是「别人的」-> 不列
        assert_eq!(ports(Some("s1"), &pol), vec![]);
        // 写进 allow_ports 的混合端口按 config 列出
        let cfg = policy(true, &[3000], None);
        let listed = list_ports(&cfg, &s, &sessions, None);
        let c = listed.iter().find(|p| p.port == 3000).unwrap();
        assert_eq!((c.source, c.session_id.clone()), (PreviewPortSource::Config, None));
    }

    #[test]
    fn the_port_list_names_the_source_and_filters_by_session() {
        let p = procs(&[(100, 1, 100), (300, 100, 100), (400, 1, 400), (600, 1, 600)]);
        let mut s = snap(&[(300, 3000), (400, 4000), (600, 6000), (600, 9000)], p, &[(400, "/w/a")]);
        s.procs.get_mut(&300).unwrap().comm = "/usr/local/bin/node".into();
        let sessions = vec![target("s1", Some(100), None), target("s2", None, Some("/w/a"))];
        let pol = policy(true, &[9000, 7000], None);
        let all = list_ports(&pol, &s, &sessions, None);
        let summary: Vec<(u16, PreviewPortSource, Option<&str>)> =
            all.iter().map(|p| (p.port, p.source, p.session_id.as_deref())).collect();
        assert_eq!(
            summary,
            vec![
                (3000, PreviewPortSource::Session, Some("s1")),
                (4000, PreviewPortSource::Workspace, Some("s2")),
                (7000, PreviewPortSource::Config, None),
                (9000, PreviewPortSource::Config, None),
            ]
        );
        assert_eq!(all[0].pid, Some(300));
        assert_eq!(all[0].process.as_deref(), Some("node"), "ps comm, base name only");
        assert_eq!(all[3].pid, Some(600), "a configured port shows who listens on it");
        assert_eq!(all[2].pid, None);
        // 只看 s2：s1 的 3000 不列，config 的照列
        let only: Vec<u16> = list_ports(&pol, &s, &sessions, Some("s2")).iter().map(|p| p.port).collect();
        assert_eq!(only, vec![4000, 7000, 9000]);
        // 不认识的会话：只剩 config
        let none: Vec<u16> = list_ports(&pol, &s, &sessions, Some("nope")).iter().map(|p| p.port).collect();
        assert_eq!(none, vec![7000, 9000]);
        // 探测不可用：只列 config
        let un = list_ports(&pol, &Snapshot::unavailable(), &sessions, None);
        assert_eq!(un.iter().map(|p| p.port).collect::<Vec<_>>(), vec![7000, 9000]);
        assert_eq!(Snapshot::unavailable().detection(), PreviewDetection::Unavailable);
        assert_eq!(s.detection(), PreviewDetection::Ok);
    }

    #[test]
    fn local_hub_ports() {
        for (url, port) in [
            ("http://127.0.0.1:8080", Some(8080)),
            ("http://localhost:8080/", Some(8080)),
            ("http://LOCALHOST", Some(80)),
            ("https://localhost", Some(443)),
            ("ws://[::1]:9000/x", Some(9000)),
            ("http://127.0.0.5:81", Some(81)),
            ("http://user:pw@localhost:8081/p?q=1#f", Some(8081)),
            ("https://hub.example.com:443", None),
            ("https://hub.example.com", None),
            ("http://192.168.1.2:8080", None),
            ("http://10.0.0.1", None),
            ("not a url", None),
            ("", None),
        ] {
            assert_eq!(local_hub_port(Some(url)), port, "{url}");
        }
        assert_eq!(local_hub_port(None), None);
    }

    // ------------------------------------------------ 执行与缓存

    struct FakeRunner {
        calls: AtomicUsize,
        delay: Duration,
        /// 命令名 -> 输出；没有的命令返回 "not installed"。
        outputs: Mutex<HashMap<String, Result<String, String>>>,
    }

    impl FakeRunner {
        fn new(outputs: &[(&str, Result<&str, &str>)]) -> Arc<FakeRunner> {
            Arc::new(FakeRunner {
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
                outputs: Mutex::new(
                    outputs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.map(str::to_string).map_err(str::to_string)))
                        .collect(),
                ),
            })
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            // lsof 被调用两次：监听列表和 cwd，按参数区分
            let key = if program == "lsof" && args.iter().any(|a| a == "cwd") { "lsof-cwd" } else { program };
            self.outputs.lock().unwrap().get(key).cloned().unwrap_or_else(|| Err(format!("{program} is not installed")))
        }
    }

    #[tokio::test]
    async fn the_detector_runs_three_commands_and_caches_for_two_seconds() {
        let runner = FakeRunner::new(&[
            ("lsof", Ok("p300\ncnode\nn*:3000\n")),
            ("ps", Ok("300 100 100 /usr/bin/node\n100 1 100 /bin/claude\n")),
            ("lsof-cwd", Ok("p300\nfcwd\nn/w/proj\n")),
        ]);
        let det = PortDetector::new(runner.clone());
        let a = det.snapshot().await;
        assert!(a.available);
        assert_eq!(a.listeners.len(), 1);
        assert_eq!(a.cwds[&300], PathBuf::from("/w/proj"));
        assert_eq!(a.procs[&300].ppid, 100);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);
        let _ = det.snapshot().await;
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3, "served from the cache");
        det.invalidate().await;
        let _ = det.snapshot().await;
        assert_eq!(runner.calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn a_snapshot_reports_its_age_and_refresh_ignores_the_cache() {
        let runner = FakeRunner::new(&[("lsof", Ok("")), ("ps", Ok(""))]);
        let det = PortDetector::new(runner.clone());
        let (_, age) = det.snapshot_aged().await;
        assert_eq!(age, Duration::ZERO, "fresh");
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, age) = det.snapshot_aged().await;
        assert!(age >= Duration::from_millis(30) && age < CACHE_TTL, "{age:?}");
        assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
        let _ = det.refresh().await;
        assert_eq!(runner.calls.load(Ordering::SeqCst), 4, "refresh runs the commands again");
        let (_, age) = det.snapshot_aged().await;
        assert!(age < Duration::from_millis(30), "and the new result is what the cache holds");
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_detection() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(50),
            outputs: Mutex::new(
                [("lsof", Ok(String::new())), ("ps", Ok(String::new()))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ),
        });
        let det = Arc::new(PortDetector::new(runner.clone()));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let d = det.clone();
                tokio::spawn(async move { d.snapshot().await.available })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap());
        }
        // 没有监听进程就不查 cwd：lsof + ps 两次
        assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failing_step_degrades_to_unavailable_and_warns_once_per_reason() {
        for failing in ["lsof", "ps", "lsof-cwd"] {
            let mut outputs: Vec<(&str, Result<&str, &str>)> = vec![
                ("lsof", Ok("p300\ncnode\nn*:3000\n")),
                ("ps", Ok("300 1 300 node\n")),
                ("lsof-cwd", Ok("p300\nn/w\n")),
            ];
            for o in outputs.iter_mut() {
                if o.0 == failing {
                    o.1 = Err("boom");
                }
            }
            let det = PortDetector::new(FakeRunner::new(&outputs));
            let s = det.snapshot().await;
            assert!(!s.available, "{failing}");
            assert!(s.listeners.is_empty(), "nothing partial leaks out");
            assert!(det.warned_recently("boom"));
        }
        // 命令不存在
        let det = PortDetector::new(FakeRunner::new(&[]));
        assert_eq!(det.snapshot().await.detection(), PreviewDetection::Unavailable);
        assert!(det.warned_recently("lsof is not installed"));
    }

    #[tokio::test]
    async fn the_system_runner_clears_the_environment_and_reports_missing_programs() {
        let env: Arc<HashMap<String, String>> = Arc::new(
            [("PATH".to_string(), "/usr/bin:/bin".to_string()), ("ANYCON_X".to_string(), "from-daemon".to_string())]
                .into(),
        );
        let r = SystemRunner::new(env);
        let out = r.run("env", &[]).await.unwrap();
        assert!(out.contains("ANYCON_X=from-daemon"), "the daemon environment is passed on");
        assert!(!out.contains("HOME="), "everything else of the parent environment is cleared");
        let e = r.run("anycon-no-such-program-xyz", &[]).await.unwrap_err();
        assert!(e.contains("not installed"), "{e}");
    }
}
