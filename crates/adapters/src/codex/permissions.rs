//! app-server 的服务端请求 ↔ [`PendingPermission`] / [`PermissionDecision`]（docs/spec/daemon.md § 9）。
//!
//! | 请求 | kind | 答复 |
//! |---|---|---|
//! | `item/commandExecution/requestApproval` | `tool`（shell） | `{ decision: accept \| acceptForSession \| decline \| cancel }` |
//! | `item/fileChange/requestApproval` | `tool`（file_edit） | 同上 |
//! | `item/permissions/requestApproval` | `tool`（other） | `{ permissions: <请求的原样>, scope: turn \| session }`；拒绝 = `{ permissions: {} }` |
//! | `item/tool/requestUserInput` | `question` | `{ answers: { <题目 id>: { answers: [<所选 label>…] } } }` |
//!
//! 选项固定三项：`allow`（accept）、`allow_session`（acceptForSession）、`deny`（decline）。**会话取消时**未答复的回 `cancel`
//! （`decline` 会让 agent 接着干，`cancel` 同时中断 turn）。审批请求带的 `reason` 放在 `detail.tool.detail.description`
//! （web 的审批卡片在命令下面显示这一行），同时保留在 `input` 里。
//!
//! 不支持的服务端请求（MCP elicitation、动态工具、ChatGPT token 刷新、旧版 `applyPatchApproval` / `execCommandApproval`…）
//! 在 [`unsupported`] 里按 schema 回一个拒绝——**不能挂着不回，否则 turn 卡死**。

use anycon_agent_api::{AdapterError, PendingPermission, PermissionDecision};
use anycon_protocol::{
    PermissionDetail, PermissionKind, PermissionOption, PermissionOptionKind, Question, QuestionChoice, ToolCallDetail,
    ToolCategory, ToolPermissionDetail,
};
use serde_json::{json, Map, Value};

use super::rpc::{RpcError, INTERNAL_ERROR, METHOD_NOT_FOUND};
use super::tools::{self, ToolCallParts};
use super::wire::{CommandApproval, FileApproval, PermissionsApproval, UserInputRequest};

pub const OPT_ALLOW: &str = "allow";
pub const OPT_ALLOW_SESSION: &str = "allow_session";
pub const OPT_DENY: &str = "deny";
pub const OPT_ANSWER: &str = "answer";

pub const M_COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
pub const M_FILE_APPROVAL: &str = "item/fileChange/requestApproval";
pub const M_PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
pub const M_USER_INPUT: &str = "item/tool/requestUserInput";

/// 适配器自己记着的、回包时要用的原生上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct NativePending {
    /// 服务端请求的 JSON-RPC id，原样回传。
    pub rpc_id: Value,
    pub kind: NativeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeKind {
    Command,
    FileChange,
    /// 请求的权限轮廓（原样放回 `permissions`）。
    Permissions {
        requested: Value,
    },
    UserInput {
        question_ids: Vec<String>,
    },
}

fn option(id: &str, label: &str, kind: PermissionOptionKind) -> PermissionOption {
    PermissionOption { id: id.to_string(), label: label.to_string(), kind, needs_message: false }
}

fn tool_options() -> Vec<PermissionOption> {
    vec![
        option(OPT_ALLOW, "Allow", PermissionOptionKind::AllowOnce),
        option(OPT_ALLOW_SESSION, "Allow for this session", PermissionOptionKind::AllowAlways),
        option(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce),
    ]
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn with_reason(mut input: Value, reason: &Option<String>, extra: &[(&str, Option<&str>)]) -> Value {
    if !input.is_object() {
        input = json!({});
    }
    if let Some(r) = non_empty(reason) {
        input["reason"] = json!(r);
    }
    for (k, v) in extra {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            input[*k] = json!(v);
        }
    }
    input
}

fn tool_pending(
    request_id: &str,
    title: String,
    tool: &str,
    category: ToolCategory,
    input: Value,
    cwd: &str,
    detail: Option<ToolCallDetail>,
) -> PendingPermission {
    PendingPermission {
        request_id: request_id.to_string(),
        kind: PermissionKind::Tool,
        title,
        detail: PermissionDetail {
            tool: Some(ToolPermissionDetail {
                tool: tool.to_string(),
                category,
                input,
                cwd: Some(cwd.to_string()),
                detail,
            }),
            ..Default::default()
        },
        options: tool_options(),
        expires_in: None,
    }
}

