//! `node.logs`：daemon 日志文件的尾部（docs/spec/daemon.md § 17）。
//!
//! 只读现有的 `logs/daemon.log.<date>`，不另外收集、不另外存。daemon 自己的日志约定是不写会话内容（[`crate::logging`]），
//! 这里在返回之前再做一道过滤，因为有两类字段的文字不由 anycon 控制：
//!
//! - **`anycon_adapters` 的日志整行不返回**：adapter 直接面对 agent 进程的输出，它的错误和解析失败的字段里可能引用 agent 说过的话；
//! - **`anycon_daemon::sessions`（含 `sessions::wt`）的 `error=` 字段不返回**：这个模块会把 adapter 的错误（`Protocol` / `Exited`
//!   里可能带 agent 的 stderr 片段）和 git / gh 的输出原样写进 `error` 字段，行里 `error=` 之后的文字换成 `[withheld]`。
//!
//! 本机的 `anycon logs` 读的是同一个文件、不做这层过滤。日志是 `tracing` 默认的文本格式：
//! `<时间戳> <级别> <target>: <消息> <字段…>`，一条记录可以占多行（后面的行没有时间戳，跟随它所属记录的过滤结果）。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anycon_protocol::{NodeLogLevel, NodeLogsParams, NodeLogsResult};

/// `lines` 缺省值。
pub const DEFAULT_LINES: usize = 200;
/// `lines` 上限。
pub const MAX_LINES: usize = 1000;
/// 每行最多多少字节（含截断标记）。
pub const MAX_LINE_BYTES: usize = 2048;
/// 一次调用最多扫描多少字节的日志（从最新的文件尾部往回数，跨文件合计）。级别过滤很严时靠它封顶。
pub const SCAN_BUDGET: u64 = 4 * 1024 * 1024;

/// 整行不返回的 target 前缀（包括它的子模块）。
const WITHHELD_TARGETS: &[&str] = &["anycon_adapters"];
/// `error=` 字段不返回的 target 前缀（包括它的子模块）。
const ERROR_MASKED_TARGETS: &[&str] = &["anycon_daemon::sessions"];
const WITHHELD: &str = "[withheld]";

/// 读最近的日志行：最多 `params.lines`（缺省 200，上限 1000）行，最低级别 `params.level`（缺省不过滤）。
pub fn read(dir: &Path, params: &NodeLogsParams) -> NodeLogsResult {
    let wanted = params.lines.map_or(DEFAULT_LINES, |n| (n as usize).clamp(1, MAX_LINES));
    tail(dir, wanted, params.level, SCAN_BUDGET)
}

fn target_in(target: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| target == *p || target.strip_prefix(p).is_some_and(|rest| rest.starts_with("::")))
}

/// 日志目录里的 `daemon.log*`，最新的在前（文件名带 ISO 日期，按名字排序就是按时间排序）。
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log") && e.path().is_file())
        .map(|e| e.path())
        .collect();
    files.sort();
    files.reverse();
    files
}

/// 读一个文件的最后至多 `max` 字节。返回（内容，是不是从文件开头读起的）。
/// 没从开头读起时，第一行多半是半行，丢掉。
fn read_tail(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start).min(max) as usize);
    f.take(len - start).read_to_end(&mut buf).ok()?;
    if start > 0 {
        let cut = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
        buf.drain(..cut);
    }
    Some((buf, start == 0))
}

struct Header<'a> {
    level: NodeLogLevel,
    target: &'a str,
}

/// 一条记录的第一行：`<时间戳> <级别> <target>: …`。认不出来的行（续行、被截断的开头）返回 `None`。
fn parse_header(line: &str) -> Option<Header<'_>> {
    let mut parts = line.split_ascii_whitespace();
    let ts = parts.next()?;
    if !ts.as_bytes().first()?.is_ascii_digit() || !ts.contains('T') {
        return None;
    }
    let level = match parts.next()? {
        "TRACE" => NodeLogLevel::Trace,
        "DEBUG" => NodeLogLevel::Debug,
        "INFO" => NodeLogLevel::Info,
        "WARN" => NodeLogLevel::Warn,
        "ERROR" => NodeLogLevel::Error,
        _ => return None,
    };
    let target = parts.next()?.strip_suffix(':')?;
    Some(Header { level, target })
}

/// 一条记录怎么处理。
#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Drop,
    Keep,
    /// 保留第一行但遮掉 `error=` 之后的文字；它的续行全部不要（续行是 `error` 字段文字的一部分）。
    MaskError,
}

