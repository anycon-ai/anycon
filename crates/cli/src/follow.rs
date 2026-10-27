//! 跟随一个会话的事件流：`attach`、`new`、`send` 共用。
//!
//! 先 `session.watch`，再按 seq 补历史（不丢不重），然后读通知。权限请求（`permission.request` 通知）在终端里交互式回答；
//! 别的设备先答复了（`permission.resolved`）就取消本地提示。

use std::io::Write;

use anycon_protocol::{
    methods, Empty, Event, PermissionKind, PermissionListResult, PermissionOptionKind, PermissionRequest,
    PermissionResolved, PermissionRespondParams, RpcNotification, SessionChanged, SessionEvent, SessionHistoryResult,
    SessionStatus, TurnDoneReason,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::conn::Conn;
use crate::error::{CliError, Result};
use crate::output::visible;
use crate::output::Out;
use crate::render::{
    assign_keys, auto_approvable_request, auto_approve_option, describe_question, describe_request, first_reject,
    options_line, parse_choice, PromptQueue, Renderer,
};

/// `--auto-approve` 启动时的警告。
pub const AUTO_APPROVE_WARNING: &str = "warning: --auto-approve is on: one-time tool permissions are approved without asking (dangerous). Mode changes, plans and questions still wait for you.";

pub struct FollowOpts {
    pub session_id: String,
    pub show_thinking: bool,
    pub auto_approve: bool,
    pub out: Out,
    /// 从这个 seq 之后开始（含历史回放）
    pub start_seq: u64,
    /// 等这条消息对应的 turn 结束就返回；`None` = 一直跟随直到 Ctrl+C
    pub wait_message: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FollowEnd {
    /// 等待的 turn 结束了
    TurnDone(TurnDoneReason),
    /// 会话进入终态（error / ended / interrupted）
    SessionEnded(SessionStatus),
    /// 会话被删除
    Deleted,
    /// 连接断开
    Disconnected,
}

enum Stage {
    Choose,
    /// 选了 needs_message 的选项，等一句话
    Message {
        option: usize,
    },
    /// 逐题回答
    Question {
        idx: usize,
        answers: serde_json::Map<String, Value>,
    },
}

struct Prompt {
    req: PermissionRequest,
    keys: Vec<char>,
    stage: Stage,
    /// 提示里有文字被截断时，这里是它的全文（按 `v` 打印）。
    full: Option<String>,
}

struct Follower<'a> {
    conn: &'a Conn,
    opts: FollowOpts,
    r: Renderer,
    last_seq: u64,
    saw_message: bool,
    queue: PromptQueue,
    prompt: Option<Prompt>,
    stdin_closed: bool,
}

fn spawn_stdin() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tx.send(l).is_err() {
                return;
            }
        }
    });
    rx
}

