//! 杂项工具：时间、id、时长 / 大小解析、UTC 时间格式化。不依赖 daemon 其它模块。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;

/// 当前 Unix 毫秒。
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 会话 id：`sess_` + 22 位 base62。
pub fn new_session_id() -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(27);
    s.push_str("sess_");
    for _ in 0..22 {
        s.push(BASE62[rng.gen_range(0..62)] as char);
    }
    s
}

/// worktree id：`wt_` + 22 位 base62。
pub fn new_worktree_id() -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(25);
    s.push_str("wt_");
    for _ in 0..22 {
        s.push(BASE62[rng.gen_range(0..62)] as char);
    }
    s
}

/// uuid v4 字符串（request / message / job id）。
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 解析 `10m` / `30s` / `500ms` / `2h` / `7d` / `1w` / `0`。纯数字按秒。
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    let secs = match unit.trim() {
        "" | "s" | "sec" | "secs" => n,
        "ms" => n / 1000.0,
        "m" | "min" | "mins" => n * 60.0,
        "h" | "hr" | "hrs" => n * 3600.0,
        "d" => n * 86400.0,
        "w" => n * 7.0 * 86400.0,
        other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
    };
    if !(0.0..=1.0e10).contains(&secs) {
        return Err(format!("duration out of range: {s}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

/// 解析 `50MiB` / `10MB` / `1024` / `2G`。
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid size: {s}"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1024.0 * 1024.0,
        "g" | "gb" => 1e9,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit {other:?} in {s:?}")),
    };
    Ok((n * mult) as u64)
}

/// 把 Unix 毫秒拆成 (year, month, day, hour, min, sec)，UTC。
pub fn civil_from_ms(ms: u64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Howard Hinnant 的 civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `2026-10-11 12:00:00`（UTC）。
pub fn fmt_utc(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_ms(ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// `20261011T120000Z`，回收站目录后缀。
pub fn fmt_compact(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_ms(ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `fmt_compact` 的逆。
pub fn parse_compact(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(4..6)?, num(6..8)?, num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86400 + h * 3600 + mi * 60 + se;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000)
}

/// `~/x` 展开为 `$HOME/x`。
pub fn expand_tilde(p: &str) -> std::path::PathBuf {
    if p == "~" {
        return home_dir().unwrap_or_else(|| p.into());
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    p.into()
}

pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").filter(|s| !s.is_empty()).map(Into::into)
}

/// 截断到最多 `max` 个字符（按 char），尽量落在词边界上。
pub fn truncate_title(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max).collect();
    // 截断点落在词中间且前面有空格时，退回到最后一个空格
    let next_is_space = s.chars().nth(max).is_some_and(|c| c.is_whitespace());
    if !next_is_space {
        if let Some(idx) = cut.rfind(' ') {
            if idx >= max / 2 {
                return cut[..idx].trim_end().to_string();
            }
        }
    }
    cut.trim_end().to_string()
}

/// 摘要用：取前 `max` 个字符，换行折叠成空格。
pub fn preview_of(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        flat.to_string()
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// 把字节数格式化成人类可读。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 键有序的紧凑 JSON 文本：比较两份配置 / 做指纹时不受 HashMap 迭代顺序影响。
pub fn canonical_json(v: &serde_json::Value) -> String {
    fn write(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push(':');
                    write(&m[k], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    write(v, &mut s);
    s
}

/// 全局共享的随机字节。
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::thread_rng().fill(&mut buf[..]);
    buf
}

/// 关键路径上不吞错误：失败至少记一条 warn（`let _ =` 只留给确实无所谓的收尾）。
pub fn warn_err<T, E: std::fmt::Display>(what: &str, r: Result<T, E>) {
    if let Err(e) = r {
        tracing::warn!(error = %e, "{what} failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("50MiB").unwrap(), 50 * 1024 * 1024);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("2GB").unwrap(), 2_000_000_000);
        assert!(parse_size("1qq").is_err());
    }

    #[test]
    fn civil_roundtrip() {
        let ms = 1_791_720_000_000u64; // 2026-10-11T12:00:00Z
        assert_eq!(fmt_compact(ms), "20261011T120000Z");
        assert_eq!(parse_compact("20261011T120000Z"), Some(ms));
        assert_eq!(fmt_utc(0), "1970-01-01 00:00:00");
        assert_eq!(parse_compact("garbage"), None);
    }

    #[test]
    fn titles() {
        assert_eq!(truncate_title("hello world", 60), "hello world");
        let long = "word ".repeat(30);
        let t = truncate_title(&long, 60);
        assert!(t.chars().count() <= 60);
        assert!(t.ends_with("word"));
        assert_eq!(truncate_title("  a \n b  ", 60), "a b");
    }

    #[test]
    fn session_ids() {
        let id = new_session_id();
        assert!(id.starts_with("sess_"));
        assert_eq!(id.len(), 27);
    }
}