fn verdict(h: &Header<'_>, min_level: Option<NodeLogLevel>) -> Verdict {
    if min_level.is_some_and(|min| h.level < min) || target_in(h.target, WITHHELD_TARGETS) {
        Verdict::Drop
    } else if target_in(h.target, ERROR_MASKED_TARGETS) {
        Verdict::MaskError
    } else {
        Verdict::Keep
    }
}

/// 把 `error=` 之后的文字换成 `[withheld]`（没有 `error=` 的行原样）。
fn mask_error(line: &str) -> String {
    match line.find(" error=") {
        Some(i) => format!("{} error={WITHHELD}", &line[..i]),
        None => line.to_string(),
    }
}

/// 截到 [`MAX_LINE_BYTES`]（含标记），不切断多字节字符。
fn clip(line: String) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line;
    }
    const MARK: &str = "…";
    let mut cut = MAX_LINE_BYTES - MARK.len();
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{MARK}", &line[..cut])
}

/// 一段日志文本 → 过滤、遮盖、截断之后的行（旧的在前）。开头认不出所属记录的续行丢掉（不知道它来自哪个 target）。
fn filter(bytes: &[u8], min_level: Option<NodeLogLevel>) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    // `None`：还没见到记录的第一行
    let mut current: Option<Verdict> = None;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        let is_header = match parse_header(line) {
            Some(h) => {
                current = Some(verdict(&h, min_level));
                true
            }
            None => false,
        };
        match (current, is_header) {
            (Some(Verdict::Keep), _) => out.push(clip(line.to_string())),
            (Some(Verdict::MaskError), true) => out.push(clip(mask_error(line))),
            _ => {}
        }
    }
    out
}

