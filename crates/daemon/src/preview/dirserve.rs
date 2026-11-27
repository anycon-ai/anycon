//! 目录预览的 stream（ADR-62，docs/spec/daemon.md § 19.6）：daemon 自己把一个项目目录（或 `.scratch`）里的文件按静态网站
//! 提供。一个 HTTP stream 回一个文件，不连任何上游；agent 生成的 HTML 报告和它旁边的截图、样式就这样在预览 origin 里打开。
//!
//! 规则（[`prepare`] 及其下的纯函数，各有单元测试）：
//!
//! - 只有 `GET` / `HEAD`，别的方法 `405`（带 `allow`）；WebSocket 升级回普通的 `404`；
//! - 路径去掉 `#…` 和 `?query` 后按 UTF-8 百分号解码（不是合法 UTF-8 → `400`），含 NUL、`\` 或 `..` 段 → `400`，
//!   空段和 `.` 跳过；
//! - **不提供隐藏文件**：任何一段以 `.` 开头都是 `404`（`.env`、`.git`、`.anycon/attachments` 之类），只有开头的
//!   `.anycon/worktrees/<名字>/` 例外——worktree 会话的工作目录在这里；`<名字>` 和之后的段照样检查。请求路径和
//!   canonicalize 之后相对目录的路径都要过这一条（指向 `.git` 的符号链接也读不到）。这不是完整的边界：报告里的脚本
//!   读得到同一个目录里别的普通文件；
//! - 每个请求都重新解析目录（项目改名、删除之后立刻 `404`），目标 canonicalize 之后必须还在目录里（指向目录之外的
//!   符号链接 `404`），再用 [`crate::fsopen::open_file_in`] 从目录开始逐段打开（检查之后被换成符号链接也逃不出去）；
//! - 目录：不以 `/` 结尾 → `301` 到加了 `/` 的地址（页面里的相对地址才能对），以 `/` 结尾 → 它的 `index.html`，
//!   没有就 `404`；不列目录；
//! - 文件：按扩展名给 `content-type`，带 `last-modified`、弱 `etag`（大小 + 修改时间）、`cache-control: no-cache`、
//!   `x-content-type-options: nosniff`；`if-none-match` 命中（没有它时 `if-modified-since` 不早于修改时间）回 `304`。
//!   文本类大于 1 KiB 的边读边 gzip（`encoding = gzip`，不带 `content-length`），其余带 `content-length`。
//!   不支持 `Range`（总是整个文件，`200`）。
//!
//! 看不到的东西一律 `404`，不区分「不存在」和「不给看」。日志不记路径、头和正文。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anycon_protocol::{
    ErrorCode, PreviewBodyEncoding, PreviewRequestHead, PreviewResetCode, PreviewResponseHead, RpcError,
};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use tokio::io::AsyncReadExt;

use super::ledger::DATA_CHUNK;
use super::rewrite::{self, Headers, COMPRESS_MIN_BYTES};
use super::upstream::{send_data, Credit, Emit, Gzip, UpEvent};
use crate::workspace::{Workspace, SCRATCH_DIR};

/// 重定向地址里一段路径要编码的字符：控制字符、空格和在路径里有特殊含义的符号（非 ASCII 总是编码）。
const SEGMENT: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}');
const SEGMENT_STRICT: &AsciiSet = &SEGMENT.add(b'/').add(b'\\').add(b'^').add(b'|');

/// 一个目录预览 stream 需要的一切。
pub struct DirCtx {
    pub workspace: Arc<Workspace>,
    /// 票据的目录：项目名或 `.scratch`。
    pub dir: String,
    pub head: PreviewRequestHead,
    pub credit: Arc<Credit>,
    pub emit: Emit,
}

/// 目录预览的目录 → canonical 路径。`dir` 是项目名（关联项目也是，规则同 `Workspace::project_path`）或 `.scratch`
/// （全部 scratch 会话的工作目录，在主根下）；
/// 名字不合法（空、`.`、`..`、含 `/` `\` NUL、以 `.` 开头的其它名字）→ `invalid`，不存在 / 不是目录 → `not_found`。
pub fn resolve_dir(ws: &Workspace, dir: &str) -> Result<PathBuf, RpcError> {
    if dir == SCRATCH_DIR {
        let p = ws.resolve_rel(SCRATCH_DIR)?;
        if !p.is_dir() {
            return Err(RpcError::new(ErrorCode::NotFound, ".scratch is not a directory"));
        }
        return Ok(p);
    }
    ws.project_path(dir)
}

