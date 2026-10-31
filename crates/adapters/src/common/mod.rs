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

/// 作为图片直接交给模型的附件类型（ADR-35）。和 daemon 核对文件头的四种一致；其它类型的附件以路径交给 agent
/// （[`file_reference_text`]）。
pub const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 内置 adapter 的 `capabilities.attachments`：四种图片加 `*/*`（任意文件，ADR-35「任意文件」）。
pub fn accepted_attachments() -> Vec<String> {
    IMAGE_MIMES.iter().map(|m| m.to_string()).chain([anycon_protocol::ATTACHMENT_ANY_FILE.to_string()]).collect()
}

/// 这个附件作为图片交给模型（否则以路径交给 agent）。
pub fn is_inline_image(mime: &str) -> bool {
    IMAGE_MIMES.contains(&mime)
}

/// 不是图片的附件交给 agent 的方式：一段文字列出名字、类型、大小和 daemon 上的绝对路径，agent 用自己的工具去读。
/// 文件在 daemon 的数据目录里，不在 workspace 里——Claude Code 读它可能要走权限审批。没有这种附件时返回 `None`。
/// `name` 是 daemon 校验过的展示名（不含控制字符、换行），可以直接放进文字。
pub fn file_reference_text<'a>(
    files: impl IntoIterator<Item = (&'a anycon_protocol::AttachmentRef, &'a std::path::Path)>,
) -> Option<String> {
    let lines: Vec<String> = files
        .into_iter()
        .filter(|(a, _)| !is_inline_image(&a.mime))
        .map(|(a, path)| format!("- {} ({}, {} bytes): {}", a.name, a.mime, a.bytes, path.display()))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "The user attached {}. {} saved on this machine; read {} from the path{} below.\n{}",
        if lines.len() == 1 { "a file" } else { "files" },
        if lines.len() == 1 { "It is" } else { "They are" },
        if lines.len() == 1 { "it" } else { "them" },
        if lines.len() == 1 { "" } else { "s" },
        lines.join("\n")
    ))
}

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