fn tail(dir: &Path, wanted: usize, min_level: Option<NodeLogLevel>, budget: u64) -> NodeLogsResult {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut kept = 0usize;
    let mut budget_left = budget;
    // 还有没扫过的更早的日志
    let mut unread = false;
    for path in log_files(dir) {
        if kept >= wanted || budget_left == 0 {
            unread = true;
            break;
        }
        let Some((bytes, from_start)) = read_tail(&path, budget_left) else { continue };
        budget_left = budget_left.saturating_sub(bytes.len() as u64);
        let lines = filter(&bytes, min_level);
        kept += lines.len();
        chunks.push(lines);
        if !from_start {
            unread = true;
            break;
        }
    }
    // chunks 是最新的文件在前；输出旧的在前
    let mut lines: Vec<String> = chunks.into_iter().rev().flatten().collect();
    let excess = lines.len().saturating_sub(wanted);
    lines.drain(..excess);
    NodeLogsResult { lines, truncated: unread || excess > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: &str, level: &str, target: &str, rest: &str) -> String {
        format!("2026-10-30T{ts}.000000Z {level:>5} {target}: {rest}")
    }

    fn write(dir: &Path, name: &str, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn sample() -> Vec<String> {
        vec![
            line("10:00:00", "INFO", "anycon_daemon::daemon", "daemon started home=/h"),
            line("10:00:01", "DEBUG", "anycon_daemon::rpc", "rpc handled method=node.info"),
            line("10:00:02", "WARN", "anycon_daemon::peers", "peer stalled peer=abc"),
            line("10:00:03", "ERROR", "anycon_daemon::store", "cannot write error=disk full"),
            line("10:00:04", "INFO", "anycon_daemon::hub", "connected to hub"),
        ]
    }

    #[test]
    fn returns_the_last_lines_oldest_first() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-10-30", &sample());
        let r = read(d.path(), &NodeLogsParams { lines: Some(2), level: None });
        assert_eq!(r.lines, sample()[3..].to_vec());
        assert!(r.truncated, "older lines exist");
        let all = read(d.path(), &NodeLogsParams::default());
        assert_eq!(all.lines, sample());
        assert!(!all.truncated);
    }

    #[test]
    fn lines_default_to_200_and_are_capped_at_1000() {
        let d = tempfile::tempdir().unwrap();
        let many: Vec<String> =
            (0..1500).map(|i| line("10:00:00", "INFO", "anycon_daemon::x", &format!("n={i}"))).collect();
        write(d.path(), "daemon.log.2026-10-30", &many);
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!((r.lines.len(), r.truncated), (DEFAULT_LINES, true));
        assert!(r.lines.last().unwrap().ends_with("n=1499"));
        let r = read(d.path(), &NodeLogsParams { lines: Some(50_000), level: None });
        assert_eq!(r.lines.len(), MAX_LINES);
        assert!(r.lines.first().unwrap().ends_with("n=500"));
        let r = read(d.path(), &NodeLogsParams { lines: Some(0), level: None });
        assert_eq!(r.lines.len(), 1, "0 is raised to 1");
    }

    #[test]
    fn level_is_a_minimum() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "daemon.log.2026-10-30", &sample());
        let at = |level| read(d.path(), &NodeLogsParams { lines: None, level: Some(level) }).lines;
        assert_eq!(at(NodeLogLevel::Trace).len(), 5);
        assert_eq!(at(NodeLogLevel::Debug).len(), 5);
        assert_eq!(at(NodeLogLevel::Info).len(), 4);
        let warn = at(NodeLogLevel::Warn);
        assert_eq!(warn, vec![sample()[2].clone(), sample()[3].clone()]);
        assert_eq!(at(NodeLogLevel::Error), vec![sample()[3].clone()]);
    }

    #[test]
    fn continuation_lines_follow_the_level_of_their_record() {
        let d = tempfile::tempdir().unwrap();
        let lines = vec![
            line("10:00:00", "INFO", "anycon_daemon::a", "quiet one"),
            "    continued info".to_string(),
            line("10:00:01", "ERROR", "anycon_daemon::b", "loud one"),
            "    continued error".to_string(),
            "    more".to_string(),
        ];
        write(d.path(), "daemon.log.2026-10-30", &lines);
        let r = read(d.path(), &NodeLogsParams { lines: None, level: Some(NodeLogLevel::Error) });
        assert_eq!(r.lines, lines[2..].to_vec());
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(r.lines, lines);
    }

    #[test]
    fn adapter_lines_are_withheld_entirely_with_their_continuations() {
        let d = tempfile::tempdir().unwrap();
        let lines = vec![
            line("10:00:00", "WARN", "anycon_adapters::codex::handle", "codex: turn/interrupt failed error=SECRET-A"),
            "SECRET-B on a continuation line".to_string(),
            line(
                "10:00:01",
                "DEBUG",
                "anycon_adapters::claude_code::translate",
                "claude: ignoring malformed JSON line error=SECRET-C",
            ),
            line("10:00:02", "INFO", "anycon_daemon::agents", "agent probe result changed agent=codex"),
            line("10:00:03", "WARN", "anycon_adapters_lookalike", "not an adapters module"),
        ];
        write(d.path(), "daemon.log.2026-10-30", &lines);
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(r.lines, vec![lines[3].clone(), lines[4].clone()]);
        assert!(!r.lines.iter().any(|l| l.contains("SECRET")));
    }

    #[test]
    fn the_error_field_of_the_sessions_module_is_masked() {
        let d = tempfile::tempdir().unwrap();
        let lines = vec![
            line(
                "10:00:00",
                "WARN",
                "anycon_daemon::sessions",
                "cancel failed session=sess_1 error=agent exited: SECRET-A",
            ),
            "SECRET-B (a line of the error text)".to_string(),
            line(
                "10:00:01",
                "WARN",
                "anycon_daemon::sessions::wt",
                "cannot dispose session=sess_2 error=git said SECRET-C",
            ),
            line(
                "10:00:02",
                "INFO",
                "anycon_daemon::sessions",
                "deferring the restart session=sess_3 background_tasks=2",
            ),
            line("10:00:03", "ERROR", "anycon_daemon::store", "cannot write error=disk full"),
        ];
        write(d.path(), "daemon.log.2026-10-30", &lines);
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(
            r.lines,
            vec![
                line("10:00:00", "WARN", "anycon_daemon::sessions", "cancel failed session=sess_1 error=[withheld]"),
                line(
                    "10:00:01",
                    "WARN",
                    "anycon_daemon::sessions::wt",
                    "cannot dispose session=sess_2 error=[withheld]"
                ),
                lines[3].clone(),
                lines[4].clone(), // 别的模块的 error 字段照常返回
            ]
        );
    }

    #[test]
    fn long_lines_are_cut_to_2_kib_without_splitting_a_character() {
        let d = tempfile::tempdir().unwrap();
        let long = line("10:00:00", "INFO", "anycon_daemon::x", &"é".repeat(3000));
        let exact = line("10:00:01", "INFO", "anycon_daemon::x", &"a".repeat(MAX_LINE_BYTES - 60));
        write(d.path(), "daemon.log.2026-10-30", &[long, exact.clone()]);
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(r.lines.len(), 2);
        assert!(r.lines[0].len() <= MAX_LINE_BYTES && r.lines[0].ends_with('…'), "{}", r.lines[0].len());
        assert!(r.lines[0].starts_with("2026-10-30T10:00:00"));
        assert_eq!(r.lines[1], exact, "a line under the limit is untouched");
        assert!(!r.truncated, "cutting a line is not the same as leaving lines out");
    }

    #[test]
    fn a_short_current_file_is_topped_up_from_the_previous_day() {
        let d = tempfile::tempdir().unwrap();
        let old: Vec<String> =
            (0..5).map(|i| line("09:00:00", "INFO", "anycon_daemon::x", &format!("old={i}"))).collect();
        let new: Vec<String> =
            (0..2).map(|i| line("00:00:01", "INFO", "anycon_daemon::x", &format!("new={i}"))).collect();
        write(d.path(), "daemon.log.2026-10-29", &old);
        write(d.path(), "daemon.log.2026-10-30", &new);
        std::fs::write(d.path().join("launchd.err.log"), "not a daemon log\n").unwrap();
        let r = read(d.path(), &NodeLogsParams { lines: Some(4), level: None });
        assert_eq!(r.lines, vec![old[3].clone(), old[4].clone(), new[0].clone(), new[1].clone()]);
        assert!(r.truncated);
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(r.lines.len(), 7);
        assert!(!r.truncated);
    }

    #[test]
    fn the_scan_budget_caps_the_work_and_reports_truncation() {
        let d = tempfile::tempdir().unwrap();
        let many: Vec<String> =
            (0..400).map(|i| line("10:00:00", "INFO", "anycon_daemon::x", &format!("n={i:04}"))).collect();
        write(d.path(), "daemon.log.2026-10-30", &many);
        // 预算只够读最后一小截：开头那条被切断的半行不能出现在结果里
        let r = tail(d.path(), 1000, None, 1000);
        assert!(r.truncated);
        assert!(!r.lines.is_empty() && r.lines.len() < 400);
        assert!(r.lines.iter().all(|l| l.starts_with("2026-10-30T")), "no half line: {:?}", r.lines.first());
        assert!(r.lines.last().unwrap().ends_with("n=0399"));
        // 够大就读到头
        let r = tail(d.path(), 1000, None, 10_000_000);
        assert_eq!((r.lines.len(), r.truncated), (400, false));
    }

    #[test]
    fn a_missing_or_empty_log_directory_gives_an_empty_result() {
        let d = tempfile::tempdir().unwrap();
        let r = read(&d.path().join("nope"), &NodeLogsParams::default());
        assert_eq!((r.lines.len(), r.truncated), (0, false));
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!((r.lines.len(), r.truncated), (0, false));
        std::fs::write(d.path().join("daemon.log.2026-10-30"), "").unwrap();
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!((r.lines.len(), r.truncated), (0, false));
    }

    #[test]
    fn non_utf8_bytes_and_crlf_do_not_break_reading() {
        let d = tempfile::tempdir().unwrap();
        let mut body = line("10:00:00", "INFO", "anycon_daemon::x", "ok").into_bytes();
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(line("10:00:01", "INFO", "anycon_daemon::x", "bytes ").as_bytes());
        body.extend_from_slice(&[0xff, 0xfe]);
        body.push(b'\n');
        std::fs::write(d.path().join("daemon.log.2026-10-30"), body).unwrap();
        let r = read(d.path(), &NodeLogsParams::default());
        assert_eq!(r.lines.len(), 2);
        assert!(r.lines[0].ends_with("ok"));
        assert!(r.lines[1].contains('\u{fffd}'));
    }

    #[test]
    fn the_headers_the_daemon_really_writes_parse() {
        // 真实日志里的两行（tracing 默认格式：级别右对齐到 5 个字符）
        let a =
            "2026-10-24T23:57:07.359170Z  INFO anycon_daemon::daemon: daemon started home=/Users/x version=\"0.1.1\"";
        let h = parse_header(a).unwrap();
        assert_eq!((h.level, h.target), (NodeLogLevel::Info, "anycon_daemon::daemon"));
        let b = "2026-10-24T23:57:07.359170Z ERROR anycon_daemon::hub: connection failed";
        assert_eq!(parse_header(b).unwrap().level, NodeLogLevel::Error);
        assert!(parse_header("    indented continuation").is_none());
        assert!(parse_header("2026-10-24T23:57:07Z INFO no-colon-target message").is_none());
    }
}
