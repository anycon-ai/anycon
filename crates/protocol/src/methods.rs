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
    req(SESSION_DELETE, Some("SessionDeleteParams"), None),
    req(SESSION_RESTART, Some("SessionIdParams"), None),
    req(SESSION_SEARCH, Some("SessionSearchParams"), Some("SessionSearchResult")),
    req(SESSION_PROMOTE, Some("SessionPromoteParams"), None),
    req(SESSION_GIT_STATUS, Some("SessionGitStatusParams"), Some("SessionGitStatus")),
    req(SESSION_DIFF, Some("SessionDiffParams"), Some("SessionDiffResult")),
    req(SESSION_SYNC_BASE, Some("SessionSyncBaseParams"), Some("SessionSyncBaseResult")),
    req(BLOB_PUT_BEGIN, Some("BlobPutBeginParams"), Some("BlobPutBeginResult")),
    req(BLOB_PUT_CHUNK, Some("BlobPutChunkParams"), None),
    req(BLOB_PUT_END, Some("BlobIdParams"), Some("AttachmentRef")),
    req(BLOB_GET, Some("BlobGetParams"), Some("BlobGetResult")),
    req(BLOB_DISCARD, Some("BlobIdParams"), None),
    notif(PERMISSION_REQUEST, "PermissionRequest"),
    req(PERMISSION_RESPOND, Some("PermissionRespondParams"), None),
    notif(PERMISSION_RESOLVED, "PermissionResolved"),
    req(PERMISSION_LIST, Some("PermissionListParams"), Some("PermissionListResult")),
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
