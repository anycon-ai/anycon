//! 找 agent 二进制（docs/spec/daemon.md § 5.3 第 1 步）。
//!
//! 顺序：`config.command`（绝对路径直接用）→ `which -a`（用 daemon 环境的 `PATH`）→ 已知目录。
//! 对每个候选跑 `--version`（2 秒超时），取第一个能执行的——nvm / volta 的坏 shim 在这里被跳过。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{parse_command, CommandSpec};
use super::run::run_capture;

/// 候选路径从哪来，`doctor` 展示用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandOrigin {
    /// `config.agents.<id>.command` 给的路径。
    Config,
    /// daemon 环境 `PATH` 里的 `which -a`。
    Path,
    /// 已知安装位置（`~/.local/bin` 等）。
    WellKnown,
}

impl CommandOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandOrigin::Config => "config",
            CommandOrigin::Path => "PATH",
            CommandOrigin::WellKnown => "well-known location",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateProbe {
    /// 没试（前面的已经成功，或不需要）。
    NotRun,
    /// `--version` 成功，附输出（stdout，空则 stderr）。
    Ok { output: String },
    /// `--version` 失败，附原因。
    Failed { reason: String },
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub origin: CommandOrigin,
    pub probe: CandidateProbe,
}

#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// `config.command` 的原文（诊断用）。
    pub config_command: Option<String>,
    /// `config.command` 解析失败时的原因。
    pub config_error: Option<String>,
    pub candidates: Vec<Candidate>,
    /// 选中的候选下标。
    pub chosen: Option<usize>,
    /// 跟在程序后面的固定参数（来自 `config.command` 里的多余词）。
    pub prefix_args: Vec<String>,
}

impl Resolution {
    pub fn chosen(&self) -> Option<&Candidate> {
        self.chosen.and_then(|i| self.candidates.get(i))
    }

    pub fn path(&self) -> Option<&Path> {
        self.chosen().map(|c| c.path.as_path())
    }

    /// 选中候选的 `--version` 输出。
    pub fn version_output(&self) -> Option<&str> {
        match self.chosen().map(|c| &c.probe) {
            Some(CandidateProbe::Ok { output }) => Some(output.as_str()),
            _ => None,
        }
    }

    /// 全部候选里「找到了文件但没一个能跑」时的汇总原因。
    pub fn failure_summary(&self) -> Option<String> {
        if let Some(e) = &self.config_error {
            return Some(format!("invalid command in config: {e}"));
        }
        let failed: Vec<String> = self
            .candidates
            .iter()
            .filter_map(|c| match &c.probe {
                CandidateProbe::Failed { reason } => Some(format!("{}: {reason}", c.path.display())),
                _ => None,
            })
            .collect();
        if failed.is_empty() {
            None
        } else {
            Some(failed.join("; "))
        }
    }
}

pub struct ResolveOptions<'a> {
    /// daemon 环境。只用 `PATH` 和 `HOME`（找文件），子进程也用它整体作为环境。
    pub env: &'a HashMap<String, String>,
    /// `config.agents.<id>.command`。
    pub config_command: Option<&'a str>,
    /// 没有配置时找的命令名，如 `claude`。
    pub default_name: &'a str,
    /// 探活用的参数，通常 `["--version"]`。
    pub probe_args: &'a [&'a str],
    /// 单个候选 `--version` 的超时（规格：2 秒）。
    pub per_candidate_timeout: Duration,
    /// 探活子进程的 cwd（workspace 之外的临时目录）。
    pub cwd: &'a Path,
    /// true = 所有候选都跑一遍（`doctor` 用）；false = 第一个成功就停。
    pub probe_all: bool,
}

/// 已知目录里要找的文件名：unix 就是 `name`；Windows 上按 `PATHEXT` 的常见扩展名（`claude.exe`、npm 装的 `codex.cmd`）。
fn executable_names(name: &str) -> Vec<String> {
    if cfg!(windows) && Path::new(name).extension().is_none() {
        [".exe", ".cmd", ".bat", ".com"].iter().map(|ext| format!("{name}{ext}")).collect()
    } else {
        vec![name.to_string()]
    }
}

