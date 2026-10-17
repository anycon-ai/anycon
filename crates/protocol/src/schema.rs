//! JSON Schema 导出。根类型 [`ProtocolSchema`] 把所有对外类型挂成字段，生成器据此一次产出全部 definitions。

use serde::{Deserialize, Serialize};

use crate::*;

/// 仅用于导出 schema，不在线上传输。字段名即 TS / Python 的类型名。
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProtocolSchema {
    // 外层
    pub kind: Kind,
    pub notify_body: NotifyBody,
    pub notify_detail: NotifyDetail,
    pub control: Control,
    pub client_info: ClientInfo,
    pub quota: Quota,
    // rpc
    pub rpc_message: RpcMessage,
    pub rpc_error: RpcError,
    pub error_code: ErrorCode,
    pub empty: Empty,
    // node
    pub node_info: NodeInfo,
    pub node_devices_result: NodeDevicesResult,
    pub node_revoke_device_params: NodeRevokeDeviceParams,
    pub node_pair_begin_params: NodePairBeginParams,
    pub node_pair_begin_result: NodePairBeginResult,
    pub pair_ack: PairAck,
    pub project: Project,
    pub trash_entry: TrashEntry,
    pub node_stats: NodeStats,
    pub node_doctor_result: NodeDoctorResult,
    pub node_audit_params: NodeAuditParams,
    pub node_audit_result: NodeAuditResult,
    pub node_set_agent_defaults_params: NodeSetAgentDefaultsParams,
    // 账号内接入（ADR-36）
    pub enroll_frame: EnrollFrame,
    pub enroll_method: EnrollMethod,
    pub enroll_hint: EnrollHint,
    pub enroll_reply_status: EnrollReplyStatus,
    pub enroll_reject_reason: EnrollRejectReason,
    pub enroll_fail_reason: EnrollFailReason,
    pub enroll_session_status: EnrollSessionStatus,
    pub enroll_info: EnrollInfo,
    pub node_enroll_list_result: NodeEnrollListResult,
    pub node_enroll_approve_params: NodeEnrollApproveParams,
    pub node_enroll_approve_result: NodeEnrollApproveResult,
    pub node_enroll_id_params: NodeEnrollIdParams,
    pub enroll_result: EnrollResult,
    pub node_enroll_resolved: NodeEnrollResolved,
    pub node_enroll_code_result: NodeEnrollCodeResult,
    pub enroll_password_state: EnrollPasswordState,
    pub enroll_password_status: EnrollPasswordStatus,
    pub node_enroll_status_result: NodeEnrollStatusResult,
    pub node_set_enroll_password_params: NodeSetEnrollPasswordParams,
    pub node_set_enroll_enabled_params: NodeSetEnrollEnabledParams,
    pub security_event_kind: SecurityEventKind,
    pub node_security_event: NodeSecurityEvent,
    // agent
    pub agent_info: AgentInfo,
    pub agent_list_params: AgentListParams,
    pub agent_list_result: AgentListResult,
    pub agent_diagnose_params: AgentDiagnoseParams,
    pub agent_diagnose_result: AgentDiagnoseResult,
    // workspace / fs / jobs
    pub workspace_list_result: WorkspaceListResult,
    pub workspace_create_params: WorkspaceCreateParams,
    pub workspace_clone_params: WorkspaceCloneParams,
    pub workspace_rename_params: WorkspaceRenameParams,
    pub workspace_name_params: WorkspaceNameParams,
    pub workspace_trash_list_result: WorkspaceTrashListResult,
    pub workspace_trash_id_params: WorkspaceTrashIdParams,
    pub workspace_changed: WorkspaceChanged,
    pub project_kind: ProjectKind,
    pub workspace_link_params: WorkspaceLinkParams,
    pub workspace_browse_params: WorkspaceBrowseParams,
    pub workspace_browse_result: WorkspaceBrowseResult,
    pub browse_entry: BrowseEntry,
    pub link_block: LinkBlock,
    pub job_started: JobStarted,
    pub job_progress: JobProgress,
    pub job_done: JobDone,
    pub job_cancel_params: JobCancelParams,
    pub fs_list_params: FsListParams,
    pub fs_list_result: FsListResult,
    pub fs_read_params: FsReadParams,
    pub fs_read_result: FsReadResult,
    // worktree / git（ADR-34）
    pub workspace_branches_params: WorkspaceBranchesParams,
    pub workspace_branches_result: WorkspaceBranchesResult,
    pub worktree_state: WorktreeState,
    pub worktree_info: WorktreeInfo,
    pub workspace_worktrees_params: WorkspaceWorktreesParams,
    pub workspace_worktrees_result: WorkspaceWorktreesResult,
    pub workspace_remove_worktree_params: WorkspaceRemoveWorktreeParams,
    pub node_worktree_settings: NodeWorktreeSettings,
    pub session_workspace: SessionWorkspace,
    pub session_workspace_mode: SessionWorkspaceMode,
    pub session_worktree: SessionWorktree,
    pub worktree_disposal: WorktreeDisposal,
    pub session_archive_params: SessionArchiveParams,
    pub session_delete_params: SessionDeleteParams,
    pub session_git_status_params: SessionGitStatusParams,
    pub session_git_status: SessionGitStatus,
    pub pull_request_state: PullRequestState,
    pub pull_request_info: PullRequestInfo,
    pub diff_against: DiffAgainst,
    pub session_diff_params: SessionDiffParams,
    pub diff_file_status: DiffFileStatus,
    pub diff_file: DiffFile,
    pub session_diff_result: SessionDiffResult,
    pub session_sync_base_params: SessionSyncBaseParams,
    pub sync_outcome: SyncOutcome,
    pub session_sync_base_result: SessionSyncBaseResult,
    pub git_action: GitAction,
    // session
    pub session_settings: SessionSettings,
    pub session_summary: SessionSummary,
    pub session_list_params: SessionListParams,
    pub session_list_result: SessionListResult,
    pub session_id_params: SessionIdParams,
    pub session_create_params: SessionCreateParams,
    pub session_send_params: SessionSendParams,
    pub session_send_result: SessionSendResult,
    pub session_set_settings_params: SessionSetSettingsParams,
    pub session_set_settings_result: SessionSetSettingsResult,
    pub session_history_params: SessionHistoryParams,
    pub session_history_result: SessionHistoryResult,
    pub session_rename_params: SessionRenameParams,
    pub session_search_params: SessionSearchParams,
    pub session_search_result: SessionSearchResult,
    pub session_promote_params: SessionPromoteParams,
    pub session_changed: SessionChanged,
    pub session_event: SessionEvent,
    pub event: Event,
    // blob（ADR-35）
    pub attachment_ref: AttachmentRef,
    pub blob_put_begin_params: BlobPutBeginParams,
    pub blob_put_begin_result: BlobPutBeginResult,
    pub blob_put_chunk_params: BlobPutChunkParams,
    pub blob_id_params: BlobIdParams,
    pub blob_get_params: BlobGetParams,
    pub blob_get_result: BlobGetResult,
    // permission
    pub permission_request: PermissionRequest,
    pub permission_respond_params: PermissionRespondParams,
    pub permission_resolved: PermissionResolved,
    pub permission_list_params: PermissionListParams,
    pub permission_list_result: PermissionListResult,
}