/// `ToolCallDetail.description`：`reason`（审批请求为什么来）在最前面，后面是别的说明（文件清单、`grantRoot` 的提示…）。
/// web 的审批卡片在命令 / 路径下面显示这一段。没有任何内容就不动。
fn with_description(
    detail: Option<ToolCallDetail>,
    reason: &Option<String>,
    extra: &[String],
) -> Option<ToolCallDetail> {
    let mut d = detail.unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if let Some(r) = non_empty(reason) {
        parts.push(r);
    }
    // 工具调用本来就有的说明（多文件补丁的清单）紧跟在 reason 后面
    if let Some(existing) = d.description.take().filter(|e| !e.trim().is_empty()) {
        parts.push(existing);
    }
    parts.extend(extra.iter().filter(|e| !e.trim().is_empty()).cloned());
    d.description = (!parts.is_empty()).then(|| parts.join("\n\n"));
    if d == ToolCallDetail::default() {
        None
    } else {
        Some(d)
    }
}

/// `item/commandExecution/requestApproval`。`started` 是同一个 item 开始时记下的描述，只用来补请求里没有的字段
/// （cwd、`commandActions`…）；**请求自己带了非空的 `command` 就以它为准**——zsh-exec-bridge 的子命令审批（`approvalId` 非空）
/// 同一个 `itemId` 会有多次回调，每次的 `command` 是那条子命令，用户答复的也是那条，而不是整个脚本。
pub fn from_command(
    request_id: &str,
    rpc_id: Value,
    p: &CommandApproval,
    fallback_cwd: &str,
    started: Option<&ToolCallParts>,
) -> (PendingPermission, NativePending) {
    let started_cwd = started.and_then(|s| s.detail.as_ref()).and_then(|d| d.cwd.clone());
    let cwd = p.cwd.clone().filter(|c| !c.is_empty()).or(started_cwd).unwrap_or_else(|| fallback_cwd.to_string());
    let raw = p.command.clone().filter(|c| !c.trim().is_empty());
    let started_command = started.and_then(|s| s.detail.as_ref()).and_then(|d| d.command.clone());
    let shown = match &raw {
        Some(c) => Some(tools::unwrap_shell(c)),
        None => started_command.clone(),
    };
    // 请求的命令和 item 里记的不一样 = 这是脚本里的一条子命令
    let is_subcommand = p.approval_id.is_some() || matches!((&shown, &started_command), (Some(a), Some(b)) if a != b);

    let mut input = match started {
        Some(s) => s.input.clone(),
        None => json!({}),
    };
    if !input.is_object() {
        input = json!({});
    }
    if let Some(c) = &raw {
        input["command"] = json!(c);
    }
    input["cwd"] = json!(cwd);
    if let Some(a) = p.approval_id.as_deref().filter(|a| !a.is_empty()) {
        input["approvalId"] = json!(a);
    }
    let input = with_reason(input, &p.reason, &[]);
    let mut base = started.and_then(|s| s.detail.clone()).unwrap_or_default();
    base.command = shown.filter(|c| !c.trim().is_empty());
    base.cwd = Some(cwd.clone());

    let title = if p.kind.as_deref() == Some("writeStdin") {
        "Send input to a running command".to_string()
    } else if let Some(host) = p.network_approval_context.as_ref().and_then(|c| c.get("host")).and_then(Value::as_str) {
        format!("Allow network access to {host}")
    } else if is_subcommand {
        "Run sub-command".to_string()
    } else {
        "Run command".to_string()
    };
    let pending = tool_pending(
        request_id,
        title,
        "commandExecution",
        ToolCategory::Shell,
        input,
        &cwd,
        with_description(Some(base), &p.reason, &[]),
    );
    (pending, NativePending { rpc_id, kind: NativeKind::Command })
}

/// `item/fileChange/requestApproval`。补丁内容在同一个 item 开始时的 `changes` 里（请求本身只有 `itemId`、`reason`、`grantRoot`）。
///
/// - 路径：cwd 之外的保持绝对路径；多文件补丁的标题写清文件数和前几个名字，`detail.description` 里按 reason → 全部路径 →
///   `grantRoot` 的顺序列出。
/// - `grantRoot` 非空 = 同时要求本会话写这个根目录：写进标题和说明。
/// - 拿不到 `item/started`（先到的是请求）：标题和说明都醒目地写「补丁内容未知」，`input.contentUnknown = true`。
pub fn from_file_change(
    request_id: &str,
    rpc_id: Value,
    p: &FileApproval,
    fallback_cwd: &str,
    started: Option<&ToolCallParts>,
) -> (PendingPermission, NativePending) {
    let grant = p.grant_root.as_deref().map(str::trim).filter(|g| !g.is_empty());
    let grant_title = grant.map(|g| format!(" and allow writing under {g} for this session")).unwrap_or_default();
    let grant_note =
        grant.map(|g| format!("This also grants write access under {g} for the rest of the session.")).into_iter();
    let (title, input, detail, extra) = match started {
        Some(s) => (
            format!("{}{grant_title}", s.title),
            with_reason(s.input.clone(), &p.reason, &[("grantRoot", grant)]),
            s.detail.clone(),
            grant_note.collect::<Vec<_>>(),
        ),
        None => {
            let mut input = with_reason(json!({}), &p.reason, &[("grantRoot", grant)]);
            input["contentUnknown"] = json!(true);
            let mut extra = vec![
                "The patch contents are unknown: this approval request arrived without the matching item, so the files it changes cannot be shown. Review the session before allowing it."
                    .to_string(),
            ];
            extra.extend(grant_note);
            (format!("Edit files (patch contents unknown){grant_title}"), input, None, extra)
        }
    };
    let pending = tool_pending(
        request_id,
        title,
        "fileChange",
        ToolCategory::FileEdit,
        input,
        fallback_cwd,
        with_description(detail, &p.reason, &extra),
    );
    (pending, NativePending { rpc_id, kind: NativeKind::FileChange })
}

