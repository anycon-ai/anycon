//! `anycon service`：注册为用户级 launchd（macOS）/ systemd user unit（Linux）/ 计划任务（Windows）。其它平台报不支持。
//!
//! 单元文件内容由纯函数生成（有单元测试）；真正调用 `launchctl` / `systemctl` / `schtasks` 的部分带超时。
//!
//! **Windows**：当前用户的计划任务 `anycon`（登录时启动、失败后每分钟重试、不限运行时长、用电池时照常运行），不需要管理员权限。
//! 任务拉起的是同目录下的 `anyconw.exe`（没有控制台窗口的小启动器，见 `crates/windows-launcher`），它再以隐藏的控制台运行
//! `anycon.exe daemon`；找不到 `anyconw.exe` 时直接运行 `anycon.exe`，会有一个控制台窗口（关掉它 daemon 就停了）。计划任务不能设
//! 环境变量，非默认的数据目录用 `daemon --home` 传。`stop` 先发停止事件让 daemon 优雅退出（`anycon_daemon::daemon::stop_event`），
//! 等不到再 `schtasks /End`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_daemon::doctor::{service_unit_path, SERVICE_LABEL, SYSTEMD_UNIT, WINDOWS_TASK};

use crate::error::{CliError, Result};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// launchd plist。`home` 非默认位置时通过 `ANYCON_HOME` 传给 daemon。
pub fn launchd_plist(exe: &Path, home: &Path, logs: &Path, pass_home: bool) -> String {
    let mut env = String::new();
    if pass_home {
        env = format!("    <key>EnvironmentVariables</key>\n    <dict>\n        <key>ANYCON_HOME</key>\n        <string>{}</string>\n    </dict>\n", xml_escape(&home.to_string_lossy()));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
{env}</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        exe = xml_escape(&exe.to_string_lossy()),
        out = xml_escape(&logs.join("launchd.out.log").to_string_lossy()),
        err = xml_escape(&logs.join("launchd.err.log").to_string_lossy()),
        env = env,
    )
}

fn systemd_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

pub fn systemd_unit(exe: &Path, home: &Path, pass_home: bool) -> String {
    let env = if pass_home {
        format!("Environment=ANYCON_HOME={}\n", systemd_quote(&home.to_string_lossy()))
    } else {
        String::new()
    };
    format!(
        "[Unit]\nDescription=anycon daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={exe} daemon --foreground\nRestart=always\nRestartSec=3\n{env}\n[Install]\nWantedBy=default.target\n",
        exe = systemd_quote(&exe.to_string_lossy()),
        env = env,
    )
}

async fn run(prog: &str, args: &[&str]) -> Result<(bool, String)> {
    let child =
        tokio::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(20), child).await {
        Err(_) => Err(CliError::general(format!("`{prog} {}` timed out", args.join(" ")))),
        Ok(Err(e)) => Err(CliError::general(format!("cannot run {prog}: {e}"))),
        Ok(Ok(o)) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            Ok((o.status.success(), text.trim().to_string()))
        }
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid 无副作用。
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn unit_path() -> Result<PathBuf> {
    service_unit_path().ok_or_else(|| {
        CliError::general("service management is only supported on macOS (launchd) and Linux (systemd --user)")
    })
}

fn pass_home(home: &Path) -> bool {
    std::env::var_os("ANYCON_HOME").is_some()
        || home != anycon_daemon::config::Paths::from_env().home
        || home.file_name().is_none_or(|n| n != ".anycon")
}

/// 规范化路径落在 `<prefix>/Cellar/<formula>/<版本>/bin/<文件>` 时，返回 formula 的固定入口
/// `<prefix>/opt/<formula>/bin/<文件>`（只按路径推算，不检查是否存在）。
fn homebrew_opt_path(canonical: &Path) -> Option<PathBuf> {
    let file = canonical.file_name()?;
    let bin = canonical.parent()?;
    let keg = bin.parent()?;
    let rack = keg.parent()?;
    let cellar = rack.parent()?;
    if bin.file_name()? != "bin" || cellar.file_name()? != "Cellar" {
        return None;
    }
    Some(cellar.parent()?.join("opt").join(rack.file_name()?).join("bin").join(file))
}

