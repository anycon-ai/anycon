//! 内层方法名。`域.动作`。C→D 是请求，D→C 是通知（除非注明）。

// pairing（握手刚完成、PairDone 之后，daemon → joiner 的第一条加密通知；joiner 收到并校验后才落盘）
/// D→C notification，params = PairAck
pub const PAIR_ACK: &str = "pair.ack";

// node
pub const NODE_INFO: &str = "node.info";
pub const NODE_DEVICES: &str = "node.devices";
pub const NODE_REVOKE_DEVICE: &str = "node.revoke_device";
pub const NODE_PAIR_BEGIN: &str = "node.pair_begin";
pub const NODE_STATS: &str = "node.stats";
pub const NODE_DOCTOR: &str = "node.doctor";
pub const NODE_AUDIT: &str = "node.audit";
pub const NODE_SET_AGENT_DEFAULTS: &str = "node.set_agent_defaults";
/// 可以远程查看的配置项（白名单，ADR-43）。
pub const NODE_GET_CONFIG: &str = "node.get_config";
/// 远程修改白名单里的配置项；安全相关的键只能收紧（ADR-43）。
pub const NODE_SET_CONFIG: &str = "node.set_config";
/// daemon 日志文件的尾部。
pub const NODE_LOGS: &str = "node.logs";

// 账号内接入（ADR-36）
/// 进行中的接入会话。
pub const NODE_ENROLL_LIST: &str = "node.enroll_list";
/// 批准一个接入请求：`code` 返回 6 位码，`scan` 带二维码里的密钥。
pub const NODE_ENROLL_APPROVE: &str = "node.enroll_approve";
pub const NODE_ENROLL_DENY: &str = "node.enroll_deny";
/// 主动生成 6 位接入码（「添加设备」、`anycon pair --enroll`）。
pub const NODE_ENROLL_CODE: &str = "node.enroll_code";
pub const NODE_ENROLL_STATUS: &str = "node.enroll_status";
pub const NODE_SET_ENROLL_PASSWORD: &str = "node.set_enroll_password";
pub const NODE_SET_ENROLL_ENABLED: &str = "node.set_enroll_enabled";
/// D→C notification，params = EnrollInfo
pub const NODE_ENROLL_REQUESTED: &str = "node.enroll_requested";
/// D→C notification，params = NodeEnrollResolved
pub const NODE_ENROLL_RESOLVED: &str = "node.enroll_resolved";
/// 开一次只接受 Twin 设备的短码配对（docs/spec/daemon.md § 27.2）：`{}` → `{ code, expires_ms }`。只由人调用。
pub const NODE_TWIN_LINK_CODE: &str = "node.twin_link_code";
/// D→C notification，params = NodeSecurityEvent
pub const NODE_SECURITY_EVENT: &str = "node.security_event";

// agent
pub const AGENT_LIST: &str = "agent.list";
pub const AGENT_DIAGNOSE: &str = "agent.diagnose";
/// D→C notification，params = AgentListResult
pub const AGENT_CHANGED: &str = "agent.changed";

// workspace
pub const WORKSPACE_LIST: &str = "workspace.list";
pub const WORKSPACE_CREATE: &str = "workspace.create";
pub const WORKSPACE_CLONE: &str = "workspace.clone";
pub const WORKSPACE_RENAME: &str = "workspace.rename";
pub const WORKSPACE_TRASH: &str = "workspace.trash";
pub const WORKSPACE_TRASH_LIST: &str = "workspace.trash_list";
pub const WORKSPACE_RESTORE: &str = "workspace.restore";
pub const WORKSPACE_PURGE: &str = "workspace.purge";
/// 把本机已有目录关联为项目（不移动、不复制）。
pub const WORKSPACE_LINK: &str = "workspace.link";
/// 解除关联：只从列表里去掉，不动目录。
pub const WORKSPACE_UNLINK: &str = "workspace.unlink";
/// 给关联选择器列本机目录（只在 `workspace.link_roots` 之内）。
pub const WORKSPACE_BROWSE: &str = "workspace.browse";
/// 列项目的分支（新建会话选 base 用）。
pub const WORKSPACE_BRANCHES: &str = "workspace.branches";
/// 列 anycon 建的 worktree（ADR-34）。
pub const WORKSPACE_WORKTREES: &str = "workspace.worktrees";
/// 删掉一个已保留 / 没有会话的 worktree。
pub const WORKSPACE_REMOVE_WORKTREE: &str = "workspace.remove_worktree";
/// D→C notification
pub const WORKSPACE_CHANGED: &str = "workspace.changed";
/// D→C notification
pub const JOB_PROGRESS: &str = "job.progress";
/// D→C notification
pub const JOB_DONE: &str = "job.done";
pub const JOB_CANCEL: &str = "job.cancel";