// ---------------------------------------------------------------- 纯规则

/// 解析过的请求路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqPath {
    /// 解码之后的各段（没有空段和 `.`）。
    pub segs: Vec<String>,
    /// 原路径以 `/` 结尾（或者就是 `/`）。
    pub trailing_slash: bool,
    /// 从 `?` 起的查询串（含 `?`）；没有就是空串。重定向时原样带上。
    pub query: String,
}

/// 解析请求的 `path`。不合法（不是 UTF-8、含 NUL / `\`、有 `..` 段）返回 `None`（`400`）。
pub fn parse_path(raw: &str) -> Option<ReqPath> {
    let raw = raw.split('#').next().unwrap_or("");
    let (path, query) = match raw.find('?') {
        Some(i) => (&raw[..i], &raw[i..]),
        None => (raw, ""),
    };
    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    let mut segs = Vec::new();
    for s in decoded.split('/') {
        match s {
            "" | "." => {}
            ".." => return None,
            s => segs.push(s.to_string()),
        }
    }
    let trailing_slash = decoded.is_empty() || decoded.ends_with('/');
    Some(ReqPath { segs, trailing_slash, query: query.to_string() })
}

/// 这条路径（相对目录的各段）是不是落在隐藏文件 / 目录上：任何一段以 `.` 开头。开头的 `.anycon/worktrees/<名字>/`
/// 例外（worktree 会话的工作目录），`<名字>` 和之后的段照样检查。
pub fn is_hidden<S: AsRef<str>>(segs: &[S]) -> bool {
    let worktree = segs.len() >= 3 && segs[0].as_ref() == ".anycon" && segs[1].as_ref() == "worktrees";
    let skip = if worktree { 2 } else { 0 };
    segs[skip..].iter().any(|s| s.as_ref().starts_with('.'))
}

/// 按文件名的扩展名给 `content-type`。不认识的是 `application/octet-stream`（配合 `nosniff`，浏览器不会去猜）。
pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "txt" | "md" | "markdown" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "tsv" => "text/tab-separated-values; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// 弱 `etag`：文件大小和修改时间（毫秒），都是十六进制。
pub fn etag(len: u64, mtime: Option<SystemTime>) -> String {
    let ms = mtime.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis());
    format!("W/\"{len:x}-{ms:x}\"")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// 条件请求命中（回 `304`）：有 `if-none-match` 时只看它（弱比较，`*` 也算）；没有时看 `if-modified-since`
/// 是否不早于修改时间（按秒，HTTP 日期只到秒）。
pub fn not_modified(headers: &[(String, String)], etag: &str, mtime: Option<SystemTime>) -> bool {
    let weakless = |t: &str| t.strip_prefix("W/").unwrap_or(t).to_string();
    if let Some(v) = header(headers, "if-none-match") {
        let ours = weakless(etag);
        return v.split(',').map(str::trim).any(|t| t == "*" || weakless(t) == ours);
    }
    let (Some(v), Some(m)) = (header(headers, "if-modified-since"), mtime) else { return false };
    let Ok(since) = httpdate::parse_http_date(v.trim()) else { return false };
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    secs(m) <= secs(since)
}

/// 目录的 `301` 地址：各段重新编码、以 `/` 结尾，再接原来的查询串。总是以单个 `/` 开头加一个非空段，
/// 不会变成 `//host` 这样的协议相对地址。
pub fn directory_location(req: &ReqPath) -> String {
    let mut out = String::new();
    for s in &req.segs {
        out.push('/');
        out.extend(utf8_percent_encode(s, SEGMENT_STRICT));
    }
    out.push('/');
    // 查询串来自浏览器解析过的 URL；有控制字符（不该有）就不带，免得头部不合法
    if !req.query.chars().any(char::is_control) {
        out.push_str(&req.query);
    }
    out
}

// ---------------------------------------------------------------- 响应

/// 准备好的响应。
pub enum Reply {
    /// 状态码 + 头 + 短正文：错误、重定向、`304`。
    Small { status: u16, headers: Headers, body: Vec<u8> },
    /// `200` 和一个打开的文件。`len` 是打开时的大小，只发这么多（文件在读的过程中变长也不多发）。
    File { headers: Headers, file: std::fs::File, len: u64, gzip: bool, send_body: bool },
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Small { status, headers, .. } => {
                f.debug_struct("Small").field("status", status).field("headers", headers).finish_non_exhaustive()
            }
            Reply::File { headers, len, gzip, send_body, .. } => f
                .debug_struct("File")
                .field("headers", headers)
                .field("len", len)
                .field("gzip", gzip)
                .field("send_body", send_body)
                .finish_non_exhaustive(),
        }
    }
}