/// 已知安装位置。`~/.nvm/versions/node/*/bin` 展开成各个已装的 node 版本。Windows 上是原生安装器、npm、scoop 等的目录。
#[cfg(windows)]
pub fn well_known_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = home {
        // Claude Code 的原生安装器、bun、cargo、npm 全局（`%APPDATA%\npm`）、scoop、volta
        dirs.push(h.join(".local").join("bin"));
        dirs.push(h.join(".bun").join("bin"));
        dirs.push(h.join(".cargo").join("bin"));
        dirs.push(h.join("AppData").join("Roaming").join("npm"));
        dirs.push(h.join("scoop").join("shims"));
        dirs.push(h.join("AppData").join("Local").join("Volta").join("bin"));
    }
    dirs
}

/// 已知安装位置。`~/.nvm/versions/node/*/bin` 展开成各个已装的 node 版本。
#[cfg(not(windows))]
pub fn well_known_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = home {
        dirs.push(h.join(".local/bin"));
        dirs.push(h.join(".bun/bin"));
        dirs.push(h.join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(h) = home {
        let nvm = h.join(".nvm/versions/node");
        if let Ok(rd) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
            // 新版本在前
            versions.sort();
            versions.reverse();
            dirs.extend(versions);
        }
        dirs.push(h.join(".volta/bin"));
        dirs.push(h.join(".npm-global/bin"));
    }
    dirs
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Windows 没有可执行位，按扩展名决定（[`executable_names`] 只拼可执行的扩展名）。
#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// 只留下 `PATH` 里的绝对项。空项（POSIX 里等于「当前目录」）、`.`、`./node_modules/.bin` 这类相对项会被 `which`
/// 按 `cwd` 解析——cwd 是项目目录时就可能执行克隆下来的仓库里自带的同名程序，所以一律丢掉。
fn absolute_path_entries(path_var: &str) -> Option<std::ffi::OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(path_var).filter(|p| p.is_absolute()).collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// `which -a name`：只用给定的 `PATH` 字符串里的**绝对**项；`cwd` 只是兜底，不参与解析（缺省用中性目录）。
pub fn which_all(name: &str, path_var: &str, cwd: &Path) -> Vec<PathBuf> {
    let Some(path) = absolute_path_entries(path_var) else { return Vec::new() };
    let cwd = if cwd.is_absolute() && cwd.is_dir() { cwd.to_path_buf() } else { super::neutral_cwd() };
    match which::which_in_all(name, Some(path), cwd) {
        Ok(it) => it.collect(),
        Err(_) => Vec::new(),
    }
}

/// 收集候选（不跑命令）：PATH 里的 + 已知目录里的，去重保序。
pub fn collect_candidates(name: &str, env: &HashMap<String, String>, cwd: &Path) -> Vec<(PathBuf, CommandOrigin)> {
    let mut out: Vec<(PathBuf, CommandOrigin)> = Vec::new();
    let push = |p: PathBuf, origin: CommandOrigin, out: &mut Vec<(PathBuf, CommandOrigin)>| {
        if !out.iter().any(|(q, _)| q == &p) {
            out.push((p, origin));
        }
    };
    if let Some(path_var) = env.get("PATH") {
        for p in which_all(name, path_var, cwd) {
            push(p, CommandOrigin::Path, &mut out);
        }
    }
    // 相对的 HOME 会让已知目录按 daemon 当前目录解析，不可信（`env_home` 只认绝对路径）
    let home = anycon_agent_api::env_home(env).map(PathBuf::from);
    for dir in well_known_dirs(home.as_deref()) {
        for file in executable_names(name) {
            let p = dir.join(file);
            if is_executable_file(&p) {
                push(p, CommandOrigin::WellKnown, &mut out);
            }
        }
    }
    out
}

pub async fn resolve_binary(opts: ResolveOptions<'_>) -> Resolution {
    let mut res = Resolution::default();
    let mut name = opts.default_name.to_string();
    let mut raw: Vec<(PathBuf, CommandOrigin)> = Vec::new();

    if let Some(cfg) = opts.config_command.map(str::trim).filter(|s| !s.is_empty()) {
        res.config_command = Some(cfg.to_string());
        match parse_command(cfg) {
            Ok(CommandSpec { program, args }) => {
                res.prefix_args = args;
                if program.contains('/') {
                    // 相对路径会按子进程的 cwd 解析，可能落到项目目录里；配置里的路径必须是绝对的
                    if !Path::new(&program).is_absolute() {
                        res.config_error = Some(format!("the command path must be absolute (got {program})"));
                        return res;
                    }
                    raw.push((PathBuf::from(&program), CommandOrigin::Config));
                    name = String::new();
                } else {
                    name = program;
                }
            }
            Err(e) => {
                res.config_error = Some(e.to_string());
                return res;
            }
        }
    }
    if !name.is_empty() {
        raw.extend(collect_candidates(&name, opts.env, opts.cwd));
    }

    let mut probe_args: Vec<String> = res.prefix_args.clone();
    probe_args.extend(opts.probe_args.iter().map(|s| s.to_string()));

    for (path, origin) in raw {
        let probe = if res.chosen.is_some() && !opts.probe_all {
            CandidateProbe::NotRun
        } else {
            let out = run_capture(&path, &probe_args, opts.env, opts.cwd, opts.per_candidate_timeout).await;
            if out.success() {
                let text = if out.stdout.trim().is_empty() { out.stderr.trim() } else { out.stdout.trim() };
                if text.is_empty() {
                    CandidateProbe::Failed { reason: "printed nothing".to_string() }
                } else {
                    CandidateProbe::Ok { output: text.to_string() }
                }
            } else {
                CandidateProbe::Failed { reason: out.failure_reason() }
            }
        };
        let ok = matches!(probe, CandidateProbe::Ok { .. });
        res.candidates.push(Candidate { path, origin, probe });
        if ok && res.chosen.is_none() {
            res.chosen = Some(res.candidates.len() - 1);
        }
    }
    res
}

/// 找要执行的程序 + 固定前置参数（spawn 时用，不再逐个 `--version`）。优先级：配置里的绝对路径 → 探测时选中的路径 →
/// 现找（`PATH` 里的绝对项 + 已知目录，用中性 cwd）。
///
/// 路径一律要求绝对（相对路径会按子进程的 cwd——也就是项目目录——解析，可能执行仓库里带的同名程序）。
/// 找不到 = `Err(NotInstalled)`，消息里带 `install_hint`。
pub fn resolve_program(
    config: &anycon_agent_api::AdapterConfig,
    probed_path: Option<&str>,
    env: &HashMap<String, String>,
    default_name: &str,
    install_hint: &str,
) -> anycon_agent_api::Result<(PathBuf, Vec<String>)> {
    use anycon_agent_api::AdapterError;
    let mut prefix = Vec::new();
    let mut name = default_name.to_string();
    if let Some(cmd) = config.command.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let spec = parse_command(cmd).map_err(|e| AdapterError::Spawn(format!("invalid command in config: {e}")))?;
        prefix = spec.args.clone();
        if spec.is_path() {
            if !Path::new(&spec.program).is_absolute() {
                return Err(AdapterError::Spawn(format!(
                    "invalid command in config: the path must be absolute (got {})",
                    spec.program
                )));
            }
            return Ok((PathBuf::from(spec.program), prefix));
        }
        name = spec.program;
    }
    if let Some(p) = probed_path.filter(|p| !p.is_empty()).map(PathBuf::from) {
        if p.is_absolute() && p.exists() {
            return Ok((p, prefix));
        }
    }
    match collect_candidates(&name, env, &super::neutral_cwd()).into_iter().next() {
        Some((p, _)) => Ok((p, prefix)),
        None => Err(AdapterError::NotInstalled(format!("{name} was not found; install it with: {install_hint}"))),
    }
}