// fs
pub const FS_LIST: &str = "fs.list";
pub const FS_READ: &str = "fs.read";

// session
pub const SESSION_LIST: &str = "session.list";
pub const SESSION_GET: &str = "session.get";
pub const SESSION_CREATE: &str = "session.create";
pub const SESSION_SEND: &str = "session.send";
pub const SESSION_CANCEL: &str = "session.cancel";
pub const SESSION_SET_SETTINGS: &str = "session.set_settings";
pub const SESSION_WATCH: &str = "session.watch";
pub const SESSION_UNWATCH: &str = "session.unwatch";
pub const SESSION_HISTORY: &str = "session.history";
/// D→C notification，params = SessionEvent
pub const SESSION_EVENT: &str = "session.event";
/// D→C notification，params = SessionChanged
pub const SESSION_CHANGED: &str = "session.changed";
pub const SESSION_RENAME: &str = "session.rename";
pub const SESSION_ARCHIVE: &str = "session.archive";
pub const SESSION_UNARCHIVE: &str = "session.unarchive";
pub const SESSION_SET_MARKS: &str = "session.set_marks";
pub const SESSION_MARK_READ: &str = "session.mark_read";
pub const SESSION_DELETE: &str = "session.delete";
pub const SESSION_RESTART: &str = "session.restart";
pub const SESSION_SEARCH: &str = "session.search";
pub const SESSION_PROMOTE: &str = "session.promote";
/// 会话工作目录的 git 状态（ADR-34）。
pub const SESSION_GIT_STATUS: &str = "session.git_status";
/// 会话的改动（只读 diff）。
pub const SESSION_DIFF: &str = "session.diff";
/// 把 base 合并进 worktree 分支。
pub const SESSION_SYNC_BASE: &str = "session.sync_base";

// todo（清单，docs/spec/daemon.md § 21）
pub const TODO_LIST: &str = "todo.list";
/// 读一项：不存在 → `not_found`。只读，不审计、不通知。
pub const TODO_GET: &str = "todo.get";
/// 按 `id` 幂等：已存在时原样返回已有的那一项。
pub const TODO_ADD: &str = "todo.add";
pub const TODO_UPDATE: &str = "todo.update";
/// 物理删除；带 `if_updated_ms` 时只在这一项现在的 `updated_ms` 正是它时才删（否则 `invalid` / `changed`）。
pub const TODO_DELETE: &str = "todo.delete";
/// D→C notification，params = TodoChanged
pub const TODO_CHANGED: &str = "todo.changed";
/// 此刻派发会发出的第一条消息（`{ id, no_push? }` → `{ first_message }`，docs/spec/daemon.md § 22.2）。
/// 不改这一项，也不开会话、不审计、不发推送通知；唯一的副作用是先把这一项排着、还没记的结果记进「经过」（和派发之前的那一步一样，
/// 记了就广播 `todo.event`，`activity` 因此变了才跟一条 `todo.changed`），这样预览里带的「之前的结果」和随后派发带的一致。
pub const TODO_DISPATCH_PREVIEW: &str = "todo.dispatch_preview";
/// 从一项开一个会话并关联（docs/spec/daemon.md § 22）。
pub const TODO_DISPATCH: &str = "todo.dispatch";
/// 现在执行一次周期项（`{ id }` → 新复制出的 `Todo`，docs/spec/daemon.md § 26.9）：和到点复制一样，但不动周期项的下一次时间。
/// 只由人调用；不是周期项 / 已关闭 / 未收下 / 上一份还没关闭 → `invalid`（`data.reason`）。
pub const TODO_REPEAT_NOW: &str = "todo.repeat_now";
/// 一项的「经过」，按 `seq` 从新到旧翻页。
pub const TODO_HISTORY: &str = "todo.history";
/// D→C notification，params = TodoEventNotice：「经过」多了一条。
pub const TODO_EVENT: &str = "todo.event";

