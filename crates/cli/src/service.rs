//! `anycon service`：注册为用户级 launchd（macOS）/ systemd user unit（Linux）。其它平台报不支持。
//!
//! 单元文件内容由纯函数生成（有单元测试）；真正调用 `launchctl` / `systemctl` 的部分带超时。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_daemon::doctor::{service_unit_path, SERVICE_LABEL, SYSTEMD_UNIT};

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

fn uid() -> u32 {
    // SAFETY: getuid 无副作用。
    unsafe { libc::getuid() }
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

pub async fn uninstall() -> Result<String> {
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

pub async fn control(action: &str) -> Result<String> {
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
    let path = unit_path()?;
    let mut s = String::new();
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