/// 写进服务单元的二进制路径。
///
/// 一般用规范化后的实际路径（安装脚本、`cargo install` 放的是普通文件，升级时原地替换）。
/// Homebrew 的实际文件在带版本号的 Cellar 目录里，`brew upgrade` 清理旧版本后路径就失效了，
/// 所以改用 formula 的 opt 入口（始终指向当前版本）；opt 入口不存在或不指向同一个文件时，
/// 退回启动时用的符号链接路径（如 `<prefix>/bin/anycon`）。
fn service_exe_path(exe: &Path) -> PathBuf {
    let canonical = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let Some(opt) = homebrew_opt_path(&canonical) else {
        return canonical;
    };
    if opt.canonicalize().is_ok_and(|p| p == canonical) {
        return opt;
    }
    // Linux 的 current_exe 已经是规范化路径，这一步只在 macOS 从符号链接启动时有用
    if exe.is_absolute() && exe != canonical {
        return exe.to_path_buf();
    }
    canonical
}

pub async fn install(home: &Path) -> Result<String> {
    if cfg!(windows) {
        return windows_install(home).await;
    }
    let path = unit_path()?;
    let exe = service_exe_path(&std::env::current_exe()?);
    let logs = home.join("logs");
    std::fs::create_dir_all(&logs)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if cfg!(target_os = "macos") {
        std::fs::write(&path, launchd_plist(&exe, home, &logs, pass_home(home)))?;
        let target = format!("gui/{}", uid());
        // 已加载过则先卸载，保证新的 plist 生效
        let _ = run("launchctl", &["bootout", &format!("{target}/{SERVICE_LABEL}")]).await;
        let (ok, out) = run("launchctl", &["bootstrap", &target, &path.to_string_lossy()]).await?;
        if !ok {
            return Err(CliError::general(format!("launchctl bootstrap failed: {out}")));
        }
        Ok(format!("installed {} and started the service ({SERVICE_LABEL})", path.display()))
    } else {
        std::fs::write(&path, systemd_unit(&exe, home, pass_home(home)))?;
        let _ = run("systemctl", &["--user", "daemon-reload"]).await?;
        let (ok, out) = run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT]).await?;
        if !ok {
            return Err(CliError::general(format!("systemctl enable failed: {out}")));
        }
        Ok(format!("installed {} and started the service ({SYSTEMD_UNIT}). To keep it running after logout: `loginctl enable-linger $USER`", path.display()))
    }
}

pub async fn uninstall(home: &Path) -> Result<String> {
    if cfg!(windows) {
        return windows_uninstall(home).await;
    }
    let path = unit_path()?;
    if cfg!(target_os = "macos") {
        let _ = run("launchctl", &["bootout", &format!("gui/{}/{SERVICE_LABEL}", uid())]).await;
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]).await;
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    if !cfg!(target_os = "macos") {
        let _ = run("systemctl", &["--user", "daemon-reload"]).await;
    }
    Ok(format!("removed {}", path.display()))
}

pub async fn control(action: &str, home: &Path) -> Result<String> {
    if cfg!(windows) {
        return windows_control(action, home).await;
    }
    let path = unit_path()?;
    if !path.exists() {
        return Err(CliError::general(format!(
            "the service is not installed ({} not found); run `anycon service install`",
            path.display()
        )));
    }
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}", uid());
        let svc = format!("{target}/{SERVICE_LABEL}");
        let loaded = run("launchctl", &["print", &svc]).await?.0;
        match action {
            "start" => {
                if !loaded {
                    let (ok, out) = run("launchctl", &["bootstrap", &target, &path.to_string_lossy()]).await?;
                    if !ok {
                        return Err(CliError::general(format!("launchctl bootstrap failed: {out}")));
                    }
                } else {
                    let _ = run("launchctl", &["kickstart", &svc]).await?;
                }
            }
            "stop" => {
                if loaded {
                    let (ok, out) = run("launchctl", &["bootout", &svc]).await?;
                    if !ok {
                        return Err(CliError::general(format!("launchctl bootout failed: {out}")));
                    }
                }
            }
            "restart" => {
                if loaded {
                    let (ok, out) = run("launchctl", &["kickstart", "-k", &svc]).await?;
                    if !ok {
                        return Err(CliError::general(format!("launchctl kickstart failed: {out}")));
                    }
                } else {
                    let (ok, out) = run("launchctl", &["bootstrap", &target, &path.to_string_lossy()]).await?;
                    if !ok {
                        return Err(CliError::general(format!("launchctl bootstrap failed: {out}")));
                    }
                }
            }
            _ => unreachable!(),
        }
    } else {
        let (ok, out) = run("systemctl", &["--user", action, SYSTEMD_UNIT]).await?;
        if !ok {
            return Err(CliError::general(format!("systemctl {action} failed: {out}")));
        }
    }
    Ok(format!("service {action}: ok"))
}