// rule（决策档案，docs/spec/daemon.md § 23）
/// `{ session_id }` → `{ run?, points }`
pub const RULE_EXTRACT: &str = "rule.extract";
/// params = RuleExtractRunsParams → RuleExtractRunsResult
pub const RULE_EXTRACT_RUNS: &str = "rule.extract_runs";
/// D→C notification，params = RuleExtractDone
pub const RULE_EXTRACT_DONE: &str = "rule.extract_done";
/// params = RuleListParams → RuleListResult
pub const RULE_LIST: &str = "rule.list";
/// 按 `id` 幂等：已存在时原样返回已有的那一条。
pub const RULE_ADD: &str = "rule.add";
/// 带 `if_updated_ms` 时只在这条规则现在的 `updated_ms` 正是它时才改（否则 `invalid` / `changed`）。
pub const RULE_UPDATE: &str = "rule.update";
/// 物理删除；`if_updated_ms` 同 `rule.update`。
pub const RULE_DELETE: &str = "rule.delete";
/// 现在开一个这个项目的会话会注入的那段文字。只读。
pub const RULE_PREVIEW: &str = "rule.preview";
/// D→C notification，params = RuleChanged
pub const RULE_CHANGED: &str = "rule.changed";

// Twin（docs/spec/daemon.md § 26）
pub const TWIN_GET: &str = "twin.get";
pub const TWIN_SET: &str = "twin.set";
pub const TWIN_WAKE: &str = "twin.wake";
pub const TWIN_DRY_RUN: &str = "twin.dry_run";
pub const TWIN_RUNS: &str = "twin.runs";
/// C→D：最近几个本地日的 Twin 统计（`TwinStatsParams` → `TwinStatsResult`），只读。
pub const TWIN_STATS: &str = "twin.stats";
pub const TWIN_QUESTIONS: &str = "twin.questions";
pub const TWIN_ANSWER: &str = "twin.answer";
pub const TWIN_CHANGED: &str = "twin.changed";
pub const TWIN_QUESTION: &str = "twin.question";
// Twin 连接（docs/spec/daemon.md § 27）
/// A 上：用 B 的短码建立一条 Twin 连接（`{ code, node_id }` → `TwinLink`）。只由人调用。
pub const TWIN_LINK: &str = "twin.link";
/// A 上：列出 Twin 连接（`{}` → `{ links }`）。
pub const TWIN_LINKS: &str = "twin.links";
/// A 上：忘掉一条 Twin 连接（`{ node_id }`）。B 上的设备记录要在 B 上吊销。
pub const TWIN_UNLINK: &str = "twin.unlink";
/// B 上：对这条 Twin 连接开放了什么、还剩多少（`{}` → `TwinPeerStatus`，只给调用的这条连接自己的）。
pub const TWIN_PEER_STATUS: &str = "twin.peer_status";

// blob（附件，ADR-35）
/// 开始上传一个附件。
pub const BLOB_PUT_BEGIN: &str = "blob.put_begin";
/// 上传一块（可乱序、可并发、可重发）。
pub const BLOB_PUT_CHUNK: &str = "blob.put_chunk";
/// 结束上传：校验大小、SHA-256、文件头，返回 `AttachmentRef`。
pub const BLOB_PUT_END: &str = "blob.put_end";
/// 按偏移读一段已上传的 blob。
pub const BLOB_GET: &str = "blob.get";
/// 丢掉一个进行中的上传，或已传完但还没被消息引用的 blob。
pub const BLOB_DISCARD: &str = "blob.discard";

// permission
/// D→C notification，params = PermissionRequest
pub const PERMISSION_REQUEST: &str = "permission.request";
pub const PERMISSION_RESPOND: &str = "permission.respond";
/// D→C notification，params = PermissionResolved
pub const PERMISSION_RESOLVED: &str = "permission.resolved";
pub const PERMISSION_LIST: &str = "permission.list";