// 假 CLI 是 `#!/bin/sh` 脚本，只在 unix 上跑
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn env_with_path(path: &str, home: &Path) -> HashMap<String, String> {
        HashMap::from([
            ("PATH".to_string(), path.to_string()),
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
        ])
    }

    #[tokio::test]
    async fn skips_broken_shim_and_picks_first_working() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad");
        let good = tmp.path().join("good");
        fs::create_dir_all(&bad).unwrap();
        fs::create_dir_all(&good).unwrap();
        script(&bad, "mytool", "echo broken shim >&2; exit 127");
        script(&good, "mytool", "echo 'mytool 1.2.3'");
        let path = format!("{}:{}", bad.display(), good.display());
        let env = env_with_path(&path, tmp.path());
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: None,
            default_name: "mytool",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert_eq!(res.candidates.len(), 2);
        assert!(matches!(res.candidates[0].probe, CandidateProbe::Failed { .. }));
        assert_eq!(res.chosen, Some(1));
        assert_eq!(res.path().unwrap(), good.join("mytool"));
        assert_eq!(res.version_output(), Some("mytool 1.2.3"));
        assert_eq!(res.candidates[1].origin, CommandOrigin::Path);
    }

    #[tokio::test]
    async fn finds_in_well_known_dir_when_not_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        fs::create_dir_all(&local_bin).unwrap();
        script(&local_bin, "mytool", "echo 'mytool 9.9.9'");
        let env = env_with_path("/usr/bin:/bin", tmp.path());
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: None,
            default_name: "mytool",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        let c = res.chosen().unwrap_or_else(|| panic!("no candidate chosen: {:?}", res.candidates));
        assert_eq!(c.origin, CommandOrigin::WellKnown);
        assert_eq!(c.path, local_bin.join("mytool"));
    }

    #[tokio::test]
    async fn nvm_version_dirs_are_expanded() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".nvm/versions/node/v24.1.0/bin");
        fs::create_dir_all(&bin).unwrap();
        script(&bin, "mytool", "echo 'mytool 1.0.0'");
        let env = env_with_path("/usr/bin:/bin", tmp.path());
        let dirs = well_known_dirs(Some(tmp.path()));
        assert!(dirs.contains(&bin));
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: None,
            default_name: "mytool",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert_eq!(res.path().unwrap(), bin.join("mytool"));
    }

    #[tokio::test]
    async fn config_absolute_path_with_args() {
        let tmp = tempfile::tempdir().unwrap();
        let p = script(tmp.path(), "wrapper", "echo \"wrapper $* ok\"");
        let env = env_with_path("/usr/bin:/bin", tmp.path());
        let cfg = format!("{} --profile x", p.display());
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: Some(&cfg),
            default_name: "claude",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert_eq!(res.chosen().unwrap().origin, CommandOrigin::Config);
        assert_eq!(res.prefix_args, vec!["--profile", "x"]);
        // 探活时带上了前置参数
        assert_eq!(res.version_output(), Some("wrapper --profile x --version ok"));
    }

    #[tokio::test]
    async fn nothing_found_and_all_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with_path(&tmp.path().to_string_lossy(), tmp.path());
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: None,
            default_name: "definitely-not-installed-xyz",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert!(res.candidates.is_empty());
        assert!(res.chosen.is_none());
        assert!(res.failure_summary().is_none());

        let bad = script(tmp.path(), "broken", "exit 1");
        let cfg = bad.display().to_string();
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: Some(&cfg),
            default_name: "x",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert!(res.chosen.is_none());
        let summary = res.failure_summary().unwrap();
        assert!(summary.contains("exited with code 1"), "{summary}");
    }

    #[tokio::test]
    async fn invalid_config_quoting_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with_path("/usr/bin", tmp.path());
        let res = resolve_binary(ResolveOptions {
            env: &env,
            config_command: Some("claude 'oops"),
            default_name: "claude",
            probe_args: &["--version"],
            per_candidate_timeout: Duration::from_secs(10),
            cwd: tmp.path(),
            probe_all: false,
        })
        .await;
        assert!(res.chosen.is_none());
        assert!(res.failure_summary().unwrap().contains("invalid command"));
    }
}