/// 请求的权限轮廓的一句话摘要，做标题用。
fn permissions_summary(permissions: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if permissions.pointer("/network/enabled").and_then(Value::as_bool) == Some(true) {
        parts.push("network access".to_string());
    }
    let paths = |key: &str| -> Vec<String> {
        permissions
            .pointer(&format!("/fileSystem/{key}"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let (read, write) = (paths("read"), paths("write"));
    if !read.is_empty() {
        parts.push(format!("read {}", read.join(", ")));
    }
    if !write.is_empty() {
        parts.push(format!("write {}", write.join(", ")));
    }
    if parts.is_empty() {
        "Grant additional permissions".to_string()
    } else {
        tools::first_line_clipped(&format!("Grant {}", parts.join("; ")), 120)
    }
}

/// `item/permissions/requestApproval`（agent 主动申请额外的文件系统 / 网络权限）。
pub fn from_permissions(
    request_id: &str,
    rpc_id: Value,
    p: &PermissionsApproval,
    fallback_cwd: &str,
) -> (PendingPermission, NativePending) {
    let cwd = p.cwd.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| fallback_cwd.to_string());
    let input = with_reason(json!({ "permissions": p.permissions, "cwd": cwd }), &p.reason, &[]);
    let pending = tool_pending(
        request_id,
        permissions_summary(&p.permissions),
        "requestPermissions",
        ToolCategory::Other,
        input,
        &cwd,
        with_description(None, &p.reason, &[]),
    );
    (pending, NativePending { rpc_id, kind: NativeKind::Permissions { requested: p.permissions.clone() } })
}

/// `item/tool/requestUserInput` → `question`。选项的 id 就是 label（答复里要回 label）。
pub fn from_user_input(request_id: &str, rpc_id: Value, p: &UserInputRequest) -> (PendingPermission, NativePending) {
    let questions: Vec<Question> = p
        .questions
        .iter()
        .map(|q| Question {
            id: q.id.clone(),
            text: if q.header.trim().is_empty() || q.question.starts_with(q.header.as_str()) {
                q.question.clone()
            } else {
                format!("{}: {}", q.header.trim(), q.question)
            },
            choices: q
                .options
                .iter()
                .flatten()
                .map(|o| QuestionChoice {
                    id: o.label.clone(),
                    label: if o.description.trim().is_empty() {
                        o.label.clone()
                    } else {
                        format!("{} — {}", o.label, o.description.trim())
                    },
                })
                .collect(),
            multi: false,
            free_text: q.is_other || q.options.as_ref().is_none_or(|o| o.is_empty()),
        })
        .collect();
    let pending = PendingPermission {
        request_id: request_id.to_string(),
        kind: PermissionKind::Question,
        title: "Agent has a question".to_string(),
        detail: PermissionDetail { questions, ..Default::default() },
        options: vec![
            option(OPT_ANSWER, "Submit answers", PermissionOptionKind::AllowOnce),
            PermissionOption { needs_message: false, ..option(OPT_DENY, "Skip", PermissionOptionKind::RejectOnce) },
        ],
        expires_in: None,
    };
    let question_ids = p.questions.iter().map(|q| q.id.clone()).collect();
    (pending, NativePending { rpc_id, kind: NativeKind::UserInput { question_ids } })
}

fn answer_strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        Value::String(s) => vec![s.clone()],
        Value::Null => vec![],
        other => vec![other.to_string()],
    }
}