// dev server 预览（ADR-39）。隧道本身不走 JSON-RPC，见 `preview::tunnel`。
/// 为本机端口签发预览票据（params = PreviewOpenParams → PreviewOpenResult）。
pub const PREVIEW_OPEN: &str = "preview.open";
/// 作废一个 label 的全部票据并断开隧道（params = PreviewCloseParams）。
pub const PREVIEW_CLOSE: &str = "preview.close";
/// 未过期的票据（→ PreviewListResult）。
pub const PREVIEW_LIST: &str = "preview.list";
/// 可以预览的本机端口（params = PreviewPortsParams → PreviewPortsResult）。
pub const PREVIEW_PORTS: &str = "preview.ports";

/// 方向：C→D 请求，或 D→C 通知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Request,
    Notification,
}

/// 方法注册表：方法名 → params / result 的类型名（即 `schema::ProtocolSchema` 的字段名，也是生成的 TS / Python 类型名）。
/// `just gen` 据此生成 `packages/protocol/src/method-types.ts`，SDK 和 Web 不再手写返回类型。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct MethodSpec {
    pub name: &'static str,
    pub direction: Direction,
    /// 类型名；`None` = 无参数 / 无结果（`Empty`）。
    pub params: Option<&'static str>,
    pub result: Option<&'static str>,
}

const fn req(name: &'static str, params: Option<&'static str>, result: Option<&'static str>) -> MethodSpec {
    MethodSpec { name, direction: Direction::Request, params, result }
}
const fn notif(name: &'static str, params: &'static str) -> MethodSpec {
    MethodSpec { name, direction: Direction::Notification, params: Some(params), result: None }
}