/// 进程状态 + 最近日志。
pub async fn status(home: &Path) -> Result<String> {
    let mut s = String::new();
    if cfg!(windows) {
        s = windows_status(home).await?;
    } else {
        unix_status(&mut s).await?;
    }
    if let Some(log) = anycon_daemon::logging::latest_log(&home.join("logs")) {
        if let Ok(text) = std::fs::read_to_string(&log) {
            let lines: Vec<&str> = text.lines().collect();
            s.push_str(&format!("-- last log lines ({}) --\n", log.display()));
            for l in &lines[lines.len().saturating_sub(10)..] {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    Ok(s)
}

async fn unix_status(s: &mut String) -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        s.push_str(&format!("not installed ({} not found); run `anycon service install`\n", path.display()));
    } else if cfg!(target_os = "macos") {
        let (ok, out) = run("launchctl", &["print", &format!("gui/{}/{SERVICE_LABEL}", uid())]).await?;
        if !ok {
            s.push_str("installed but not loaded (stopped); run `anycon service start`\n");
        } else {
            let field = |k: &str| {
                out.lines()
                    .find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().trim_start_matches('=').trim().to_string()))
            };
            s.push_str(&format!("state: {}\n", field("state ").unwrap_or_else(|| "loaded".into())));
            if let Some(pid) = field("pid ") {
                s.push_str(&format!("pid: {pid}\n"));
            }
        }
    } else {
        let (_, out) = run("systemctl", &["--user", "is-active", SYSTEMD_UNIT]).await?;
        s.push_str(&format!("state: {out}\n"));
    }
    Ok(())
}

// ---------------------------------------------------------------- Windows：计划任务

/// Windows 命令行的一个参数（`CommandLineToArgvW` 的规则）：没有空白和引号就原样，否则加引号，
/// 引号前和结尾的反斜杠加倍。
pub fn windows_quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || c == '"') {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// 计划任务的 XML（任务计划程序 1.2 架构）。`user` 是当前用户的 SID：登录触发器只对这个用户生效，所以不需要管理员权限。
/// 不限运行时长（默认是 72 小时）、用电池时照常运行、失败后每分钟重试；以登录用户的交互令牌运行（agent 要用到用户的凭据）。
pub fn windows_task_xml(program: &Path, args: &[String], workdir: &Path, user: &str) -> String {
    let args = args.iter().map(|a| windows_quote_arg(a)).collect::<Vec<_>>().join(" ");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>anycon daemon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{program}</Command>
      <Arguments>{args}</Arguments>
      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        program = xml_escape(&program.to_string_lossy()),
        args = xml_escape(&args),
        workdir = xml_escape(&workdir.to_string_lossy()),
    )
}

/// `schtasks /XML` 要的文件：UTF-16LE，带 BOM。
fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// 任务要拉起的程序和参数：有同目录的 `anyconw.exe` 就用它（没有控制台窗口），否则直接用 `anycon.exe`。
fn windows_task_command(exe: &Path, home: &Path) -> (PathBuf, Vec<String>, bool) {
    let launcher = exe.with_file_name("anyconw.exe");
    let mut args = vec!["daemon".to_string(), "--foreground".to_string()];
    if pass_home(home) {
        let abs = std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf());
        args.push("--home".into());
        args.push(abs.to_string_lossy().into_owned());
    }
    if launcher.is_file() {
        (launcher, args, true)
    } else {
        (exe.to_path_buf(), args, false)
    }
}

