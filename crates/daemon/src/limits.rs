//! 大小上限（ADR-30）：hub 对单个信封有上限，传输层有分片兜底，但 daemon 自己也不该产出无意义的巨型消息。
//!
//! - `tool_result.output`（以及 `tool_call.input`）超过 64 KiB：存储和发送都只保留前 48 KiB + 后 16 KiB，中间放
//!   `…[truncated N bytes]`；对象 / 数组形的值保留结构，只截里面超长的字符串叶子；
//! - `tool_output_delta` 每个工具调用累计转发不超过 64 KiB，超出的丢弃（见 `sessions.rs` 的 `DeltaBudget`）；
//! - `session.history` 一页按编码后字节数封顶 192 KiB，超出的留给下一页（`has_more`）；单个事件超过上限时单独成页；
//! - `session.send` 的内容最多 1 MiB，免得一条用户消息把以后的每一页历史都撑爆；
//! - `task` 快照：`agents` 最多 500 项，`label` / `title` / `last_tool` 截到 200 字符，`error` / `summary` 截到 4 KiB；
//! - `notice.message` 超过 64 KiB 同工具输出一样头尾截断。

use anycon_protocol::{Event, SessionEvent, TaskAgent, TaskAgentState};
use serde_json::Value;

/// `tool_result.output` / `tool_call.input` 的上限。
pub const TOOL_OUTPUT_MAX: usize = 64 * 1024;
/// 超出时保留的头 / 尾。
pub const TOOL_OUTPUT_HEAD: usize = 48 * 1024;
pub const TOOL_OUTPUT_TAIL: usize = 16 * 1024;
/// 每个工具调用累计转发的 `tool_output_delta` 上限。
pub const TOOL_DELTA_MAX: usize = 64 * 1024;
/// 一页 `session.history` 的字节上限（事件逐个 JSON 编码后相加）。
pub const HISTORY_PAGE_MAX: usize = 192 * 1024;
/// `session.send` / `session.create.prompt` 的内容上限。
pub const SEND_CONTENT_MAX: usize = 1024 * 1024;
/// `task` 快照里 `agents`（以及 `phases`）的最大项数。
pub const TASK_AGENTS_MAX: usize = 500;
/// `task` 快照里 `label` / `title` / `last_tool` 的最大字符数。
pub const TASK_LABEL_MAX_CHARS: usize = 200;
/// `task` 快照里 `error` / `summary` 的最大字节数。
pub const TASK_TEXT_MAX: usize = 4 * 1024;
/// `…[truncated N bytes]` 提示（加上两个换行）最多占多少字节。
const MARKER_SLACK: usize = 128;
/// JSON 值里单个字符串叶子超过它就先截（再看整体是否仍超限）。
const LEAF_MAX: usize = 8 * 1024;