/// 客户端的答复 → 写回 app-server 的 `result`。`cancelling`：会话正在取消，未答复的一律 `cancel`。
pub fn build_response(
    native: &NativePending,
    decision: &PermissionDecision,
    cancelling: bool,
) -> Result<Value, AdapterError> {
    let option_id = decision.option_id.as_deref();
    match &native.kind {
        NativeKind::Command | NativeKind::FileChange => {
            let d = match (cancelling, option_id) {
                (true, _) => "cancel",
                (_, Some(OPT_ALLOW)) => "accept",
                (_, Some(OPT_ALLOW_SESSION)) => "acceptForSession",
                (_, Some(OPT_DENY) | None) => "decline",
                (_, Some(other)) => return Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
            };
            Ok(json!({ "decision": d }))
        }
        NativeKind::Permissions { requested } => match (cancelling, option_id) {
            (false, Some(OPT_ALLOW)) => Ok(json!({ "permissions": requested, "scope": "turn" })),
            (false, Some(OPT_ALLOW_SESSION)) => Ok(json!({ "permissions": requested, "scope": "session" })),
            (true, _) | (_, Some(OPT_DENY) | None) => Ok(json!({ "permissions": {}, "scope": "turn" })),
            (_, Some(other)) => Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
        },
        NativeKind::UserInput { question_ids } => {
            let answers = decision.answers.as_ref().filter(|a| !a.is_empty());
            match (cancelling, option_id, answers) {
                // 跳过（或取消）：空答案。选了 deny 就一定是跳过，即使请求里带着 answers（选项优先于内容）
                (true, _, _) | (_, Some(OPT_DENY), _) | (_, None, None) => Ok(json!({ "answers": {} })),
                (_, Some(OPT_ANSWER) | None, Some(answers)) => {
                    let mut out = Map::new();
                    for (qid, v) in answers {
                        if !question_ids.contains(qid) {
                            continue;
                        }
                        out.insert(qid.clone(), json!({ "answers": answer_strings(v) }));
                    }
                    Ok(json!({ "answers": Value::Object(out) }))
                }
                (_, Some(OPT_ANSWER), None) => {
                    Err(AdapterError::Protocol("question answered with option answer but no answers".to_string()))
                }
                (_, Some(other), _) => Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
            }
        }
    }
}

/// 不支持的服务端请求怎么答。
#[derive(Debug, Clone, PartialEq)]
pub struct UnsupportedReply {
    pub result: Result<Value, RpcError>,
    /// 给用户看的说明（作为一条 `error` 事件发出去）。
    pub message: String,
}

