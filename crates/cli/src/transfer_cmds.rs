//! `anycon export` / `anycon import`（docs/spec/cli.md § 3.6）：本机管理命令，走本机 socket，由 daemon 读写数据库
//! （格式和校验见 docs/spec/daemon.md § 20）。不经 hub，远程节点不支持。
//!
//! 请求是长请求：进度以 `local.progress` 通知回来，终端上画一行；Ctrl+C 断开连接，daemon 随之取消（导出删掉临时文件，
//! 导入回滚正在写的那个会话）。daemon 返回的文字（失败原因、警告、来源节点名）出自压缩包，打印前都经 `visible` 可见化。

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anycon_daemon::local_methods;
use anycon_daemon::transfer::{ExportParams, ExportResult, ImportParams, ImportResult, Progress, PROGRESS_METHOD};
use anycon_daemon::util::{fmt_utc, human_bytes, now_ms, parse_duration, parse_utc};
use anycon_protocol::{methods, NodeInfo};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use crate::cli::{ExportArgs, ImportArgs};
use crate::commands::{resolve_session, App};
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_ERROR, EXIT_OFFLINE};
use crate::output::visible;

/// 导出 / 导入可能要很久（几 GiB 的附件）：不按 `--timeout`，要停按 Ctrl+C。
const LONG: Duration = Duration::from_secs(24 * 3600);

/// 到本机 daemon 的连接。daemon 没在跑时说清楚为什么需要它。
async fn connect(app: &App) -> Result<Conn> {
    if app.node.is_some() {
        return Err(CliError::usage("export and import work on this machine's daemon only (--node is not supported)"));
    }
    app.local_conn().await.map_err(|e| {
        if e.exit == EXIT_OFFLINE && e.code == "offline" {
            CliError::offline(
                "the daemon is not running; start it with `anycon service start` or `anycon daemon` and run this \
                 again (export and import go through the daemon because it owns the database)",
            )
        } else {
            e
        }
    })
}

/// 文件名里放节点名：只留 ASCII 字母数字和 `._-`，其它换成 `-`，不以 `.` `-` 开头或结尾，没有连续的 `-`。
fn file_safe(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_matches(|c| c == '.' || c == '-').to_string();
    if out.is_empty() {
        "node".into()
    } else {
        out
    }
}

/// `--since`：时长（`7d`）、UTC 日期 / 时刻、Unix 毫秒。
fn parse_since(s: &str) -> Result<u64> {
    if let Some(ms) = parse_utc(s) {
        return Ok(ms);
    }
    if let Ok(ms) = s.parse::<u64>() {
        if ms > 10_000_000_000 {
            return Ok(ms);
        }
    }
    let d = parse_duration(s).map_err(|e| {
        CliError::usage(format!(
            "--since: {e} (use a duration like 7d, a date like 2026-10-01, a time like 2026-10-01T12:30:00Z, or Unix milliseconds)"
        ))
    })?;
    Ok(now_ms().saturating_sub(d.as_millis() as u64))
}

/// 输出文件的绝对路径：没给就是当前目录下的 `anycon-export-<节点名>-<日期>.zip`；给的是已有目录就把默认文件名放进去。
/// daemon 的工作目录和这里不同，所以必须在这里变成绝对路径。
async fn output_path(conn: &Conn, out: Option<PathBuf>) -> Result<PathBuf> {
    let path = match out {
        Some(p) if !p.is_dir() => p,
        dir => {
            let info: NodeInfo = conn.call(methods::NODE_INFO, json!({})).await?;
            let name = format!("anycon-export-{}-{}.zip", file_safe(&info.name), &fmt_utc(now_ms())[..10]);
            dir.unwrap_or_default().join(name)
        }
    };
    std::path::absolute(&path).map_err(|e| CliError::general(format!("cannot resolve {}: {e}", path.display())))
}