pub async fn follow(
    conn: &Conn,
    notes: &mut mpsc::UnboundedReceiver<RpcNotification>,
    opts: FollowOpts,
) -> Result<FollowEnd> {
    // 一开始就登记 SIGINT：回放历史期间收到 Ctrl+C 也只是断开，而不是被默认处理杀掉
    let mut sigint = crate::output::interrupt_signal();
    let interactive = !opts.out.json;
    let mut stdin_rx = if interactive { Some(spawn_stdin()) } else { None };
    let sid = opts.session_id.clone();
    let mut f = Follower {
        conn,
        r: Renderer::new(opts.out, opts.show_thinking),
        last_seq: opts.start_seq,
        saw_message: opts.wait_message.is_none(),
        queue: PromptQueue::default(),
        prompt: None,
        stdin_closed: false,
        opts,
    };
    // 一开始就警告（不是等到第一次自动答复才说）
    if f.opts.auto_approve {
        eprintln!("{}", f.opts.out.err_yellow(AUTO_APPROVE_WARNING));
    }
    // 订阅在先，补历史在后；通知里 seq 不大于 last_seq 的丢弃
    conn.call::<Empty>(methods::SESSION_WATCH, json!({ "session_id": sid })).await?;
    loop {
        let before = f.last_seq;
        let page: SessionHistoryResult = conn
            .call(methods::SESSION_HISTORY, json!({ "session_id": sid, "after_seq": f.last_seq, "limit": 500 }))
            .await?;
        for ev in &page.events {
            if let Some(end) = f.on_event(ev).await? {
                return Ok(end);
            }
        }
        if !page.has_more {
            break;
        }
        // 防死循环：节点说还有更多、页却没有带来新的事件
        if f.last_seq == before {
            return Err(CliError::general("the node reports more history but returned no new events; giving up"));
        }
    }
    let pending: PermissionListResult = conn.call(methods::PERMISSION_LIST, json!({ "session_id": sid })).await?;
    for req in pending.requests {
        f.on_request(req).await?;
    }
    f.pump_prompt().await?;

    loop {
        tokio::select! {
            n = notes.recv() => {
                let Some(n) = n else { return Ok(FollowEnd::Disconnected) };
                if let Some(end) = f.on_notification(n).await? {
                    return Ok(end);
                }
            }
            line = async {
                match stdin_rx.as_mut() {
                    Some(rx) if f.prompt.is_some() => rx.recv().await,
                    _ => std::future::pending().await,
                }
            } => {
                match line {
                    Some(l) => f.on_line(&l).await?,
                    None => {
                        f.stdin_closed = true;
                        f.prompt = None;
                        f.r.note("stdin closed: not prompting any more; answer with `anycon approve` / `anycon deny` (or another device)");
                        stdin_rx = None;
                    }
                }
            }
            _ = sigint.recv() => {
                return Err(CliError::interrupted());
            }
        }
    }
}