/// 生成 draft-07 schema（json-schema-to-typescript 和 datamodel-codegen 都吃这个）。
pub fn generate() -> serde_json::Value {
    let settings = schemars::generate::SchemaSettings::draft07();
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<ProtocolSchema>();
    let mut v = serde_json::to_value(schema).expect("schema is json");
    mark_any_as_unknown(&mut v);
    v["x-anycon-methods"] = serde_json::to_value(crate::methods::REGISTRY).expect("registry is json");
    v["x-anycon-protocol-version"] = serde_json::json!(crate::PROTOCOL_VERSION);
    v
}

/// `serde_json::Value` 在 schema 里是「任意值」：裸的 `true`，或者只带 `default` / `description` 之类注解、
/// 没有任何约束关键字的对象（schemars 1 给带 `#[serde(default)]` 的 `Value` 字段生成的就是后者）。
/// json-schema-to-typescript 会把两种都映射成 `{ [k: string]: unknown }`，这里都加上 `"tsType": "unknown"`，
/// TS 侧得到 `unknown`；其他生成器忽略 `tsType`。按 schema 的结构位置遍历，不会误伤 `properties` 这种容器。
fn mark_any_as_unknown(v: &mut serde_json::Value) {
    const CONSTRAINTS: &[&str] = &[
        "type",
        "$ref",
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "enum",
        "const",
        "properties",
        "patternProperties",
        "additionalProperties",
        "items",
        "format",
        "tsType",
    ];
    match v {
        serde_json::Value::Bool(true) => {
            *v = serde_json::json!({ "tsType": "unknown" });
        }
        serde_json::Value::Object(map) => {
            if !map.keys().any(|k| CONSTRAINTS.contains(&k.as_str())) {
                map.insert("tsType".into(), serde_json::json!("unknown"));
                return;
            }
            for key in ["properties", "patternProperties", "definitions", "$defs"] {
                if let Some(serde_json::Value::Object(children)) = map.get_mut(key) {
                    children.values_mut().for_each(mark_any_as_unknown);
                }
            }
            for key in ["additionalProperties", "items", "not"] {
                if let Some(child) = map.get_mut(key) {
                    match child {
                        serde_json::Value::Array(items) => items.iter_mut().for_each(mark_any_as_unknown),
                        serde_json::Value::Bool(false) => {}
                        other => mark_any_as_unknown(other),
                    }
                }
            }
            for key in ["oneOf", "anyOf", "allOf"] {
                if let Some(serde_json::Value::Array(items)) = map.get_mut(key) {
                    items.iter_mut().for_each(mark_any_as_unknown);
                }
            }
        }
        _ => {}
    }
}

/// 方法注册表的独立导出（`schema/methods.json`）。
pub fn generate_methods() -> serde_json::Value {
    serde_json::to_value(crate::methods::REGISTRY).expect("registry is json")
}

#[cfg(test)]
mod tests {
    #[test]
    fn schema_generates() {
        let v = super::generate();
        assert!(v.get("definitions").is_some(), "draft-07 schema should have definitions");
        assert!(v["definitions"].get("Event").is_some());
        assert!(v["x-anycon-methods"].as_array().map(|a| a.len() > 40).unwrap_or(false));
    }

    #[test]
    fn registry_types_exist_in_schema() {
        let v = super::generate();
        let defs = v["definitions"].as_object().unwrap();
        for m in crate::methods::REGISTRY {
            for t in [m.params, m.result].into_iter().flatten() {
                assert!(defs.contains_key(t), "method {} references unknown type {}", m.name, t);
            }
        }
    }
}