impl Reply {
    pub fn status(&self) -> u16 {
        match self {
            Reply::Small { status, .. } => *status,
            Reply::File { .. } => 200,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        match self {
            Reply::Small { headers, .. } | Reply::File { headers, .. } => header(headers, name),
        }
    }
}

fn h(name: &str, value: impl Into<String>) -> (String, String) {
    (name.to_string(), value.into())
}

/// 一段纯文本的响应（错误页）。
fn text(status: u16, body: &str) -> Reply {
    Reply::Small {
        status,
        headers: vec![
            h("content-type", "text/plain; charset=utf-8"),
            h("content-length", body.len().to_string()),
            h("cache-control", "no-cache"),
            h("x-content-type-options", "nosniff"),
        ],
        body: body.as_bytes().to_vec(),
    }
}

fn not_found() -> Reply {
    text(404, "not found\n")
}

/// 相对目录的各段（canonical 路径去掉目录前缀）。不在目录之下返回 `None`。
fn rel_segments(target: &Path, root: &Path) -> Option<Vec<String>> {
    let rel = target.strip_prefix(root).ok()?;
    Some(rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
}

/// 按请求准备响应（同步：解析目录、canonicalize、打开文件；调用方放到阻塞线程上）。
pub fn prepare(ws: &Workspace, dir: &str, head: &PreviewRequestHead) -> Reply {
    if head.websocket {
        return not_found();
    }
    let head_only = head.method.eq_ignore_ascii_case("HEAD");
    if !head_only && !head.method.eq_ignore_ascii_case("GET") {
        let mut r = text(405, "method not allowed\n");
        if let Reply::Small { headers, .. } = &mut r {
            headers.push(h("allow", "GET, HEAD"));
        }
        return r;
    }
    let Some(req) = parse_path(&head.path) else { return text(400, "bad request\n") };
    if is_hidden(&req.segs) {
        return not_found();
    }
    let Ok(root) = resolve_dir(ws, dir) else { return not_found() };
    let mut segs = req.segs.clone();
    let mut index = false;
    let target = loop {
        let mut joined = root.clone();
        joined.extend(&segs);
        let Ok(target) = crate::sys::canonicalize(&joined) else { return not_found() };
        // 符号链接逃出目录，或者落在隐藏文件上
        match rel_segments(&target, &root) {
            Some(rel) if !is_hidden(&rel) => {}
            _ => return not_found(),
        }
        let Ok(md) = std::fs::metadata(&target) else { return not_found() };
        if !md.is_dir() {
            break target;
        }
        if index {
            // `index.html` 自己是个目录
            return not_found();
        }
        if !req.trailing_slash {
            let mut r = text(301, "");
            if let Reply::Small { headers, .. } = &mut r {
                headers.push(h("location", directory_location(&req)));
            }
            return r;
        }
        segs.push("index.html".into());
        index = true;
    };
    crate::fsopen::after_resolve(&target);
    let file = match crate::fsopen::open_file_in(&target, &root) {
        Ok(f) => f,
        Err(crate::fsopen::OpenError::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return text(403, "forbidden\n")
        }
        Err(_) => return not_found(),
    };
    let Ok(md) = file.metadata() else { return text(500, "cannot read the file\n") };
    let len = md.len();
    let mtime = md.modified().ok();
    let tag = etag(len, mtime);
    let mut headers = vec![h("etag", tag.clone()), h("cache-control", "no-cache")];
    if let Some(m) = mtime {
        headers.push(h("last-modified", httpdate::fmt_http_date(m)));
    }
    if not_modified(&head.headers, &tag, mtime) {
        return Reply::Small { status: 304, headers, body: Vec::new() };
    }
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ct = content_type(&name);
    headers.push(h("content-type", ct));
    headers.push(h("x-content-type-options", "nosniff"));
    let gzip = !head_only && rewrite::is_compressible(ct) && len > COMPRESS_MIN_BYTES;
    if !gzip {
        headers.push(h("content-length", len.to_string()));
    }
    Reply::File { headers, file, len, gzip, send_body: !head_only }
}

/// 任务入口：准备响应，发 `RESPONSE`、`DATA`\*、`END`。请求体（不该有）不读：channel 由隧道持有，
/// 发不进来的数据隧道照样归还窗口。
pub async fn run(ctx: DirCtx) {
    let DirCtx { workspace, dir, head, credit, emit } = ctx;
    let head_only = head.method.eq_ignore_ascii_case("HEAD");
    let reply = match tokio::task::spawn_blocking(move || prepare(&workspace, &dir, &head)).await {
        Ok(r) => r,
        Err(_) => {
            emit(UpEvent::Reset { code: PreviewResetCode::Internal, message: "cannot prepare the response" });
            return;
        }
    };
    match reply {
        Reply::Small { status, headers, body } => {
            let head = PreviewResponseHead { status, headers, encoding: PreviewBodyEncoding::Identity };
            if !emit(UpEvent::Head(head)) {
                return;
            }
            if !head_only && !body.is_empty() && !send_data(&credit, &emit, &body).await {
                return;
            }
            emit(UpEvent::End);
        }
        Reply::File { headers, file, len, gzip, send_body } => {
            let encoding = if gzip { PreviewBodyEncoding::Gzip } else { PreviewBodyEncoding::Identity };
            if !emit(UpEvent::Head(PreviewResponseHead { status: 200, headers, encoding })) {
                return;
            }
            if send_body && !send_file(file, len, gzip, &credit, &emit).await {
                return;
            }
            emit(UpEvent::End);
        }
    }
}

/// 把文件的前 `len` 个字节按窗口发出去（`gzip` 时边读边压缩）。读失败发 `RESET { internal }`；窗口关了 / 隧道没了
/// 或读失败都返回 `false`。
async fn send_file(file: std::fs::File, len: u64, gzip: bool, credit: &Credit, emit: &Emit) -> bool {
    let mut f = tokio::fs::File::from_std(file).take(len);
    let mut gz = gzip.then(Gzip::new);
    let mut buf = vec![0u8; DATA_CHUNK];
    loop {
        let n = match f.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => {
                emit(UpEvent::Reset { code: PreviewResetCode::Internal, message: "cannot read the file" });
                return false;
            }
        };
        let ok = match gz.as_mut() {
            Some(g) => {
                let out = g.push(&buf[..n]);
                out.is_empty() || send_data(credit, emit, &out).await
            }
            None => send_data(credit, emit, &buf[..n]).await,
        };
        if !ok {
            return false;
        }
    }
    match gz {
        Some(g) => {
            let tail = g.finish();
            tail.is_empty() || send_data(credit, emit, &tail).await
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn segs(p: &str) -> Vec<String> {
        parse_path(p).unwrap().segs
    }

    #[test]
    fn paths_are_decoded_split_and_checked() {
        let p = parse_path("/reports/%E6%B5%8B%E8%AF%95%20a.html?x=1#top").unwrap();
        assert_eq!(p.segs, ["reports", "测试 a.html"]);
        assert!(!p.trailing_slash);
        assert_eq!(p.query, "?x=1");
        assert_eq!(parse_path("/").unwrap(), ReqPath { segs: vec![], trailing_slash: true, query: String::new() });
        assert_eq!(parse_path("").unwrap().segs, Vec::<String>::new());
        assert!(parse_path("/a/").unwrap().trailing_slash);
        // 空段和 `.` 跳过
        assert_eq!(segs("//a/./b//c"), ["a", "b", "c"]);
        // `..`、NUL、反斜杠、坏的 UTF-8 都不行（解码之后再判断）
        for bad in ["/a/../b", "/%2e%2e/x", "/a%00b", "/a\\b", "/a%5Cb", "/%ff"] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        // 查询串里的东西不影响路径
        assert_eq!(segs("/a?b=../..&c=%00"), ["a"]);
        // 编码过的 `/` 解码后也是分隔符
        assert_eq!(segs("/a%2Fb"), ["a", "b"]);
    }

    #[test]
    fn hidden_segments_are_refused_except_the_worktree_prefix() {
        for hidden in [&[".env"][..], &[".git", "config"], &["a", ".hidden", "b"], &[".anycon", "attachments", "x"]] {
            assert!(is_hidden(hidden), "{hidden:?}");
        }
        for visible in [&["a.html"][..], &["reports", "x.png"], &[]] {
            assert!(!is_hidden(visible), "{visible:?}");
        }
        // worktree 会话的工作目录
        assert!(!is_hidden(&[".anycon", "worktrees", "crisp-otter", "reports", "a.html"]));
        assert!(!is_hidden(&[".anycon", "worktrees", "crisp-otter"]));
        // 前缀之后照样检查，worktree 名字本身也检查
        assert!(is_hidden(&[".anycon", "worktrees", "x", ".env"]));
        assert!(is_hidden(&[".anycon", "worktrees", ".x", "a"]));
        // 只有两段不算 worktree
        assert!(is_hidden(&[".anycon", "worktrees"]));
        // 前缀只在开头才算
        assert!(is_hidden(&["a", ".anycon", "worktrees", "x", "b"]));
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("测试报告.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("INDEX.HTM"), "text/html; charset=utf-8");
        assert_eq!(content_type("a.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("app.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("data.json"), "application/json; charset=utf-8");
        assert_eq!(content_type("notes.md"), "text/plain; charset=utf-8");
        assert_eq!(content_type("shot.PNG"), "image/png");
        assert_eq!(content_type("a.jpeg"), "image/jpeg");
        assert_eq!(content_type("logo.svg"), "image/svg+xml");
        assert_eq!(content_type("f.woff2"), "font/woff2");
        assert_eq!(content_type("report.pdf"), "application/pdf");
        assert_eq!(content_type("bundle.zip"), "application/zip");
        for unknown in ["Makefile", "a.exe", "noext.", ".env"] {
            assert_eq!(content_type(unknown), "application/octet-stream", "{unknown}");
        }
        // 文本类会被压缩，图片和 zip 不会
        assert!(rewrite::is_compressible(content_type("a.html")));
        assert!(rewrite::is_compressible(content_type("a.svg")));
        assert!(!rewrite::is_compressible(content_type("a.png")));
        assert!(!rewrite::is_compressible(content_type("a.zip")));
    }

    #[test]
    fn etags_and_conditional_requests() {
        let m = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let tag = etag(4096, Some(m));
        assert_eq!(tag, format!("W/\"1000-{:x}\"", 1_700_000_000_123u64));
        assert_ne!(etag(4097, Some(m)), tag);
        assert_ne!(etag(4096, Some(m + Duration::from_millis(1))), tag);
        let hs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        assert!(not_modified(&hs(&[("if-none-match", &tag)]), &tag, Some(m)));
        // 弱比较：去掉 W/ 也算
        assert!(not_modified(&hs(&[("if-none-match", tag.trim_start_matches("W/"))]), &tag, Some(m)));
        assert!(not_modified(&hs(&[("if-none-match", &format!("\"x\", {tag}"))]), &tag, Some(m)));
        assert!(not_modified(&hs(&[("if-none-match", "*")]), &tag, Some(m)));
        assert!(!not_modified(&hs(&[("if-none-match", "W/\"other\"")]), &tag, Some(m)));
        // 有 if-none-match 时不看 if-modified-since
        let later = httpdate::fmt_http_date(m + Duration::from_secs(60));
        assert!(!not_modified(&hs(&[("if-none-match", "\"x\""), ("if-modified-since", &later)]), &tag, Some(m)));
        // if-modified-since 按秒
        assert!(not_modified(&hs(&[("if-modified-since", &later)]), &tag, Some(m)));
        assert!(not_modified(&hs(&[("if-modified-since", &httpdate::fmt_http_date(m))]), &tag, Some(m)));
        let earlier = httpdate::fmt_http_date(m - Duration::from_secs(1));
        assert!(!not_modified(&hs(&[("if-modified-since", &earlier)]), &tag, Some(m)));
        assert!(!not_modified(&hs(&[("if-modified-since", "garbage")]), &tag, Some(m)));
        assert!(!not_modified(&hs(&[("if-modified-since", &later)]), &tag, None));
        assert!(!not_modified(&[], &tag, Some(m)));
    }

    #[test]
    fn directory_redirects_are_always_same_origin_paths() {
        let loc = |p: &str| directory_location(&parse_path(p).unwrap());
        assert_eq!(loc("/reports"), "/reports/");
        assert_eq!(loc("/reports?x=1"), "/reports/?x=1");
        assert_eq!(loc("/%E6%8A%A5%E5%91%8A"), "/%E6%8A%A5%E5%91%8A/");
        assert_eq!(loc("/a%20b"), "/a%20b/");
        // `//evil.example` 不会变成协议相对地址
        assert_eq!(loc("//evil.example"), "/evil.example/");
        assert_eq!(loc("/a%3Fb"), "/a%3Fb/");
    }
}