pub const REGISTRY: &[MethodSpec] = &[
    notif(PAIR_ACK, "PairAck"),
    req(NODE_INFO, None, Some("NodeInfo")),
    req(NODE_DEVICES, None, Some("NodeDevicesResult")),
    req(NODE_REVOKE_DEVICE, Some("NodeRevokeDeviceParams"), None),
    req(NODE_PAIR_BEGIN, Some("NodePairBeginParams"), Some("NodePairBeginResult")),
    req(NODE_STATS, None, Some("NodeStats")),
    req(NODE_DOCTOR, None, Some("NodeDoctorResult")),
    req(NODE_AUDIT, Some("NodeAuditParams"), Some("NodeAuditResult")),
    req(NODE_SET_AGENT_DEFAULTS, Some("NodeSetAgentDefaultsParams"), None),
    req(NODE_TWIN_LINK_CODE, None, Some("NodeTwinLinkCodeResult")),
    req(NODE_GET_CONFIG, None, Some("NodeConfigResult")),
    req(NODE_SET_CONFIG, Some("NodeSetConfigParams"), Some("NodeConfigResult")),
    req(NODE_LOGS, Some("NodeLogsParams"), Some("NodeLogsResult")),
    req(NODE_ENROLL_LIST, None, Some("NodeEnrollListResult")),
    req(NODE_ENROLL_APPROVE, Some("NodeEnrollApproveParams"), Some("NodeEnrollApproveResult")),
    req(NODE_ENROLL_DENY, Some("NodeEnrollIdParams"), None),
    req(NODE_ENROLL_CODE, None, Some("NodeEnrollCodeResult")),
    req(NODE_ENROLL_STATUS, None, Some("NodeEnrollStatusResult")),
    req(NODE_SET_ENROLL_PASSWORD, Some("NodeSetEnrollPasswordParams"), None),
    req(NODE_SET_ENROLL_ENABLED, Some("NodeSetEnrollEnabledParams"), None),
    notif(NODE_ENROLL_REQUESTED, "EnrollInfo"),
    notif(NODE_ENROLL_RESOLVED, "NodeEnrollResolved"),
    notif(NODE_SECURITY_EVENT, "NodeSecurityEvent"),
    req(AGENT_LIST, Some("AgentListParams"), Some("AgentListResult")),
    req(AGENT_DIAGNOSE, Some("AgentDiagnoseParams"), Some("AgentDiagnoseResult")),
    notif(AGENT_CHANGED, "AgentListResult"),
    req(WORKSPACE_LIST, None, Some("WorkspaceListResult")),
    req(WORKSPACE_CREATE, Some("WorkspaceCreateParams"), Some("Project")),
    req(WORKSPACE_CLONE, Some("WorkspaceCloneParams"), Some("JobStarted")),
    req(WORKSPACE_RENAME, Some("WorkspaceRenameParams"), Some("RenamedProject")),
    req(WORKSPACE_TRASH, Some("WorkspaceNameParams"), Some("TrashEntry")),
    req(WORKSPACE_TRASH_LIST, None, Some("WorkspaceTrashListResult")),
    req(WORKSPACE_RESTORE, Some("WorkspaceTrashIdParams"), Some("Project")),
    req(WORKSPACE_PURGE, Some("WorkspaceTrashIdParams"), None),
    req(WORKSPACE_LINK, Some("WorkspaceLinkParams"), Some("Project")),
    req(WORKSPACE_UNLINK, Some("WorkspaceNameParams"), None),
    req(WORKSPACE_BROWSE, Some("WorkspaceBrowseParams"), Some("WorkspaceBrowseResult")),
    req(WORKSPACE_BRANCHES, Some("WorkspaceBranchesParams"), Some("WorkspaceBranchesResult")),
    req(WORKSPACE_WORKTREES, Some("WorkspaceWorktreesParams"), Some("WorkspaceWorktreesResult")),
    req(WORKSPACE_REMOVE_WORKTREE, Some("WorkspaceRemoveWorktreeParams"), None),
    notif(WORKSPACE_CHANGED, "WorkspaceChanged"),
    notif(JOB_PROGRESS, "JobProgress"),
    notif(JOB_DONE, "JobDone"),
    req(JOB_CANCEL, Some("JobCancelParams"), None),
    req(FS_LIST, Some("FsListParams"), Some("FsListResult")),
    req(FS_READ, Some("FsReadParams"), Some("FsReadResult")),
    req(SESSION_LIST, Some("SessionListParams"), Some("SessionListResult")),
    req(SESSION_GET, Some("SessionIdParams"), Some("SessionSummary")),
    req(SESSION_CREATE, Some("SessionCreateParams"), Some("SessionSummary")),
    req(SESSION_SEND, Some("SessionSendParams"), Some("SessionSendResult")),
    req(SESSION_CANCEL, Some("SessionIdParams"), None),
    req(SESSION_SET_SETTINGS, Some("SessionSetSettingsParams"), Some("SessionSetSettingsResult")),
    req(SESSION_WATCH, Some("SessionIdParams"), None),
    req(SESSION_UNWATCH, Some("SessionIdParams"), None),
    req(SESSION_HISTORY, Some("SessionHistoryParams"), Some("SessionHistoryResult")),
    notif(SESSION_EVENT, "SessionEvent"),
    notif(SESSION_CHANGED, "SessionChanged"),
    req(SESSION_RENAME, Some("SessionRenameParams"), None),
    req(SESSION_ARCHIVE, Some("SessionArchiveParams"), None),
    req(SESSION_UNARCHIVE, Some("SessionIdParams"), None),
    req(SESSION_SET_MARKS, Some("SessionSetMarksParams"), None),
    req(SESSION_MARK_READ, Some("SessionMarkReadParams"), None),
    req(SESSION_DELETE, Some("SessionDeleteParams"), None),
    req(SESSION_RESTART, Some("SessionIdParams"), None),
    req(SESSION_SEARCH, Some("SessionSearchParams"), Some("SessionSearchResult")),
    req(SESSION_PROMOTE, Some("SessionPromoteParams"), None),
    req(SESSION_GIT_STATUS, Some("SessionGitStatusParams"), Some("SessionGitStatus")),
    req(SESSION_DIFF, Some("SessionDiffParams"), Some("SessionDiffResult")),
    req(SESSION_SYNC_BASE, Some("SessionSyncBaseParams"), Some("SessionSyncBaseResult")),
    req(TODO_LIST, Some("TodoListParams"), Some("TodoListResult")),
    req(TODO_GET, Some("TodoIdParams"), Some("Todo")),
    req(TODO_ADD, Some("TodoAddParams"), Some("Todo")),
    req(TODO_UPDATE, Some("TodoUpdateParams"), Some("Todo")),
    req(TODO_DELETE, Some("TodoDeleteParams"), None),
    notif(TODO_CHANGED, "TodoChanged"),
    req(TODO_DISPATCH_PREVIEW, Some("TodoDispatchPreviewParams"), Some("TodoDispatchPreviewResult")),
    req(TODO_DISPATCH, Some("TodoDispatchParams"), Some("TodoDispatchResult")),
    req(TODO_REPEAT_NOW, Some("TodoIdParams"), Some("Todo")),
    req(TODO_HISTORY, Some("TodoHistoryParams"), Some("TodoHistoryResult")),
    notif(TODO_EVENT, "TodoEventNotice"),
    req(RULE_LIST, Some("RuleListParams"), Some("RuleListResult")),
    req(RULE_ADD, Some("RuleAddParams"), Some("Rule")),
    req(RULE_UPDATE, Some("RuleUpdateParams"), Some("Rule")),
    req(RULE_DELETE, Some("RuleDeleteParams"), None),
    req(RULE_PREVIEW, Some("RulePreviewParams"), Some("RulePreviewResult")),
    notif(RULE_CHANGED, "RuleChanged"),
    req(RULE_EXTRACT, Some("RuleExtractParams"), Some("RuleExtractResult")),
    req(RULE_EXTRACT_RUNS, Some("RuleExtractRunsParams"), Some("RuleExtractRunsResult")),
    notif(RULE_EXTRACT_DONE, "RuleExtractDone"),
    req(TWIN_GET, Some("TwinGetParams"), Some("TwinGetResult")),
    req(TWIN_SET, Some("TwinSetParams"), Some("TwinGetResult")),
    req(TWIN_WAKE, None, None),
    req(TWIN_DRY_RUN, None, Some("TwinDryRunResult")),
    req(TWIN_RUNS, Some("TwinRunsParams"), Some("TwinRunsResult")),
    req(TWIN_STATS, Some("TwinStatsParams"), Some("TwinStatsResult")),
    req(TWIN_QUESTIONS, Some("TwinQuestionsParams"), Some("TwinQuestionsResult")),
    req(TWIN_ANSWER, Some("TwinAnswerParams"), Some("TwinQuestion")),
    notif(TWIN_CHANGED, "TwinChanged"),
    notif(TWIN_QUESTION, "TwinQuestionNotice"),
    req(TWIN_LINK, Some("TwinLinkParams"), Some("TwinLink")),
    req(TWIN_LINKS, None, Some("TwinLinksResult")),
    req(TWIN_UNLINK, Some("TwinUnlinkParams"), None),
    req(TWIN_PEER_STATUS, None, Some("TwinPeerStatus")),
    req(BLOB_PUT_BEGIN, Some("BlobPutBeginParams"), Some("BlobPutBeginResult")),
    req(BLOB_PUT_CHUNK, Some("BlobPutChunkParams"), None),
    req(BLOB_PUT_END, Some("BlobIdParams"), Some("AttachmentRef")),
    req(BLOB_GET, Some("BlobGetParams"), Some("BlobGetResult")),
    req(BLOB_DISCARD, Some("BlobIdParams"), None),
    notif(PERMISSION_REQUEST, "PermissionRequest"),
    req(PERMISSION_RESPOND, Some("PermissionRespondParams"), None),
    notif(PERMISSION_RESOLVED, "PermissionResolved"),
    req(PERMISSION_LIST, Some("PermissionListParams"), Some("PermissionListResult")),
    req(PREVIEW_OPEN, Some("PreviewOpenParams"), Some("PreviewOpenResult")),
    req(PREVIEW_CLOSE, Some("PreviewCloseParams"), None),
    req(PREVIEW_LIST, None, Some("PreviewListResult")),
    req(PREVIEW_PORTS, Some("PreviewPortsParams"), Some("PreviewPortsResult")),
];

#[cfg(test)]
mod tests {
    #[test]
    fn registry_names_unique() {
        let mut names: Vec<_> = super::REGISTRY.iter().map(|m| m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), super::REGISTRY.len());
    }
}