/// 在 UTF-8 字符边界上取不超过 `n` 字节的前缀。
fn prefix_at(s: &str, n: usize) -> &str {
    let mut end = n.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 在 UTF-8 字符边界上取不超过 `n` 字节的后缀。
fn suffix_at(s: &str, n: usize) -> &str {
    let mut start = s.len().saturating_sub(n);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// 文本超过 `max` 字节时保留头 `head` + 尾 `tail`，中间放 `…[truncated N bytes]`（N = 去掉的字节数）。
/// 没超限返回 `None`，否则返回 (截断后的文本, N)。
pub fn truncate_middle_counted(s: &str, max: usize, head: usize, tail: usize) -> Option<(String, usize)> {
    // 留一点余量放得下提示本身：截过一次的文本（头 + 尾 + 提示）再截一次必须是空操作（读历史时会再过一遍）
    if s.len() <= max + MARKER_SLACK {
        return None;
    }
    let h = prefix_at(s, head);
    let t = suffix_at(s, tail);
    let removed = s.len() - h.len() - t.len();
    Some((format!("{h}\n…[truncated {removed} bytes]\n{t}"), removed))
}

/// [`truncate_middle_counted`] 不带字节数的版本。
pub fn truncate_middle(s: &str, max: usize, head: usize, tail: usize) -> Option<String> {
    truncate_middle_counted(s, max, head, tail).map(|(t, _)| t)
}

/// 工具输出专用的 [`truncate_middle`]（64 KiB = 48 KiB 头 + 16 KiB 尾）。
pub fn truncate_tool_text(s: &str) -> Option<String> {
    truncate_middle(s, TOOL_OUTPUT_MAX, TOOL_OUTPUT_HEAD, TOOL_OUTPUT_TAIL)
}

/// 把 JSON 值里每个超过 `leaf` 字节的字符串叶子头尾截断（保留结构和键）。
fn truncate_leaves_to(v: &mut Value, leaf: usize) {
    match v {
        Value::String(s) => {
            if let Some(t) = truncate_middle(s, leaf, leaf * 3 / 4, leaf / 4) {
                *s = t;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| truncate_leaves_to(x, leaf)),
        Value::Object(o) => o.values_mut().for_each(|x| truncate_leaves_to(x, leaf)),
        _ => {}
    }
}

/// 数组 / 对象的条目太多：每个容器只保留前 `keep` 项，其余换成一条标记（数组里是一个字符串元素，对象里是 `"…truncated"` 键）。
fn trim_containers(v: &mut Value, keep: usize) {
    match v {
        Value::Array(a) => {
            if a.len() > keep {
                let dropped = a.len() - keep;
                a.truncate(keep);
                a.push(Value::String(format!("…[truncated {dropped} items]")));
            }
            a.iter_mut().for_each(|x| trim_containers(x, keep));
        }
        Value::Object(o) => {
            if o.len() > keep {
                let dropped = o.len() - keep;
                let extra: Vec<String> = o.keys().skip(keep).cloned().collect();
                for k in extra {
                    o.remove(&k);
                }
                o.insert("…truncated".into(), Value::String(format!("{dropped} entries")));
            }
            o.values_mut().for_each(|x| trim_containers(x, keep));
        }
        _ => {}
    }
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

/// 把一个 JSON 值限制在约 64 KiB 以内。改动过就返回丢掉的字节数（字符串 = 提示行里的 N；对象 / 数组 = 序列化长度之差），
/// 没改动返回 `None`。
///
/// - 字符串：直接头尾截断；
/// - 对象 / 数组（`tool_call.input`、对象形的 `tool_result.output`）**保留结构**（客户端按字段渲染，比如 Write 工具的
///   diff，值的类型不能从 object 变成 string）：依次把超长的字符串叶子缩到 8 KiB / 2 KiB / 512 B / 128 B，直到整体不超限；
///   还是太大（成千上万个小字段）就只保留每个容器的前 256 / 64 / 16 项，其余换成一条 `…[truncated N items]` 标记；
///   只有这样还装不下的极端情况才退成截断后的 JSON 文本。
pub fn limit_value_counted(v: &mut Value) -> Option<u64> {
    if let Value::String(s) = v {
        let (t, removed) = truncate_middle_counted(s, TOOL_OUTPUT_MAX, TOOL_OUTPUT_HEAD, TOOL_OUTPUT_TAIL)?;
        *s = t;
        return Some(removed as u64);
    }
    let size = json_len(v);
    if size <= TOOL_OUTPUT_MAX {
        return None;
    }
    for leaf in [LEAF_MAX, 2 * 1024, 512, 128] {
        truncate_leaves_to(v, leaf);
        if json_len(v) <= TOOL_OUTPUT_MAX {
            break;
        }
    }
    for keep in [256, 64, 16] {
        if json_len(v) <= TOOL_OUTPUT_MAX {
            break;
        }
        trim_containers(v, keep);
    }
    let text = serde_json::to_string(v).unwrap_or_default();
    if text.len() > TOOL_OUTPUT_MAX {
        *v = Value::String(truncate_tool_text(&text).unwrap_or(text));
    }
    Some(size.saturating_sub(json_len(v)) as u64)
}

/// [`limit_value_counted`]，只关心有没有改动。
pub fn limit_value(v: &mut Value) -> bool {
    limit_value_counted(v).is_some()
}

/// 字符串超过 `max` 个字符时截成 `max` 个字符（最后一个是 `…`）。截过的结果再截一次是空操作。
fn cut_chars(s: &mut String, max: usize) -> bool {
    if s.chars().count() <= max {
        return false;
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    *s = t;
    true
}

/// 字符串超过 `max` 字节时在字符边界截断并补 `…`（结果不超过 `max` 字节）。截过的结果再截一次是空操作。
fn cut_bytes(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut t = prefix_at(s, max.saturating_sub('…'.len_utf8())).to_string();
    t.push('…');
    *s = t;
    true
}

fn cut_chars_opt(s: &mut Option<String>, max: usize) -> bool {
    s.as_mut().is_some_and(|s| cut_chars(s, max))
}

fn cut_bytes_opt(s: &mut Option<String>, max: usize) -> bool {
    s.as_mut().is_some_and(|s| cut_bytes(s, max))
}

/// `agents` 超过上限时丢掉多出的项：先保住 `running` / `failed` 的（用户最想看的），空位再按原顺序补其它的，
/// 保留下来的仍然按原来的顺序排列。
fn limit_task_agents(agents: &mut Vec<TaskAgent>) -> bool {
    if agents.len() <= TASK_AGENTS_MAX {
        return false;
    }
    let mut keep = vec![false; agents.len()];
    let mut room = TASK_AGENTS_MAX;
    for pass in 0..2 {
        for (i, a) in agents.iter().enumerate() {
            if room == 0 {
                break;
            }
            let urgent = matches!(a.state, TaskAgentState::Running | TaskAgentState::Failed);
            if !keep[i] && (urgent == (pass == 0)) {
                keep[i] = true;
                room -= 1;
            }
        }
    }
    let mut i = 0;
    agents.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    true
}

/// 发送 / 存储前限制单个事件的大小。返回是否改动过。
/// `tool_result.output` 被截断时，丢掉的字节数填进 `detail.output_truncated_bytes`（`detail` 为空就新建）。
pub fn limit_event(ev: &mut Event) -> bool {
    match ev {
        Event::Task { title, last_tool, summary, phases, agents, .. } => {
            let mut changed = cut_chars(title, TASK_LABEL_MAX_CHARS);
            changed |= cut_chars_opt(last_tool, TASK_LABEL_MAX_CHARS);
            changed |= cut_bytes_opt(summary, TASK_TEXT_MAX);
            if phases.len() > TASK_AGENTS_MAX {
                phases.truncate(TASK_AGENTS_MAX);
                changed = true;
            }
            for p in phases.iter_mut() {
                changed |= cut_chars(&mut p.title, TASK_LABEL_MAX_CHARS);
            }
            changed |= limit_task_agents(agents);
            for a in agents.iter_mut() {
                changed |= cut_chars(&mut a.label, TASK_LABEL_MAX_CHARS);
                changed |= cut_chars_opt(&mut a.last_tool, TASK_LABEL_MAX_CHARS);
                changed |= cut_bytes_opt(&mut a.error, TASK_TEXT_MAX);
            }
            changed
        }
        Event::Notice { message, .. } => match truncate_tool_text(message) {
            Some(t) => {
                *message = t;
                true
            }
            None => false,
        },
        Event::ToolResult { output, detail, .. } => match limit_value_counted(output) {
            Some(n) => {
                detail.get_or_insert_with(Default::default).output_truncated_bytes = Some(n);
                true
            }
            None => false,
        },
        Event::ToolCall { input, .. } => limit_value(input),
        _ => false,
    }
}

/// 一个事件 JSON 编码后的字节数（历史分页用）。
pub fn event_bytes(ev: &SessionEvent) -> usize {
    serde_json::to_vec(ev).map(|v| v.len()).unwrap_or(0)
}

/// 按字节上限截一页历史。`events` 按 seq 升序。
/// `keep_newest = false`（向后翻页，`after_seq`）保留前缀；`true`（`before_seq`，取最近的）保留后缀。
/// 至少留一个事件（单个事件超限时单独成页）。返回 (页, 是否被截掉了一部分)。
pub fn cap_page(events: Vec<SessionEvent>, max_bytes: usize, keep_newest: bool) -> (Vec<SessionEvent>, bool) {
    let sizes: Vec<usize> = events.iter().map(event_bytes).collect();
    let n = events.len();
    let mut total = 0usize;
    let mut kept = 0usize;
    let order: Box<dyn Iterator<Item = usize>> = if keep_newest { Box::new((0..n).rev()) } else { Box::new(0..n) };
    for i in order {
        if kept > 0 && total + sizes[i] > max_bytes {
            break;
        }
        total += sizes[i];
        kept += 1;
    }
    if kept == n {
        return (events, false);
    }
    let page =
        if keep_newest { events.into_iter().skip(n - kept).collect() } else { events.into_iter().take(kept).collect() };
    (page, true)
}

/// 单个工具调用的 `tool_output_delta` 转发预算。
#[derive(Debug, Default)]
pub struct DeltaBudget {
    sent: usize,
    cut: bool,
}

impl DeltaBudget {
    /// 这次调用的流式输出已经因为超过上限而停止转发（最终 `tool_result.detail.live_output_truncated` 要置 `true`）。
    pub fn is_cut(&self) -> bool {
        self.cut
    }
}

/// 预算判决。
#[derive(Debug, PartialEq, Eq)]
pub enum DeltaVerdict {
    /// 照常转发（文本可能被截到预算边界）。
    Forward(String),
    /// 这一段把预算用完了：转发 `Forward` 里的前缀，再补一条截断提示。
    ForwardLast(String),
    /// 已经超限，丢弃。
    Drop,
}

impl DeltaBudget {
    pub fn offer(&mut self, text: &str) -> DeltaVerdict {
        if self.cut {
            return DeltaVerdict::Drop;
        }
        let room = TOOL_DELTA_MAX - self.sent;
        if text.len() <= room {
            self.sent += text.len();
            return DeltaVerdict::Forward(text.to_string());
        }
        self.cut = true;
        self.sent = TOOL_DELTA_MAX;
        DeltaVerdict::ForwardLast(prefix_at(text, room).to_string())
    }
}

/// 流式输出被截断时补在输出流里的一行提示（结构化的标记在最终 `tool_result.detail.live_output_truncated`）。
pub const DELTA_CUT_NOTICE: &str = "\n…[live output truncated; the final result keeps the head and tail]\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, text: &str) -> SessionEvent {
        SessionEvent {
            session_id: "s".into(),
            seq,
            ts_ms: seq,
            turn_id: None,
            event: Event::TextDelta { text: text.into() },
        }
    }

    #[test]
    fn middle_truncation_keeps_head_and_tail_on_char_boundaries() {
        assert!(truncate_tool_text(&"a".repeat(TOOL_OUTPUT_MAX)).is_none());
        // 截过的文本再截一次是空操作
        let once = truncate_tool_text(&"a".repeat(500_000)).unwrap();
        assert!(truncate_tool_text(&once).is_none());
        // 字节数：提示行里的 N 就是去掉的字节数
        let (_, removed) =
            truncate_middle_counted(&"a".repeat(500_000), TOOL_OUTPUT_MAX, TOOL_OUTPUT_HEAD, TOOL_OUTPUT_TAIL).unwrap();
        assert_eq!(removed, 500_000 - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL);
        let s = format!("BIGSTART{}BIGEND", "x".repeat(300_000));
        let t = truncate_tool_text(&s).unwrap();
        assert!(t.starts_with("BIGSTART") && t.ends_with("BIGEND"));
        assert!(t.len() < TOOL_OUTPUT_MAX + 64, "{}", t.len());
        let removed = s.len() - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL;
        assert!(t.contains(&format!("…[truncated {removed} bytes]")), "marker with the removed byte count");
        // 多字节字符不会被劈开
        let cjk = "汉".repeat(40_000); // 120_000 字节
        let t = truncate_tool_text(&cjk).unwrap();
        assert!(t.is_char_boundary(t.len()) && t.chars().all(|c| c == '汉' || c == '\n' || c.is_ascii() || c == '…'));
    }

    #[test]
    fn a_truncated_tool_result_records_how_many_bytes_were_dropped_in_its_detail() {
        use anycon_protocol::ToolResultDetail;
        let mk = |output: Value, detail: Option<ToolResultDetail>| Event::ToolResult {
            id: "t".into(),
            output,
            is_error: false,
            detail,
        };
        // detail 为空：新建并填
        let mut ev = mk(Value::String("x".repeat(300_000)), None);
        assert!(limit_event(&mut ev));
        let Event::ToolResult { output, detail, .. } = &ev else { unreachable!() };
        let n = detail.as_ref().unwrap().output_truncated_bytes.unwrap();
        assert_eq!(n as usize, 300_000 - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL);
        assert!(
            output.as_str().unwrap().contains(&format!("…[truncated {n} bytes]")),
            "the marker and the field agree"
        );
        // 已有 detail：其它字段保留
        let keep = ToolResultDetail { exit_code: Some(3), ..Default::default() };
        let mut ev = mk(Value::String("y".repeat(300_000)), Some(keep));
        assert!(limit_event(&mut ev));
        let Event::ToolResult { detail, .. } = &ev else { unreachable!() };
        let d = detail.as_ref().unwrap();
        assert_eq!((d.exit_code, d.output_truncated_bytes.is_some(), d.live_output_truncated), (Some(3), true, None));
        // 再过一遍（读历史）是空操作，字段不变
        let before = ev.clone();
        assert!(!limit_event(&mut ev));
        assert_eq!(ev, before);
        // 没超限：不动 detail
        let mut small = mk(Value::String("ok".into()), None);
        assert!(!limit_event(&mut small));
        let Event::ToolResult { detail, .. } = &small else { unreachable!() };
        assert!(detail.is_none());
        // 对象形的输出也记字节数
        let mut obj = mk(serde_json::json!({ "content": "z".repeat(500_000) }), None);
        assert!(limit_event(&mut obj));
        let Event::ToolResult { detail, .. } = &obj else { unreachable!() };
        assert!(detail.as_ref().unwrap().output_truncated_bytes.unwrap() > 400_000);
    }

    #[test]
    fn json_values_are_bounded() {
        let mut v = Value::String("y".repeat(200_000));
        assert!(limit_value(&mut v));
        assert!(v.as_str().unwrap().len() < TOOL_OUTPUT_MAX + 64);
        // 对象里的长字符串叶子
        let mut v = serde_json::json!({ "content": "z".repeat(500_000), "path": "/a/b" });
        assert!(limit_value(&mut v));
        assert_eq!(v["path"], "/a/b");
        assert!(serde_json::to_string(&v).unwrap().len() < TOOL_OUTPUT_MAX);
        // 很多小叶子：仍然是数组（保留结构），只留前面的项和一条标记
        let mut v = Value::Array((0..30_000).map(|i| Value::String(format!("item-{i}"))).collect());
        assert!(limit_value(&mut v));
        let a = v.as_array().expect("still an array");
        assert_eq!(a[0], "item-0");
        assert!(a.last().unwrap().as_str().unwrap().starts_with("…[truncated "), "{:?}", a.last());
        assert!(serde_json::to_string(&v).unwrap().len() <= TOOL_OUTPUT_MAX);
        // 再过一遍是空操作
        assert!(!limit_value(&mut v));
        // 小的原样
        let mut v = serde_json::json!({ "a": 1 });
        assert!(!limit_value(&mut v));
    }

    #[test]
    fn object_inputs_keep_their_shape_and_only_long_strings_inside_are_cut() {
        // Write 工具：路径 + 一个很长的文件内容。客户端按 `file_path` / `content` 渲染 diff，类型不能变
        let content = format!("HEAD{}TAIL", "w".repeat(300_000));
        let mut ev = Event::ToolCall {
            id: "t".into(),
            name: "Write".into(),
            category: anycon_protocol::ToolCategory::FileEdit,
            title: "write".into(),
            input: serde_json::json!({ "file_path": "/a/b.rs", "content": content, "flags": { "force": true } }),
            detail: None,
            parent_id: None,
        };
        assert!(limit_event(&mut ev));
        let Event::ToolCall { input, .. } = &ev else { unreachable!() };
        assert!(input.is_object(), "an object stays an object");
        assert_eq!(input["file_path"], "/a/b.rs");
        assert_eq!(input["flags"]["force"], true);
        let c = input["content"].as_str().unwrap();
        assert!(
            c.starts_with("HEAD") && c.ends_with("TAIL") && c.contains("…[truncated "),
            "{}",
            &c[..40.min(c.len())]
        );
        assert!(serde_json::to_string(input).unwrap().len() <= TOOL_OUTPUT_MAX);
        // 很多个中等大小的字符串：叶子要缩得更小才装得下，结构（键）仍在
        let mut v = Value::Object((0..40).map(|i| (format!("k{i:02}"), Value::String("m".repeat(10_000)))).collect());
        assert!(limit_value(&mut v));
        let o = v.as_object().expect("still an object");
        assert_eq!(o.len(), 40);
        assert!(o.values().all(|x| x.as_str().unwrap().len() < 2_000));
        assert!(serde_json::to_string(&v).unwrap().len() <= TOOL_OUTPUT_MAX);
        // 极端：深层嵌套的大数组也不会退回字符串
        let mut v =
            serde_json::json!({ "rows": (0..50_000).map(|i| serde_json::json!({ "n": i })).collect::<Vec<_>>() });
        assert!(limit_value(&mut v));
        assert!(v["rows"].is_array() && v["rows"][0]["n"] == 0);
        assert!(serde_json::to_string(&v).unwrap().len() <= TOOL_OUTPUT_MAX);
    }

    fn task_agent(index: u32, label: &str, state: TaskAgentState) -> TaskAgent {
        TaskAgent {
            index,
            label: label.into(),
            state,
            phase_index: None,
            model: None,
            tokens: None,
            tool_calls: None,
            duration_ms: None,
            last_tool: None,
            error: None,
            attempt: None,
            cached: None,
        }
    }

    fn task(agents: Vec<TaskAgent>) -> Event {
        Event::Task {
            id: "t".into(),
            kind: anycon_protocol::TaskKind::Workflow,
            tool_id: None,
            title: "wf".into(),
            status: anycon_protocol::TaskStatus::Running,
            usage: None,
            last_tool: None,
            summary: None,
            phases: vec![],
            agents,
        }
    }

    #[test]
    fn a_task_snapshot_is_bounded() {
        // 小的原样
        let mut small = task(vec![task_agent(0, "a", TaskAgentState::Done)]);
        let before = small.clone();
        assert!(!limit_event(&mut small));
        assert_eq!(small, before);

        // 文本字段：label / title / last_tool 按字符，error / summary 按字节
        let long_cjk = "汉".repeat(500);
        let mut ev = task(vec![TaskAgent {
            last_tool: Some(long_cjk.clone()),
            error: Some("e".repeat(10_000)),
            ..task_agent(0, &long_cjk, TaskAgentState::Failed)
        }]);
        if let Event::Task { title, last_tool, summary, phases, .. } = &mut ev {
            *title = long_cjk.clone();
            *last_tool = Some(long_cjk.clone());
            *summary = Some("é".repeat(10_000));
            phases.push(anycon_protocol::TaskPhase { index: 0, title: long_cjk.clone() });
        }
        assert!(limit_event(&mut ev));
        let Event::Task { title, last_tool, summary, phases, agents, .. } = &ev else { unreachable!() };
        assert_eq!(title.chars().count(), TASK_LABEL_MAX_CHARS);
        assert!(title.ends_with('…'));
        assert_eq!(last_tool.as_ref().unwrap().chars().count(), TASK_LABEL_MAX_CHARS);
        assert_eq!(phases[0].title.chars().count(), TASK_LABEL_MAX_CHARS);
        assert!(summary.as_ref().unwrap().len() <= TASK_TEXT_MAX, "multi-byte text is cut on a char boundary");
        assert_eq!(agents[0].label.chars().count(), TASK_LABEL_MAX_CHARS);
        assert_eq!(agents[0].last_tool.as_ref().unwrap().chars().count(), TASK_LABEL_MAX_CHARS);
        let err = agents[0].error.as_ref().unwrap();
        assert!(err.len() <= TASK_TEXT_MAX && err.ends_with('…') && err.starts_with("eee"));
        // 再过一遍是空操作（读历史时会再过一遍）
        let before = ev.clone();
        assert!(!limit_event(&mut ev));
        assert_eq!(ev, before);
    }

    #[test]
    fn a_task_snapshot_keeps_at_most_500_agents_and_prefers_running_and_failed_ones() {
        // 600 个 agent：前 550 个 done，后 50 个 running / failed
        let mut agents: Vec<TaskAgent> = (0..550).map(|i| task_agent(i, "done", TaskAgentState::Done)).collect();
        agents.extend(
            (550..600).map(|i| {
                task_agent(i, "live", if i % 2 == 0 { TaskAgentState::Running } else { TaskAgentState::Failed })
            }),
        );
        let mut ev = task(agents);
        assert!(limit_event(&mut ev));
        let Event::Task { agents, .. } = &ev else { unreachable!() };
        assert_eq!(agents.len(), TASK_AGENTS_MAX);
        assert_eq!(agents.iter().filter(|a| a.label == "live").count(), 50, "running / failed ones are kept");
        assert!(agents.windows(2).all(|w| w[0].index < w[1].index), "original order is kept");
        assert_eq!(agents[0].index, 0);
        assert!(!limit_event(&mut ev));

        // 恰好 500 个：不动
        let mut ev = task((0..500).map(|i| task_agent(i, "a", TaskAgentState::Queued)).collect());
        assert!(!limit_event(&mut ev));

        // running / failed 本身就超过 500：留前 500 个
        let mut ev = task((0..700).map(|i| task_agent(i, "a", TaskAgentState::Running)).collect());
        assert!(limit_event(&mut ev));
        let Event::Task { agents, .. } = &ev else { unreachable!() };
        assert_eq!((agents.len(), agents[499].index), (500, 499));
    }

    #[test]
    fn a_notice_message_is_cut_like_tool_output() {
        let mk = |m: String| Event::Notice {
            code: "command_output".into(),
            level: anycon_protocol::NoticeLevel::Info,
            message: m,
            model_from: None,
            model_to: None,
            resets_at_ms: None,
        };
        let mut small = mk("ok\nline 2".into());
        assert!(!limit_event(&mut small));
        let mut big = mk(format!("HEAD{}TAIL", "n".repeat(300_000)));
        assert!(limit_event(&mut big));
        let Event::Notice { message, code, .. } = &big else { unreachable!() };
        assert_eq!(code, "command_output");
        assert!(message.starts_with("HEAD") && message.ends_with("TAIL") && message.contains("…[truncated "));
        assert!(message.len() < TOOL_OUTPUT_MAX + 64);
        assert!(!limit_event(&mut big), "cutting twice is a no-op");
    }

    #[test]
    fn pages_are_capped_by_bytes_and_a_big_event_stands_alone() {
        let small: Vec<SessionEvent> = (1..=10).map(|i| ev(i, &"a".repeat(50_000))).collect();
        let (page, cut) = cap_page(small.clone(), HISTORY_PAGE_MAX, false);
        assert!(cut);
        assert_eq!(page.first().unwrap().seq, 1);
        assert!(page.len() >= 3 && page.len() <= 4, "{}", page.len());
        assert!(page.iter().map(event_bytes).sum::<usize>() <= HISTORY_PAGE_MAX);
        // 向前翻（取最近的）：保留最新的
        let (page, cut) = cap_page(small.clone(), HISTORY_PAGE_MAX, true);
        assert!(cut);
        assert_eq!(page.last().unwrap().seq, 10);
        // 单个事件超过上限：仍然返回它（单独成页）
        let huge = vec![ev(1, &"b".repeat(400_000)), ev(2, "tail")];
        let (page, cut) = cap_page(huge, HISTORY_PAGE_MAX, false);
        assert!(cut);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].seq, 1);
        // 没超限：原样
        let (page, cut) = cap_page(vec![ev(1, "a"), ev(2, "b")], HISTORY_PAGE_MAX, false);
        assert!(!cut);
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn delta_budget_forwards_up_to_the_cap_then_drops() {
        let mut b = DeltaBudget::default();
        let chunk = "c".repeat(20_000);
        assert_eq!(b.offer(&chunk), DeltaVerdict::Forward(chunk.clone()));
        assert_eq!(b.offer(&chunk), DeltaVerdict::Forward(chunk.clone()));
        assert_eq!(b.offer(&chunk), DeltaVerdict::Forward(chunk.clone()));
        // 第四段只剩 65536 - 60000 = 5536 字节
        match b.offer(&chunk) {
            DeltaVerdict::ForwardLast(p) => assert_eq!(p.len(), TOOL_DELTA_MAX - 60_000),
            other => panic!("{other:?}"),
        }
        assert_eq!(b.offer("more"), DeltaVerdict::Drop);
        assert!(b.is_cut());
        assert!(!DeltaBudget::default().is_cut());
    }
}