async fn windows_task_exists() -> Result<bool> {
    Ok(run("schtasks", &["/Query", "/TN", WINDOWS_TASK]).await?.0)
}

/// daemon 的本机通道连得上。
async fn daemon_running(home: &Path) -> bool {
    anycon_daemon::local::connect(&anycon_daemon::config::Paths::new(home).sock()).await.is_ok()
}

/// 等 daemon 退出（本机通道连不上），最多 `secs` 秒。
async fn wait_daemon_gone(home: &Path, secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if !daemon_running(home).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    !daemon_running(home).await
}

#[cfg(windows)]
fn windows_user_sid() -> Result<String> {
    anycon_daemon::sys::win::current_user_sid()
        .map_err(|e| CliError::general(format!("cannot determine the current user: {e}")))
}

#[cfg(not(windows))]
fn windows_user_sid() -> Result<String> {
    Err(CliError::general("not on Windows"))
}

#[cfg(windows)]
fn request_stop(home: &Path) -> bool {
    anycon_daemon::daemon::stop_event::request(&anycon_daemon::config::Paths::new(home)).unwrap_or(false)
}

#[cfg(not(windows))]
fn request_stop(_home: &Path) -> bool {
    false
}

async fn windows_install(home: &Path) -> Result<String> {
    let exe = anycon_daemon::sys::canonicalize(std::env::current_exe()?)?;
    let (program, args, windowless) = windows_task_command(&exe, home);
    std::fs::create_dir_all(home.join("logs"))?;
    let xml = windows_task_xml(&program, &args, home, &windows_user_sid()?);
    let file = home.join("anycon-task.xml");
    std::fs::write(&file, utf16le_with_bom(&xml))?;
    let created = run("schtasks", &["/Create", "/TN", WINDOWS_TASK, "/XML", &file.to_string_lossy(), "/F"]).await;
    let _ = std::fs::remove_file(&file);
    let (ok, out) = created?;
    if !ok {
        return Err(CliError::general(format!("schtasks /Create failed: {out}")));
    }
    if !daemon_running(home).await {
        let (ok, out) = run("schtasks", &["/Run", "/TN", WINDOWS_TASK]).await?;
        if !ok {
            return Err(CliError::general(format!("schtasks /Run failed: {out}")));
        }
    }
    let mut msg = format!("installed the scheduled task {WINDOWS_TASK} (runs at logon) and started it");
    if !windowless {
        msg.push_str(&format!(
            "\nnote: {} was not found next to anycon.exe, so the daemon runs in a visible console window; closing it stops the daemon",
            program.with_file_name("anyconw.exe").display()
        ));
    }
    Ok(msg)
}

/// 优雅停止：先发停止事件，等 15 秒；还在就 `schtasks /End`（强制结束，agent 随 job 一起被清掉）。
async fn windows_stop(home: &Path) -> Result<()> {
    if request_stop(home) && wait_daemon_gone(home, 15).await {
        let _ = run("schtasks", &["/End", "/TN", WINDOWS_TASK]).await;
        return Ok(());
    }
    let (ok, out) = run("schtasks", &["/End", "/TN", WINDOWS_TASK]).await?;
    if !ok && daemon_running(home).await {
        return Err(CliError::general(format!("schtasks /End failed: {out}")));
    }
    Ok(())
}

async fn windows_uninstall(home: &Path) -> Result<String> {
    if !windows_task_exists().await? {
        return Ok(format!("the scheduled task {WINDOWS_TASK} is not installed"));
    }
    windows_stop(home).await?;
    let (ok, out) = run("schtasks", &["/Delete", "/TN", WINDOWS_TASK, "/F"]).await?;
    if !ok {
        return Err(CliError::general(format!("schtasks /Delete failed: {out}")));
    }
    Ok(format!("removed the scheduled task {WINDOWS_TASK}"))
}

