//! 各 adapter 共用的小工具：命令行解析、二进制定位（`which -a` + 已知目录 + `--version` 探活）、
//! semver 解析、带超时的一次性子进程调用、以及「换行分隔 JSON 子进程」封装（含优雅关闭）。
//!
//! 这里不含任何 agent 专属知识。`claude_code`、以后的 `codex` / `acp` 都用同一套。

pub mod bin;
pub mod child;
pub mod command;
pub mod probe_util;
pub mod proctree;
pub mod run;
pub mod version;

pub use bin::{resolve_binary, Candidate, CandidateProbe, CommandOrigin, Resolution, ResolveOptions};
pub use child::{
    ExitInfo, JsonlChild, LineEvent, ShutdownTimings, SpawnSpec, DEFAULT_MAX_LINE_BYTES, OVERSIZE_PREFIX_BYTES,
};
pub use command::{parse_command, CommandParseError, CommandSpec};
pub use probe_util::{effective_env, field, shell_type_a, version_timeout, Budget, VERSION_TIMEOUT_ENV};
pub use run::{run_capture, RunOutput, MAX_CAPTURE_BYTES};
pub use version::{parse_semver, version_at_least};

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 各 adapter 都收的图片附件类型（ADR-35）。daemon 在 `blob.put_begin` 的白名单和这里一致，
/// 发送时再按该 agent 的 `capabilities.attachments` 核对。
pub const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 「中性」工作目录：系统临时目录（取不到就退到 `/`）。找二进制、跑 `--version` 这类探测子进程时用——
/// 绝不用用户的家目录或项目目录（那里可能有克隆下来的仓库带着同名可执行文件）。
pub fn neutral_cwd() -> PathBuf {
    let t = std::env::temp_dir();
    if t.is_absolute() && t.is_dir() {
        t
    } else {
        PathBuf::from("/")
    }
}

/// 当前 Unix 毫秒。
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 截断到不超过 `max` 字节（落在字符边界上），超出时追加 `…`。用于错误信息里带 stderr 片段。
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