impl Follower<'_> {
    async fn on_notification(&mut self, n: RpcNotification) -> Result<Option<FollowEnd>> {
        let sid = self.opts.session_id.clone();
        match n.method.as_str() {
            methods::SESSION_EVENT => {
                if let Ok(ev) = serde_json::from_value::<SessionEvent>(n.params) {
                    if ev.session_id == sid {
                        return self.on_event(&ev).await;
                    }
                }
            }
            methods::PERMISSION_REQUEST => {
                if let Ok(req) = serde_json::from_value::<PermissionRequest>(n.params) {
                    if req.session_id == sid {
                        self.on_request(req).await?;
                        self.pump_prompt().await?;
                    }
                }
            }
            methods::PERMISSION_RESOLVED => {
                if let Ok(res) = serde_json::from_value::<PermissionResolved>(n.params) {
                    if res.session_id == sid {
                        self.on_resolved(&res.request_id, &res.by_device);
                        self.pump_prompt().await?;
                    }
                }
            }
            methods::SESSION_CHANGED => {
                if let Ok(c) = serde_json::from_value::<SessionChanged>(n.params) {
                    if c.session_id == sid && c.deleted {
                        return Ok(Some(FollowEnd::Deleted));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    async fn on_event(&mut self, ev: &SessionEvent) -> Result<Option<FollowEnd>> {
        if ev.seq <= self.last_seq {
            return Ok(None);
        }
        self.last_seq = ev.seq;
        if self.opts.out.json {
            self.opts.out.json_line(ev);
        } else {
            self.r.event(ev);
        }
        match &ev.event {
            Event::UserMessage { message_id, .. } => {
                if self.opts.wait_message.as_deref() == Some(message_id.as_str()) {
                    self.saw_message = true;
                }
            }
            Event::PermissionResolved { request_id, by_device, .. } => {
                self.on_resolved(request_id, by_device);
                self.pump_prompt().await?;
            }
            Event::TurnDone { reason, .. } if self.saw_message && self.opts.wait_message.is_some() => {
                return Ok(Some(FollowEnd::TurnDone(*reason)));
            }
            Event::Status { state, .. } if self.saw_message && self.opts.wait_message.is_some() => {
                if matches!(state, SessionStatus::Error | SessionStatus::Ended | SessionStatus::Interrupted) {
                    return Ok(Some(FollowEnd::SessionEnded(*state)));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn on_resolved(&mut self, request_id: &str, by_device: &str) {
        self.queue.drop_request(request_id);
        if self.prompt.as_ref().is_some_and(|p| p.req.request_id == request_id) {
            self.prompt = None;
            if !self.opts.out.json {
                self.r.note(&format!("[the request was answered by {}]", visible(by_device)));
            }
        }
    }

    async fn on_request(&mut self, req: PermissionRequest) -> Result<()> {
        if self.opts.auto_approve && auto_approvable_request(&req) {
            if !self.queue.seen.insert(req.request_id.clone()) {
                return Ok(());
            }
            return self.auto_answer(&req).await;
        }
        if self.opts.out.json {
            if self.opts.auto_approve && self.queue.seen.insert(req.request_id.clone()) {
                self.needs_you(&req);
            }
            return Ok(());
        }
        // 交互模式：普通权限、以及 `--auto-approve` 不替你决定的几种（切换模式 / 计划 / 提问）都在终端里问
        self.queue.push(req);
        Ok(())
    }

    fn needs_you(&mut self, req: &PermissionRequest) {
        let id = &req.request_id;
        self.r.note(&format!(
            "[{:?} request {id} needs a decision from you: `anycon approve {id}` / `anycon deny {id}`, or answer on another device]",
            req.kind
        ));
    }

    /// `--auto-approve`：只自动批准「允许这一次」的工具权限（`on_request` 已经筛过种类）；
    /// 没有 allow_once 选项就拒绝，不会升级成 allow_always。
    async fn auto_answer(&mut self, req: &PermissionRequest) -> Result<()> {
        let id = &req.request_id;
        let params = match auto_approve_option(req) {
            Some(o) => PermissionRespondParams {
                request_id: id.clone(),
                option_id: Some(o.id.clone()),
                message: None,
                answers: None,
            },
            None => {
                self.r.note(&format!(
                    "[request {id} has no one-time allow option; denied rather than widening the grant]"
                ));
                PermissionRespondParams {
                    request_id: id.clone(),
                    option_id: first_reject(req).map(|o| o.id.clone()),
                    message: Some("auto-approve only grants one-time permissions".into()),
                    answers: None,
                }
            }
        };
        self.respond(params).await
    }

    async fn respond(&mut self, params: PermissionRespondParams) -> Result<()> {
        match self.conn.call::<Empty>(methods::PERMISSION_RESPOND, &params).await {
            Ok(_) => Ok(()),
            Err(e) if e.code == "already_resolved" => {
                self.r.note("[the request was already answered elsewhere]");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 没有活动提示且队列非空时，开始下一个提示。
    async fn pump_prompt(&mut self) -> Result<()> {
        if self.prompt.is_some() || self.stdin_closed || self.opts.out.json {
            return Ok(());
        }
        let Some(req) = self.queue.queue.pop_front() else { return Ok(()) };
        // 来自 agent 的文字全部可见化，默认完整展示；被截断时 `v` 看全文（所以不分配给选项）
        let described = describe_request(&self.opts.out, &req);
        let keys = assign_keys(&req.options, if described.full.is_some() { &['v'] } else { &[] });
        eprintln!("{}", described.text);
        match req.kind {
            PermissionKind::Question if req.detail.questions.is_empty() => {
                // 没有问题可问：直接跳过，别卡在一个没有提示的状态里
                let reject = first_reject(&req).map(|o| o.id.clone());
                self.respond(PermissionRespondParams {
                    request_id: req.request_id.clone(),
                    option_id: reject,
                    message: Some("skipped (the request carried no questions)".into()),
                    answers: None,
                })
                .await?;
                return Box::pin(self.pump_prompt()).await;
            }
            PermissionKind::Question => {
                let stage = Stage::Question { idx: 0, answers: serde_json::Map::new() };
                self.prompt = Some(Prompt { req, keys, stage, full: described.full });
                self.print_question_prompt();
            }
            _ => {
                self.prompt = Some(Prompt { req, keys, stage: Stage::Choose, full: described.full });
                self.print_choose_prompt();
            }
        }
        Ok(())
    }

    fn print_choose_prompt(&self) {
        let Some(p) = &self.prompt else { return };
        let v = if p.full.is_some() { "  [v] view all" } else { "" };
        eprint!("    {}{v}  > ", options_line(&p.req.options, &p.keys));
        let _ = std::io::stderr().flush();
    }

    /// 打印当前这道题（题干和选项标签都经过可见化；题干超长才截断并留 `v`），然后是输入提示。
    fn print_question_prompt(&mut self) {
        let Some(p) = &mut self.prompt else { return };
        let Stage::Question { idx, .. } = &p.stage else { return };
        let Some(q) = p.req.detail.questions.get(*idx) else { return };
        let described = describe_question(q);
        eprintln!("{}", described.text);
        p.full = described.full;
        self.print_question_input_prompt();
    }

    fn print_question_input_prompt(&self) {
        let Some(p) = &self.prompt else { return };
        let Stage::Question { idx, .. } = &p.stage else { return };
        let Some(q) = p.req.detail.questions.get(*idx) else { return };
        let hint = match (q.choices.is_empty(), q.free_text, q.multi) {
            (true, _, _) => "type an answer",
            (false, true, true) => "numbers separated by commas, or type an answer",
            (false, true, false) => "a number, or type an answer",
            (false, false, true) => "numbers separated by commas",
            (false, false, false) => "a number",
        };
        let v = if p.full.is_some() { "; v views the whole question" } else { "" };
        // 可选的题：空行只是不答这一题；必答的题空行是跳过整个提问
        let empty = if q.optional { "optional, empty line leaves it blank" } else { "empty line skips" };
        eprint!("    ({hint}; {empty}{v}) > ");
        let _ = std::io::stderr().flush();
    }

    async fn on_line(&mut self, line: &str) -> Result<()> {
        let Some(mut p) = self.prompt.take() else { return Ok(()) };
        // 文字被截断时 `v` 打印全文（只在选择 / 答题阶段；追问「给 agent 的话」时 v 就是一句话）
        if let (Some(full), true) = (&p.full, line.trim().eq_ignore_ascii_case("v")) {
            if matches!(p.stage, Stage::Choose | Stage::Question { .. }) {
                eprintln!("{full}");
                let in_question = matches!(p.stage, Stage::Question { .. });
                self.prompt = Some(p);
                if in_question {
                    self.print_question_input_prompt();
                } else {
                    self.print_choose_prompt();
                }
                return Ok(());
            }
        }
        match std::mem::replace(&mut p.stage, Stage::Choose) {
            Stage::Choose => match parse_choice(line, &p.req.options, &p.keys) {
                None => {
                    eprint!("    please choose one of the options > ");
                    let _ = std::io::stderr().flush();
                    self.prompt = Some(p);
                }
                Some(i) if p.req.options[i].needs_message => {
                    eprint!("    message for the agent (optional, Enter to skip) > ");
                    let _ = std::io::stderr().flush();
                    p.stage = Stage::Message { option: i };
                    self.prompt = Some(p);
                }
                Some(i) => {
                    let id = p.req.options[i].id.clone();
                    self.respond(PermissionRespondParams {
                        request_id: p.req.request_id.clone(),
                        option_id: Some(id),
                        message: None,
                        answers: None,
                    })
                    .await?;
                    self.pump_prompt().await?;
                }
            },
            Stage::Message { option } => {
                let id = p.req.options[option].id.clone();
                let message = (!line.trim().is_empty()).then(|| line.trim().to_string());
                self.respond(PermissionRespondParams {
                    request_id: p.req.request_id.clone(),
                    option_id: Some(id),
                    message,
                    answers: None,
                })
                .await?;
                self.pump_prompt().await?;
            }
            Stage::Question { idx, mut answers } => {
                // 节点发来的问题列表可能是空的 / 下标越界：不 panic，当作跳过
                let Some(q) = p.req.detail.questions.get(idx).cloned() else {
                    let reject = first_reject(&p.req).map(|o| o.id.clone());
                    self.respond(PermissionRespondParams {
                        request_id: p.req.request_id.clone(),
                        option_id: reject,
                        message: Some("skipped".into()),
                        answers: None,
                    })
                    .await?;
                    return self.pump_prompt().await;
                };
                let t = line.trim();
                if t.is_empty() && !q.optional {
                    // 跳过整个提问
                    let reject = first_reject(&p.req).map(|o| o.id.clone());
                    self.respond(PermissionRespondParams {
                        request_id: p.req.request_id.clone(),
                        option_id: reject,
                        message: Some("skipped".into()),
                        answers: None,
                    })
                    .await?;
                    return self.pump_prompt().await;
                }
                let pick = |tok: &str| -> Option<String> {
                    let tok = tok.trim();
                    if let Ok(n) = tok.parse::<usize>() {
                        if n >= 1 && n <= q.choices.len() {
                            return Some(q.choices[n - 1].id.clone());
                        }
                    }
                    q.choices.iter().find(|c| c.id == tok || c.label.eq_ignore_ascii_case(tok)).map(|c| c.id.clone())
                };
                // 可选的题留空：不放进 answers（答复里没有这一题），接着问下一题
                if !t.is_empty() {
                    let value = if q.multi && !q.choices.is_empty() {
                        let picked: Vec<Value> = t.split(',').filter_map(&pick).map(Value::String).collect();
                        if picked.is_empty() && q.free_text {
                            Value::String(t.to_string())
                        } else {
                            Value::Array(picked)
                        }
                    } else {
                        match pick(t) {
                            Some(id) => Value::String(id),
                            None if q.free_text || q.choices.is_empty() => Value::String(t.to_string()),
                            None => {
                                eprint!("    please pick one of the listed choices > ");
                                let _ = std::io::stderr().flush();
                                p.stage = Stage::Question { idx, answers };
                                self.prompt = Some(p);
                                return Ok(());
                            }
                        }
                    };
                    answers.insert(q.id.clone(), value);
                }
                if idx + 1 < p.req.detail.questions.len() {
                    p.stage = Stage::Question { idx: idx + 1, answers };
                    self.prompt = Some(p);
                    self.print_question_prompt();
                } else {
                    let option = p
                        .req
                        .options
                        .iter()
                        .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways))
                        .map(|o| o.id.clone());
                    self.respond(PermissionRespondParams {
                        request_id: p.req.request_id.clone(),
                        option_id: option,
                        message: None,
                        answers: Some(answers),
                    })
                    .await?;
                    self.pump_prompt().await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anycon_protocol::{
        PermissionDetail, PermissionOption, Question, QuestionChoice, ToolCallDetail, ToolCategory,
        ToolPermissionDetail,
    };

    use super::*;
    use crate::conn::Req;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// 一个假的 daemon 连接：记下每个请求，一律回 `{}`。
    fn fake_conn() -> (Conn, Calls) {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Req>();
        let (_note_tx, note_rx) = mpsc::unbounded_channel();
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        tokio::spawn(async move {
            while let Some(r) = req_rx.recv().await {
                seen.lock().unwrap().push((r.method.clone(), r.params.clone()));
                let _ = r.reply.send(Ok(json!({})));
            }
        });
        (Conn::from_parts(req_tx, note_rx, false, None, Arc::new(Mutex::new(None))), calls)
    }

    fn follower(conn: &Conn) -> Follower<'_> {
        let out = Out { json: false, color: false, color_err: false };
        Follower {
            conn,
            opts: FollowOpts {
                session_id: "s".into(),
                show_thinking: false,
                auto_approve: false,
                out,
                start_seq: 0,
                wait_message: None,
            },
            r: Renderer::new(out, false),
            last_seq: 0,
            saw_message: true,
            queue: PromptQueue::default(),
            prompt: None,
            stdin_closed: false,
        }
    }

    fn option(id: &str, label: &str, kind: PermissionOptionKind, needs_message: bool) -> PermissionOption {
        PermissionOption { id: id.into(), label: label.into(), kind, needs_message }
    }

    fn tool_request(command: &str) -> PermissionRequest {
        PermissionRequest {
            session_id: "s".into(),
            request_id: "r1".into(),
            kind: PermissionKind::Tool,
            title: "Run".into(),
            detail: PermissionDetail {
                tool: Some(ToolPermissionDetail {
                    tool: "Bash".into(),
                    category: ToolCategory::Shell,
                    input: json!({ "command": command }),
                    cwd: None,
                    detail: Some(ToolCallDetail { command: Some(command.into()), ..Default::default() }),
                }),
                ..Default::default()
            },
            options: vec![
                option("allow", "Allow", PermissionOptionKind::AllowOnce, false),
                option("deny", "Deny", PermissionOptionKind::RejectOnce, false),
            ],
            created_at_ms: 0,
            expires_at_ms: None,
        }
    }

    fn responds(calls: &Calls) -> Vec<Value> {
        calls.lock().unwrap().iter().filter(|(m, _)| m == methods::PERMISSION_RESPOND).map(|(_, p)| p.clone()).collect()
    }

    #[tokio::test]
    async fn v_shows_the_whole_clipped_request_without_answering_it_and_is_never_an_option_key() {
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        f.on_request(tool_request(&format!("{}; rm -rf /", "x".repeat(2500)))).await.unwrap();
        f.pump_prompt().await.unwrap();
        let p = f.prompt.as_ref().expect("the prompt is up");
        assert!(p.full.is_some(), "the command was clipped, so there is a full view");
        assert!(!p.keys.contains(&'v'), "v is reserved for viewing: {:?}", p.keys);
        // v：打印全文，提示还在，没有答复
        f.on_line("v").await.unwrap();
        assert!(f.prompt.is_some() && responds(&calls).is_empty());
        f.on_line(" V ").await.unwrap();
        assert!(f.prompt.is_some() && responds(&calls).is_empty());
        // 之后照常选择
        f.on_line("a").await.unwrap();
        assert!(f.prompt.is_none());
        assert_eq!(responds(&calls), vec![json!({ "request_id": "r1", "option_id": "allow" })]);
    }

    #[tokio::test]
    async fn without_clipping_v_is_just_an_unknown_choice() {
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        f.on_request(tool_request("echo hi")).await.unwrap();
        f.pump_prompt().await.unwrap();
        assert!(f.prompt.as_ref().unwrap().full.is_none());
        f.on_line("v").await.unwrap();
        assert!(f.prompt.is_some() && responds(&calls).is_empty(), "re-asked, nothing answered");
        f.on_line("d").await.unwrap();
        assert_eq!(responds(&calls)[0]["option_id"], "deny");
    }

    fn question_request(text: &str, questions: Vec<Question>) -> PermissionRequest {
        PermissionRequest {
            session_id: "s".into(),
            request_id: "q1".into(),
            kind: PermissionKind::Question,
            title: "Question".into(),
            detail: PermissionDetail { questions, text: Some(text.into()), ..Default::default() },
            options: vec![
                option("ok", "Submit", PermissionOptionKind::AllowOnce, false),
                option("skip", "Skip", PermissionOptionKind::RejectOnce, false),
            ],
            created_at_ms: 0,
            expires_at_ms: None,
        }
    }

    #[tokio::test]
    async fn question_text_with_control_characters_or_a_huge_body_is_handled_and_v_views_it() {
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        let q = Question {
            id: "a".into(),
            text: format!("Proceed?\x1b[2K\r{}", "z".repeat(2600)),
            choices: vec![QuestionChoice { id: "yes".into(), label: "Yes\x1b[0m".into() }],
            multi: false,
            free_text: true,
            optional: false,
        };
        f.on_request(question_request("body", vec![q])).await.unwrap();
        f.pump_prompt().await.unwrap();
        assert!(f.prompt.as_ref().unwrap().full.is_some(), "the long question text is clipped");
        f.on_line("v").await.unwrap();
        assert!(f.prompt.is_some() && responds(&calls).is_empty(), "v only views");
        // 选第 1 项 -> 答案是 choice id，提交到「允许」类选项
        f.on_line("1").await.unwrap();
        assert!(f.prompt.is_none());
        let r = &responds(&calls)[0];
        assert_eq!((r["request_id"].as_str(), r["option_id"].as_str()), (Some("q1"), Some("ok")));
        assert_eq!(r["answers"], json!({ "a": "yes" }));
    }

    fn text_question(id: &str, optional: bool) -> Question {
        Question {
            id: id.into(),
            text: format!("Value of {id}?"),
            choices: vec![],
            multi: false,
            free_text: true,
            optional,
        }
    }

    #[tokio::test]
    async fn an_empty_line_leaves_an_optional_question_blank_and_moves_on_but_still_skips_a_required_one() {
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        // 必答 -> 可选（留空）-> 可选（回答）：答复里只有回答了的两题
        f.on_request(question_request(
            "form",
            vec![text_question("name", false), text_question("nick", true), text_question("note", true)],
        ))
        .await
        .unwrap();
        f.pump_prompt().await.unwrap();
        f.on_line("Ada").await.unwrap();
        f.on_line("").await.unwrap();
        assert!(f.prompt.is_some() && responds(&calls).is_empty(), "the blank optional question does not end the form");
        f.on_line("hello").await.unwrap();
        assert!(f.prompt.is_none());
        let r = &responds(&calls)[0];
        assert_eq!((r["request_id"].as_str(), r["option_id"].as_str()), (Some("q1"), Some("ok")));
        assert_eq!(r["answers"], json!({ "name": "Ada", "note": "hello" }));

        // 最后一题是可选的、留空：直接提交已有的回答
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        f.on_request(question_request("form", vec![text_question("name", false), text_question("nick", true)]))
            .await
            .unwrap();
        f.pump_prompt().await.unwrap();
        f.on_line("Ada").await.unwrap();
        f.on_line("  ").await.unwrap();
        assert!(f.prompt.is_none());
        assert_eq!(responds(&calls)[0]["answers"], json!({ "name": "Ada" }));

        // 必答的题留空：跳过整个提问（走「拒绝」类选项）
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        f.on_request(question_request("form", vec![text_question("name", false), text_question("nick", true)]))
            .await
            .unwrap();
        f.pump_prompt().await.unwrap();
        f.on_line("").await.unwrap();
        assert!(f.prompt.is_none());
        let r = &responds(&calls)[0];
        assert_eq!(r["option_id"], "skip");
        assert!(r.get("answers").is_none());
    }

    #[tokio::test]
    async fn an_empty_question_list_or_an_out_of_range_index_never_panics() {
        let (conn, calls) = fake_conn();
        let mut f = follower(&conn);
        f.on_request(question_request("nothing to ask", vec![])).await.unwrap();
        f.pump_prompt().await.unwrap();
        assert!(f.prompt.is_none(), "skipped instead of waiting on a prompt that has nothing to ask");
        let r = responds(&calls);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["option_id"], "skip");
    }
}