/// 对不认识 / 不支持的服务端请求按 schema 回拒绝。
pub fn unsupported(method: &str) -> UnsupportedReply {
    let msg =
        |what: &str| format!("Codex asked for {what}, which anycon does not support yet; the request was declined.");
    match method {
        "mcpServer/elicitation/request" => UnsupportedReply {
            result: Ok(json!({ "action": "decline" })),
            message: msg("input from an MCP server (elicitation)"),
        },
        "item/tool/call" => UnsupportedReply {
            result: Ok(json!({
                "contentItems": [{ "type": "inputText", "text": "This client does not support dynamic tool calls." }],
                "success": false
            })),
            message: msg("a client-side dynamic tool call"),
        },
        "applyPatchApproval" | "execCommandApproval" => UnsupportedReply {
            result: Ok(json!({ "decision": "abort" })),
            message: msg("an approval through the legacy approval API"),
        },
        "account/chatgptAuthTokens/refresh" => UnsupportedReply {
            result: Err(RpcError::new(INTERNAL_ERROR, "anycon does not manage ChatGPT tokens; run `codex login` on the machine")),
            message: "Codex asked the client to refresh its ChatGPT token, which anycon cannot do; run `codex login` on the machine."
                .to_string(),
        },
        "attestation/generate" => UnsupportedReply {
            result: Err(RpcError::new(INTERNAL_ERROR, "anycon does not provide attestation")),
            message: msg("a client attestation"),
        },
        other => UnsupportedReply {
            result: Err(RpcError::new(METHOD_NOT_FOUND, format!("method not supported: {other}"))),
            message: msg(other),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd_params(v: Value) -> CommandApproval {
        serde_json::from_value(v).unwrap()
    }

    fn tool_of(p: &PendingPermission) -> &ToolPermissionDetail {
        p.detail.tool.as_ref().expect("tool detail")
    }

    #[test]
    fn command_approval_carries_the_reason_as_description() {
        let p = cmd_params(json!({
            "kind": "command", "itemId": "c1", "command": "/bin/zsh -lc 'rm -rf build'", "cwd": "/work/proj",
            "reason": "Do you want to run this command outside the sandbox?"
        }));
        let (pending, native) = from_command("req-1", json!(7), &p, "/fallback", None);
        assert_eq!(pending.kind, PermissionKind::Tool);
        assert_eq!(pending.title, "Run command");
        let t = tool_of(&pending);
        assert_eq!((t.tool.as_str(), t.category), ("commandExecution", ToolCategory::Shell));
        assert_eq!(t.cwd.as_deref(), Some("/work/proj"));
        let d = t.detail.as_ref().unwrap();
        assert_eq!(d.command.as_deref(), Some("rm -rf build"));
        assert_eq!(d.description.as_deref(), Some("Do you want to run this command outside the sandbox?"));
        assert_eq!(t.input["reason"], "Do you want to run this command outside the sandbox?");
        assert_eq!(t.input["command"], "/bin/zsh -lc 'rm -rf build'");
        assert_eq!(
            pending.options.iter().map(|o| (o.id.as_str(), o.kind)).collect::<Vec<_>>(),
            vec![
                ("allow", PermissionOptionKind::AllowOnce),
                ("allow_session", PermissionOptionKind::AllowAlways),
                ("deny", PermissionOptionKind::RejectOnce)
            ]
        );
        assert_eq!(native.rpc_id, json!(7));
        // 没有 reason：没有 description
        let p = cmd_params(json!({"itemId": "c1", "command": "ls", "cwd": "/w"}));
        let (pending, _) = from_command("r", json!(1), &p, "/fallback", None);
        assert_eq!(tool_of(&pending).detail.as_ref().unwrap().description, None);
        assert!(tool_of(&pending).input.get("reason").is_none());
        // 空白 reason 同样当没有
        let p = cmd_params(json!({"itemId": "c1", "command": "ls", "reason": "   "}));
        let (pending, _) = from_command("r", json!(1), &p, "/fallback", None);
        assert_eq!(tool_of(&pending).detail.as_ref().unwrap().description, None);
        assert_eq!(tool_of(&pending).cwd.as_deref(), Some("/fallback"));
    }

    #[test]
    fn command_approval_reuses_the_started_item() {
        let started = ToolCallParts {
            name: "commandExecution".into(),
            category: ToolCategory::Shell,
            title: "echo hi".into(),
            input: json!({"command": "/bin/zsh -lc 'echo hi'", "cwd": "/w", "commandActions": []}),
            detail: Some(ToolCallDetail {
                command: Some("echo hi".into()),
                cwd: Some("/w".into()),
                ..Default::default()
            }),
        };
        let p = cmd_params(json!({"itemId": "c1", "command": "/bin/zsh -lc 'echo hi'", "reason": "needs network"}));
        let (pending, _) = from_command("r", json!(1), &p, "/w", Some(&started));
        let t = tool_of(&pending);
        assert_eq!(t.input["commandActions"], json!([]));
        assert_eq!(t.detail.as_ref().unwrap().command.as_deref(), Some("echo hi"));
        assert_eq!(t.detail.as_ref().unwrap().description.as_deref(), Some("needs network"));
    }

    #[test]
    fn network_and_stdin_approvals_get_their_own_titles() {
        let p = cmd_params(json!({
            "itemId": "c", "command": "curl x", "networkApprovalContext": {"host": "example.org", "protocol": "https"}
        }));
        assert_eq!(from_command("r", json!(1), &p, "/w", None).0.title, "Allow network access to example.org");
        let p = cmd_params(json!({"itemId": "c", "kind": "writeStdin"}));
        assert_eq!(from_command("r", json!(1), &p, "/w", None).0.title, "Send input to a running command");
    }

    fn started_patch(cwd: &str, changes: Value) -> ToolCallParts {
        let changes: Vec<crate::codex::wire::FileUpdateChange> = serde_json::from_value(changes).unwrap();
        ToolCallParts {
            name: "fileChange".into(),
            category: ToolCategory::FileEdit,
            title: tools::file_change_title(cwd, &changes),
            input: tools::file_change_input(&changes),
            detail: tools::file_change_detail(cwd, &changes),
        }
    }

    #[test]
    fn file_change_approval_uses_the_started_changes_and_carries_the_reason() {
        let started = started_patch("/w", json!([{"path": "/w/hello.txt", "kind": {"type": "add"}, "diff": "hi\n"}]));
        let p: FileApproval = serde_json::from_value(
            json!({"itemId": "f1", "reason": "write outside the workspace", "grantRoot": "/data"}),
        )
        .unwrap();
        let (pending, native) = from_file_change("r", json!(2), &p, "/w", Some(&started));
        // grantRoot 写进标题和说明（「并允许本会话写入 <root>」）
        assert_eq!(pending.title, "Create hello.txt and allow writing under /data for this session");
        let t = tool_of(&pending);
        assert_eq!((t.tool.as_str(), t.category), ("fileChange", ToolCategory::FileEdit));
        let d = t.detail.as_ref().unwrap();
        assert_eq!(d.path.as_deref(), Some("/w/hello.txt"));
        assert_eq!(
            d.description.as_deref(),
            Some(
                "write outside the workspace\n\nThis also grants write access under /data for the rest of the session."
            )
        );
        assert_eq!(t.input["changes"][0]["diff"], "hi\n");
        assert_eq!(t.input["reason"], "write outside the workspace");
        assert_eq!(t.input["grantRoot"], "/data");
        assert_eq!(native.kind, NativeKind::FileChange);
        // 没有 grantRoot / reason：标题和说明都干净
        let plain: FileApproval = serde_json::from_value(json!({"itemId": "f1"})).unwrap();
        let (pending, _) = from_file_change("r", json!(2), &plain, "/w", Some(&started));
        assert_eq!(pending.title, "Create hello.txt");
        assert_eq!(tool_of(&pending).detail.as_ref().unwrap().description, None);
    }

    #[test]
    fn patch_outside_the_project_shows_absolute_paths_and_multi_file_patches_list_everything() {
        let started = started_patch(
            "/w",
            json!([
                {"path": "/w/src/a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@\n"},
                {"path": "/home/me/.ssh/authorized_keys", "kind": {"type": "update", "move_path": null}, "diff": "@@\n"},
                {"path": "/w/new.txt", "kind": {"type": "add"}, "diff": "x\n"}
            ]),
        );
        let p: FileApproval =
            serde_json::from_value(json!({"itemId": "f", "reason": "needs to touch your ssh config"})).unwrap();
        let (pending, _) = from_file_change("r", json!(1), &p, "/w", Some(&started));
        // 标题：文件数 + 前几个名字，项目外的是绝对路径
        assert_eq!(pending.title, "Edit 3 files (src/a.rs, /home/me/.ssh/authorized_keys, new.txt)");
        let d = tool_of(&pending).detail.as_ref().unwrap();
        // reason 在最前面，然后是全部路径
        assert_eq!(
            d.description.as_deref(),
            Some("needs to touch your ssh config\n\n- edit src/a.rs\n- edit /home/me/.ssh/authorized_keys\n- create new.txt")
        );
        // 单个项目外的文件
        let one = started_patch(
            "/w",
            json!([{"path": "/etc/hosts", "kind": {"type": "update", "move_path": null}, "diff": "@@\n"}]),
        );
        let (pending, _) = from_file_change("r", json!(1), &p, "/w", Some(&one));
        assert_eq!(pending.title, "Edit /etc/hosts");
        assert_eq!(tool_of(&pending).detail.as_ref().unwrap().path.as_deref(), Some("/etc/hosts"));
    }

    #[test]
    fn patch_approval_without_the_started_item_says_the_contents_are_unknown() {
        let p: FileApproval = serde_json::from_value(
            json!({"itemId": "f1", "reason": "write outside the workspace", "grantRoot": "/data"}),
        )
        .unwrap();
        let (pending, _) = from_file_change("r", json!(2), &p, "/w", None);
        assert_eq!(pending.title, "Edit files (patch contents unknown) and allow writing under /data for this session");
        let t = tool_of(&pending);
        assert_eq!(t.input["contentUnknown"], true);
        assert_eq!(t.input["reason"], "write outside the workspace");
        assert_eq!(t.input["grantRoot"], "/data");
        let d = t.detail.as_ref().unwrap().description.as_deref().unwrap();
        assert!(d.starts_with("write outside the workspace\n\nThe patch contents are unknown"), "{d}");
        assert!(d.contains("grants write access under /data"), "{d}");
        // 什么都没有也要有醒目的说明
        let bare: FileApproval = serde_json::from_value(json!({"itemId": "f1"})).unwrap();
        let (pending, _) = from_file_change("r", json!(2), &bare, "/w", None);
        assert_eq!(pending.title, "Edit files (patch contents unknown)");
        assert!(tool_of(&pending)
            .detail
            .as_ref()
            .unwrap()
            .description
            .as_deref()
            .unwrap()
            .starts_with("The patch contents are unknown"));
    }

    #[test]
    fn the_requests_own_command_wins_over_the_started_item() {
        // zsh-exec-bridge：同一个 item 的多次回调，每次的 `command` 是脚本里的一条子命令
        let started = ToolCallParts {
            name: "commandExecution".into(),
            category: ToolCategory::Shell,
            title: "make all".into(),
            input: json!({"command": "/bin/zsh -lc 'make all'", "cwd": "/w", "commandActions": []}),
            detail: Some(ToolCallDetail {
                command: Some("make all".into()),
                cwd: Some("/w".into()),
                ..Default::default()
            }),
        };
        let sub = cmd_params(json!({
            "itemId": "c1", "approvalId": "7d8b-sub-1", "command": "rm -rf build", "cwd": "/w/sub", "reason": "deleting a directory"
        }));
        let (pending, _) = from_command("r", json!(1), &sub, "/fallback", Some(&started));
        assert_eq!(pending.title, "Run sub-command");
        let t = tool_of(&pending);
        let d = t.detail.as_ref().unwrap();
        assert_eq!(d.command.as_deref(), Some("rm -rf build"));
        assert_eq!(d.cwd.as_deref(), Some("/w/sub"));
        assert_eq!(d.description.as_deref(), Some("deleting a directory"));
        assert_eq!(t.input["command"], "rm -rf build");
        assert_eq!(t.input["approvalId"], "7d8b-sub-1");
        assert_eq!(t.input["commandActions"], json!([])); // started 只补请求里没有的
        assert_eq!(t.cwd.as_deref(), Some("/w/sub"));
        // 一个没有 approvalId 的普通审批，请求和 item 的命令一致：还是 "Run command"
        let plain = cmd_params(json!({"itemId": "c1", "command": "/bin/zsh -lc 'make all'"}));
        let (pending, _) = from_command("r", json!(1), &plain, "/fallback", Some(&started));
        assert_eq!(pending.title, "Run command");
        assert_eq!(tool_of(&pending).detail.as_ref().unwrap().command.as_deref(), Some("make all"));
        // 请求没带 command（或是空的）：才用 item 里记的，cwd 也从 item 补
        let bare = cmd_params(json!({"itemId": "c1", "command": "  "}));
        let (pending, _) = from_command("r", json!(1), &bare, "/fallback", Some(&started));
        let t = tool_of(&pending);
        assert_eq!(t.detail.as_ref().unwrap().command.as_deref(), Some("make all"));
        assert_eq!(t.cwd.as_deref(), Some("/w"));
        assert_eq!(pending.title, "Run command");
    }

    #[test]
    fn permissions_request_summary_and_reason() {
        let p: PermissionsApproval = serde_json::from_value(json!({
            "itemId": "p1", "cwd": "/w", "reason": "Need network access to fetch a package",
            "permissions": {"network": {"enabled": true}, "fileSystem": {"read": ["/usr/share"], "write": ["/w/sub"]}}
        }))
        .unwrap();
        let (pending, native) = from_permissions("r", json!(3), &p, "/fallback");
        assert_eq!(pending.title, "Grant network access; read /usr/share; write /w/sub");
        let t = tool_of(&pending);
        assert_eq!((t.tool.as_str(), t.category), ("requestPermissions", ToolCategory::Other));
        assert_eq!(t.detail.as_ref().unwrap().description.as_deref(), Some("Need network access to fetch a package"));
        assert_eq!(t.input["permissions"]["network"]["enabled"], true);
        assert!(matches!(native.kind, NativeKind::Permissions { .. }));
        // 空轮廓
        let p: PermissionsApproval = serde_json::from_value(json!({"itemId": "p1", "permissions": {}})).unwrap();
        let (pending, _) = from_permissions("r", json!(3), &p, "/fallback");
        assert_eq!(pending.title, "Grant additional permissions");
        assert!(tool_of(&pending).detail.is_none());
    }

    fn decide(option: Option<&str>) -> PermissionDecision {
        PermissionDecision { option_id: option.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn command_and_file_decisions() {
        for kind in [NativeKind::Command, NativeKind::FileChange] {
            let n = NativePending { rpc_id: json!(1), kind };
            let r = |o: Option<&str>, cancelling| build_response(&n, &decide(o), cancelling);
            assert_eq!(r(Some("allow"), false).unwrap(), json!({"decision": "accept"}));
            assert_eq!(r(Some("allow_session"), false).unwrap(), json!({"decision": "acceptForSession"}));
            assert_eq!(r(Some("deny"), false).unwrap(), json!({"decision": "decline"}));
            assert_eq!(r(None, false).unwrap(), json!({"decision": "decline"}));
            // 会话取消：不管选了什么都是 cancel（连未知选项也是）
            assert_eq!(r(Some("deny"), true).unwrap(), json!({"decision": "cancel"}));
            assert_eq!(r(Some("whatever"), true).unwrap(), json!({"decision": "cancel"}));
            assert!(matches!(r(Some("whatever"), false), Err(AdapterError::Protocol(_))));
        }
    }

    #[test]
    fn permissions_decisions_echo_the_request_or_grant_nothing() {
        let requested = json!({"network": {"enabled": true}});
        let n = NativePending { rpc_id: json!(1), kind: NativeKind::Permissions { requested: requested.clone() } };
        let r = |o: Option<&str>, cancelling| build_response(&n, &decide(o), cancelling).unwrap();
        assert_eq!(r(Some("allow"), false), json!({"permissions": requested, "scope": "turn"}));
        assert_eq!(r(Some("allow_session"), false), json!({"permissions": requested, "scope": "session"}));
        assert_eq!(r(Some("deny"), false), json!({"permissions": {}, "scope": "turn"}));
        assert_eq!(r(Some("allow"), true), json!({"permissions": {}, "scope": "turn"}));
    }

    #[test]
    fn user_input_round_trip() {
        let req: UserInputRequest = serde_json::from_value(json!({
            "questions": [{
                "id": "which_db", "header": "Database", "question": "Which database should I use?", "isOther": true,
                "options": [{"label": "SQLite (Recommended)", "description": "Zero setup"}, {"label": "Postgres", "description": ""}]
            }]
        }))
        .unwrap();
        let (pending, native) = from_user_input("r", json!("s-1"), &req);
        assert_eq!(pending.kind, PermissionKind::Question);
        let q = &pending.detail.questions[0];
        assert_eq!(q.id, "which_db");
        assert_eq!(q.text, "Database: Which database should I use?");
        assert!(q.free_text);
        assert_eq!(q.choices[0].id, "SQLite (Recommended)");
        assert_eq!(q.choices[0].label, "SQLite (Recommended) — Zero setup");
        assert_eq!(q.choices[1].label, "Postgres");
        assert_eq!(native.rpc_id, json!("s-1"));

        let answers = serde_json::Map::from_iter([
            ("which_db".to_string(), json!("SQLite (Recommended)")),
            ("not_asked".to_string(), json!("x")),
        ]);
        let answer = PermissionDecision {
            option_id: Some("answer".into()),
            answers: Some(answers.clone()),
            ..Default::default()
        };
        assert_eq!(
            build_response(&native, &answer, false).unwrap(),
            json!({"answers": {"which_db": {"answers": ["SQLite (Recommended)"]}}})
        );
        // 没带 option_id 但带了 answers 视为回答；多选是数组
        let multi = serde_json::Map::from_iter([("which_db".to_string(), json!(["a", "b"]))]);
        let d = PermissionDecision { answers: Some(multi), ..Default::default() };
        assert_eq!(
            build_response(&native, &d, false).unwrap(),
            json!({"answers": {"which_db": {"answers": ["a", "b"]}}})
        );
        // 跳过：空答案，即使带着 answers 也以选项为准
        let skip = PermissionDecision { option_id: Some("deny".into()), answers: Some(answers), ..Default::default() };
        assert_eq!(build_response(&native, &skip, false).unwrap(), json!({"answers": {}}));
        assert_eq!(build_response(&native, &decide(None), false).unwrap(), json!({"answers": {}}));
        assert_eq!(build_response(&native, &answer, true).unwrap(), json!({"answers": {}}));
        // answer 却没有 answers / 选项不认识
        assert!(build_response(&native, &decide(Some("answer")), false).is_err());
        assert!(build_response(&native, &decide(Some("bogus")), false).is_err());
    }

    #[test]
    fn question_without_options_is_free_text() {
        let req: UserInputRequest = serde_json::from_value(
            json!({"questions": [{"id": "q", "header": "", "question": "Name?", "options": null}]}),
        )
        .unwrap();
        let (pending, _) = from_user_input("r", json!(1), &req);
        let q = &pending.detail.questions[0];
        assert_eq!(q.text, "Name?");
        assert!(q.free_text && q.choices.is_empty());
    }

    #[test]
    fn unsupported_requests_get_a_schema_valid_refusal() {
        assert_eq!(unsupported("mcpServer/elicitation/request").result, Ok(json!({"action": "decline"})));
        let tool = unsupported("item/tool/call").result.unwrap();
        assert_eq!(tool["success"], false);
        assert_eq!(tool["contentItems"][0]["type"], "inputText");
        assert_eq!(unsupported("applyPatchApproval").result, Ok(json!({"decision": "abort"})));
        assert_eq!(unsupported("execCommandApproval").result, Ok(json!({"decision": "abort"})));
        assert!(unsupported("account/chatgptAuthTokens/refresh").result.is_err());
        assert!(unsupported("attestation/generate").result.is_err());
        let unknown = unsupported("some/new/request");
        assert_eq!(unknown.result.unwrap_err().code, METHOD_NOT_FOUND);
        assert!(unknown.message.contains("some/new/request"));
        // 说明总是写给人看的
        assert!(unsupported("mcpServer/elicitation/request").message.contains("MCP"));
    }
}