async fn windows_control(action: &str, home: &Path) -> Result<String> {
    if !windows_task_exists().await? {
        return Err(CliError::general(format!(
            "the service is not installed (no scheduled task {WINDOWS_TASK}); run `anycon service install`"
        )));
    }
    match action {
        "start" => {
            if !daemon_running(home).await {
                let (ok, out) = run("schtasks", &["/Run", "/TN", WINDOWS_TASK]).await?;
                if !ok {
                    return Err(CliError::general(format!("schtasks /Run failed: {out}")));
                }
            }
        }
        "stop" => windows_stop(home).await?,
        "restart" => {
            windows_stop(home).await?;
            let (ok, out) = run("schtasks", &["/Run", "/TN", WINDOWS_TASK]).await?;
            if !ok {
                return Err(CliError::general(format!("schtasks /Run failed: {out}")));
            }
        }
        _ => unreachable!(),
    }
    Ok(format!("service {action}: ok"))
}

async fn windows_status(home: &Path) -> Result<String> {
    if !windows_task_exists().await? {
        return Ok(format!("not installed (no scheduled task {WINDOWS_TASK}); run `anycon service install`\n"));
    }
    let state = if daemon_running(home).await { "running" } else { "stopped; run `anycon service start`" };
    Ok(format!("scheduled task: {WINDOWS_TASK}\nstate: {state}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_well_formed_and_escapes_paths() {
        let p = launchd_plist(
            Path::new("/opt/any&con/bin/anycon"),
            Path::new("/tmp/h<ome>"),
            Path::new("/tmp/h<ome>/logs"),
            true,
        );
        assert!(p.contains("<string>ai.anycon.daemon</string>"));
        assert!(p.contains("<string>/opt/any&amp;con/bin/anycon</string>"));
        assert!(p.contains("<key>ANYCON_HOME</key>"));
        assert!(p.contains("/tmp/h&lt;ome&gt;"));
        assert!(p.contains("<string>--foreground</string>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        let no_home = launchd_plist(
            Path::new("/usr/local/bin/anycon"),
            Path::new("/Users/a/.anycon"),
            Path::new("/Users/a/.anycon/logs"),
            false,
        );
        assert!(!no_home.contains("ANYCON_HOME"));
        assert!(no_home.matches("<dict>").count() == no_home.matches("</dict>").count());
    }

    #[test]
    fn systemd_unit_quotes_exec_path() {
        let u = systemd_unit(Path::new("/opt/my apps/anycon"), Path::new("/home/a/.anycon"), true);
        assert!(u.contains("ExecStart=\"/opt/my apps/anycon\" daemon --foreground"));
        assert!(u.contains("Environment=ANYCON_HOME=/home/a/.anycon"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(!systemd_unit(Path::new("/usr/bin/anycon"), Path::new("/x"), false).contains("Environment="));
    }

    #[test]
    fn windows_args_are_quoted_like_command_line_to_argv() {
        assert_eq!(windows_quote_arg("daemon"), "daemon");
        assert_eq!(windows_quote_arg(r"C:\Users\me\.anycon"), r"C:\Users\me\.anycon");
        assert_eq!(windows_quote_arg(r"C:\Users\John Doe\.anycon"), r#""C:\Users\John Doe\.anycon""#);
        // 结尾的反斜杠在收尾引号前加倍；引号前的反斜杠加倍再转义引号
        assert_eq!(windows_quote_arg(r"C:\My Dir\"), r#""C:\My Dir\\""#);
        assert_eq!(windows_quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(windows_quote_arg(""), r#""""#);
    }

    #[test]
    fn windows_task_runs_at_logon_without_time_limit_and_escapes() {
        let x = windows_task_xml(
            Path::new(r"C:\Program Files\anycon & co\anyconw.exe"),
            &["daemon".into(), "--foreground".into(), "--home".into(), r"C:\Users\John Doe\.anycon".into()],
            Path::new(r"C:\Users\John Doe\.anycon"),
            "S-1-5-21-1-2-3-1001",
        );
        assert!(x.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        assert!(x.contains("<LogonTrigger>") && x.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"));
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(x.contains(r"<Command>C:\Program Files\anycon &amp; co\anyconw.exe</Command>"));
        assert!(
            x.contains(r#"<Arguments>daemon --foreground --home &quot;C:\Users\John Doe\.anycon&quot;</Arguments>"#)
        );
        let bytes = utf16le_with_bom(&x);
        assert_eq!(&bytes[..4], &[0xFF, 0xFE, b'<', 0]);
    }

    #[test]
    fn windows_task_prefers_the_windowless_launcher() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("anycon.exe");
        std::fs::write(&exe, "").unwrap();
        let home = anycon_daemon::config::Paths::from_env().home;
        let (program, args, windowless) = windows_task_command(&exe, &home);
        assert_eq!((program.as_path(), windowless), (exe.as_path(), false));
        assert_eq!(&args[..2], ["daemon", "--foreground"]);
        std::fs::write(tmp.path().join("anyconw.exe"), "").unwrap();
        let (program, _, windowless) = windows_task_command(&exe, &home);
        assert_eq!((program, windowless), (tmp.path().join("anyconw.exe"), true));
        let (_, args, _) = windows_task_command(&exe, &tmp.path().join("custom"));
        assert_eq!(&args[2..3], ["--home"]);
    }

    #[test]
    fn homebrew_opt_path_only_matches_cellar_layout() {
        assert_eq!(
            homebrew_opt_path(Path::new("/opt/homebrew/Cellar/anycon/0.1.0_1/bin/anycon")),
            Some(PathBuf::from("/opt/homebrew/opt/anycon/bin/anycon"))
        );
        assert_eq!(homebrew_opt_path(Path::new("/home/a/.local/bin/anycon")), None);
        assert_eq!(homebrew_opt_path(Path::new("/opt/homebrew/Cellar/anycon/0.1.0/libexec/anycon")), None);
        assert_eq!(homebrew_opt_path(Path::new("/bin/anycon")), None);
    }

    #[cfg(unix)]
    #[test]
    fn service_exe_path_prefers_stable_homebrew_entry() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        // macOS 的临时目录在 /var 下，而 /var 是指向 /private/var 的符号链接
        let prefix = tmp.path().canonicalize().unwrap();
        let keg_bin = prefix.join("Cellar/anycon/0.1.0/bin");
        std::fs::create_dir_all(&keg_bin).unwrap();
        let real = keg_bin.join("anycon");
        std::fs::write(&real, "").unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let bin_link = prefix.join("bin/anycon");
        symlink("../Cellar/anycon/0.1.0/bin/anycon", &bin_link).unwrap();

        // 没有 opt 入口：退回符号链接路径；从规范路径启动（Linux）则只能用规范路径
        assert_eq!(service_exe_path(&bin_link), bin_link);
        assert_eq!(service_exe_path(&real), real);

        // opt 入口指向同一版本：两种启动方式都用 opt 路径
        std::fs::create_dir_all(prefix.join("opt")).unwrap();
        let opt_link = prefix.join("opt/anycon");
        symlink("../Cellar/anycon/0.1.0", &opt_link).unwrap();
        let opt_exe = prefix.join("opt/anycon/bin/anycon");
        assert_eq!(service_exe_path(&bin_link), opt_exe);
        assert_eq!(service_exe_path(&real), opt_exe);

        // opt 入口指向别的版本（当前进程不是已链接的版本）：不用 opt
        let other_bin = prefix.join("Cellar/anycon/0.2.0/bin");
        std::fs::create_dir_all(&other_bin).unwrap();
        std::fs::write(other_bin.join("anycon"), "").unwrap();
        std::fs::remove_file(&opt_link).unwrap();
        symlink("../Cellar/anycon/0.2.0", &opt_link).unwrap();
        assert_eq!(service_exe_path(&bin_link), bin_link);

        // 不在 Cellar 里：保持规范化后的实际路径
        let plain = prefix.join("local/bin");
        std::fs::create_dir_all(&plain).unwrap();
        let plain_exe = plain.join("anycon");
        std::fs::write(&plain_exe, "").unwrap();
        let plain_link = prefix.join("anycon-link");
        symlink(&plain_exe, &plain_link).unwrap();
        assert_eq!(service_exe_path(&plain_exe), plain_exe);
        assert_eq!(service_exe_path(&plain_link), plain_exe);
    }
}