/// 发一个长请求，边等边把 `local.progress` 画在 stderr 的一行里（只在终端、非 `--json` 时画）。Ctrl+C = 取消。
async fn call_long<T: DeserializeOwned>(
    app: &App,
    conn: &mut Conn,
    method: &str,
    params: impl Serialize,
    verb: &str,
) -> Result<T> {
    let mut notes = conn.notifications();
    let mut sigint = crate::output::interrupt_signal();
    let draw = !app.json() && std::io::stderr().is_terminal();
    let call = conn.call_timeout(method, params, LONG);
    tokio::pin!(call);
    let mut notes_open = true;
    let result = loop {
        tokio::select! {
            r = &mut call => break r,
            n = notes.recv(), if notes_open => match n {
                Some(n) if n.method == PROGRESS_METHOD && draw => {
                    if let Ok(p) = serde_json::from_value::<Progress>(n.params) {
                        eprint!("\r\x1b[2K{verb} sessions {}/{} ({})", p.done, p.total, human_bytes(p.bytes));
                        let _ = std::io::stderr().flush();
                    }
                }
                Some(_) => {}
                None => notes_open = false,
            },
            _ = sigint.recv() => {
                if draw {
                    eprintln!();
                }
                return Err(CliError::interrupted());
            }
        }
    };
    if draw {
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
    let v = result?;
    serde_json::from_value(v).map_err(|e| CliError::general(format!("unexpected response to {method}: {e}")))
}

fn plural(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

pub async fn export(app: &App, a: ExportArgs) -> Result<u8> {
    let mut conn = connect(app).await?;
    let since_ms = a.since.as_deref().map(parse_since).transpose()?;
    let mut sessions = Vec::new();
    for key in &a.sessions {
        sessions.push(resolve_session(&conn, key).await?);
    }
    let out = output_path(&conn, a.out).await?;
    let params = ExportParams {
        out: out.to_string_lossy().into_owned(),
        project: a.project,
        since_ms,
        sessions,
        include_archived: a.all,
        overwrite: a.force,
    };
    let r: ExportResult = call_long(app, &mut conn, local_methods::EXPORT, params, "exporting").await?;
    if app.json() {
        app.out.json_line(&r);
        return Ok(0);
    }
    println!("{}", render_export(&r));
    if r.skipped_archived > 0 {
        eprintln!(
            "note: {} left out; add --all to include archived sessions",
            plural(r.skipped_archived, "archived session")
        );
    }
    for w in &r.warnings {
        eprintln!("warning: {}", visible(w));
    }
    Ok(0)
}

fn render_export(r: &ExportResult) -> String {
    format!(
        "exported {} ({}, {}, {}) to {}",
        plural(r.sessions, "session"),
        plural(r.events, "event"),
        plural(r.blobs, "attachment"),
        human_bytes(r.bytes),
        visible(&r.path)
    )
}

pub async fn import(app: &App, a: ImportArgs) -> Result<u8> {
    let mut conn = connect(app).await?;
    let file = std::path::absolute(&a.file)
        .map_err(|e| CliError::general(format!("cannot resolve {}: {e}", a.file.display())))?;
    if !file.is_file() {
        return Err(CliError::usage(format!("{} is not a file", file.display())));
    }
    let params = ImportParams { file: file.to_string_lossy().into_owned(), dry_run: a.dry_run };
    let r: ImportResult = call_long(app, &mut conn, local_methods::IMPORT, params, "importing").await?;
    if app.json() {
        app.out.json_line(&r);
    } else {
        print!("{}", render_import(&r));
        for w in &r.warnings {
            eprintln!("warning: {}", visible(w));
        }
    }
    if r.failed.is_empty() {
        Ok(0)
    } else {
        Err(CliError::new(
            EXIT_ERROR,
            "import_failed",
            format!("{} could not be imported; nothing of them was written", plural(r.failed.len() as u64, "session")),
        ))
    }
}

fn render_import(r: &ImportResult) -> String {
    let mut out = String::new();
    let (n_ok, n_skip, n_fail) = (r.imported.len() as u64, r.skipped.len() as u64, r.failed.len() as u64);
    let detail = format!("{}, {}", plural(r.events, "event"), plural(r.blobs, "attachment"));
    if r.dry_run {
        out.push_str(&format!(
            "dry run, nothing was written: {} can be imported ({detail}), {n_skip} would be skipped, {n_fail} failed the checks\n",
            plural(n_ok, "session"),
        ));
    } else {
        out.push_str(&format!("imported {} ({detail}), skipped {n_skip}, failed {n_fail}\n", plural(n_ok, "session"),));
    }
    out.push_str(&format!(
        "  from an export of {} made {} UTC by anycon {}\n",
        visible(&r.source_node),
        fmt_utc(r.exported_at_ms),
        visible(&r.source_version)
    ));
    for s in &r.skipped {
        let why =
            if s.reason == "already_exists" { "already exists, left as it is".to_string() } else { visible(&s.reason) };
        out.push_str(&format!("  skipped {}: {why}\n", visible(&s.id)));
    }
    for f in &r.failed {
        out.push_str(&format!("  failed  {}: {}\n", visible(&f.id), visible(&f.error)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_daemon::transfer::{ImportFailed, ImportSkipped};

    #[test]
    fn node_names_become_safe_file_names() {
        assert_eq!(file_safe("mac-mini"), "mac-mini");
        assert_eq!(file_safe("Haiwei's MacBook Pro.local"), "Haiwei-s-MacBook-Pro.local");
        assert_eq!(file_safe("../../etc/passwd"), "etc-passwd");
        assert_eq!(file_safe("a/b\\c:d"), "a-b-c-d");
        assert_eq!(file_safe("日本語"), "node");
        assert_eq!(file_safe(""), "node");
        assert_eq!(file_safe("--rf"), "rf");
        assert_eq!(file_safe("x\u{1b}[31m"), "x-31m");
    }

    #[test]
    fn since_takes_dates_times_durations_and_milliseconds() {
        assert_eq!(parse_since("2026-10-01").unwrap(), 1_790_812_800_000);
        assert_eq!(parse_since("2026-10-01T12:30:00Z").unwrap(), 1_790_812_800_000 + 45_000_000);
        assert_eq!(parse_since("1791720000000").unwrap(), 1_791_720_000_000);
        let d = parse_since("7d").unwrap();
        let want = now_ms() - 7 * 86_400_000;
        assert!(d.abs_diff(want) < 5_000, "{d} vs {want}");
        for bad in ["", "yesterday", "2026-13-01", "2026-10-01 12:00", "7 lightyears"] {
            let e = parse_since(bad).unwrap_err();
            assert_eq!(e.exit, crate::error::EXIT_USAGE, "{bad}");
            assert!(e.message.starts_with("--since:"), "{}", e.message);
        }
    }

    #[test]
    fn the_export_line_counts_things() {
        let r = ExportResult {
            path: "/tmp/x.zip".into(),
            bytes: 2048,
            sessions: 1,
            events: 12,
            blobs: 0,
            skipped_archived: 0,
            warnings: vec![],
        };
        assert_eq!(render_export(&r), "exported 1 session (12 events, 0 attachments, 2.0 KiB) to /tmp/x.zip");
    }

    #[test]
    fn the_import_report_lists_skips_and_failures_and_cannot_be_hijacked_by_the_archive() {
        let r = ImportResult {
            dry_run: false,
            total: 3,
            imported: vec!["sess_a".into()],
            skipped: vec![ImportSkipped { id: "sess_b".into(), reason: "already_exists".into() }],
            failed: vec![ImportFailed { id: "sess_c".into(), error: "bad \u{1b}[2Kvalue\rfake".into() }],
            events: 5,
            blobs: 1,
            source_node: "mini\u{1b}]0;x\u{7}".into(),
            source_version: "0.2.0".into(),
            exported_at_ms: 1_791_720_000_000,
            warnings: vec![],
        };
        let t = render_import(&r);
        assert!(t.starts_with("imported 1 session (5 events, 1 attachment), skipped 1, failed 1\n"), "{t}");
        assert!(t.contains("  skipped sess_b: already exists, left as it is\n"), "{t}");
        assert!(
            t.contains("  from an export of mini\\x1b]0;x\\x07 made 2026-10-11 12:00:00 UTC by anycon 0.2.0\n"),
            "{t}"
        );
        assert!(t.contains("  failed  sess_c: bad \\x1b[2Kvalue\\rfake\n"), "{t}");
        assert!(!t.contains('\x1b') && !t.contains('\r') && !t.contains('\x07'), "no raw control characters: {t:?}");
        let dry = render_import(&ImportResult { dry_run: true, failed: vec![], ..r });
        assert!(dry.starts_with("dry run, nothing was written: 1 session can be imported (5 events, 1 attachment), 1 would be skipped, 0 failed the checks\n"), "{dry}");
    }
}
