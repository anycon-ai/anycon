/**
 * 方法名 → params / result 的类型表，供 `RpcPeer.request` 之上的类型化封装用。
 *
 * 由 `just gen-ts` 从 `schema/methods.json`（`crates/protocol/src/methods.rs` 的 `REGISTRY`）生成，
 * 不要手改；`tests/method-types.test.ts` 会断言它与 schema 同步。类型名来自 `generated.ts`。
 * 没有参数 / 没有结果的方法记作 `Empty`（线上是 `{}`）。
 */
import type {
  AgentDiagnoseParams,
  AgentDiagnoseResult,
  AgentListParams,
  AgentListResult,
  AttachmentRef,
  BlobGetParams,
  BlobGetResult,
  BlobIdParams,
  BlobPutBeginParams,
  BlobPutBeginResult,
  BlobPutChunkParams,
  Empty,
  EnrollInfo,
  FsListParams,
  FsListResult,
  FsReadParams,
  FsReadResult,
  JobCancelParams,
  JobDone,
  JobProgress,
  JobStarted,
  NodeAuditParams,
  NodeAuditResult,
  NodeDevicesResult,
  NodeDoctorResult,
  NodeEnrollApproveParams,
  NodeEnrollApproveResult,
  NodeEnrollCodeResult,
  NodeEnrollIdParams,
  NodeEnrollListResult,
  NodeEnrollResolved,
  NodeEnrollStatusResult,
  NodeInfo,
  NodePairBeginParams,
  NodePairBeginResult,
  NodeRevokeDeviceParams,
  NodeSecurityEvent,
  NodeSetAgentDefaultsParams,
  NodeSetEnrollEnabledParams,
  NodeSetEnrollPasswordParams,
  NodeStats,
  PairAck,
  PermissionListParams,
  PermissionListResult,
  PermissionRequest,
  PermissionResolved,
  PermissionRespondParams,
  Project,
  RenamedProject,
  SessionArchiveParams,
  SessionChanged,
  SessionCreateParams,
  SessionDeleteParams,
  SessionDiffParams,
  SessionDiffResult,
  SessionEvent,
  SessionGitStatus,
  SessionGitStatusParams,
  SessionHistoryParams,
  SessionHistoryResult,
  SessionIdParams,
  SessionListParams,
  SessionListResult,
  SessionPromoteParams,
  SessionRenameParams,
  SessionSearchParams,
  SessionSearchResult,
  SessionSendParams,
  SessionSendResult,
  SessionSetSettingsParams,
  SessionSetSettingsResult,
  SessionSummary,
  SessionSyncBaseParams,
  SessionSyncBaseResult,
  TrashEntry,
  WorkspaceBranchesParams,
  WorkspaceBranchesResult,
  WorkspaceBrowseParams,
  WorkspaceBrowseResult,
  WorkspaceChanged,
  WorkspaceCloneParams,
  WorkspaceCreateParams,
  WorkspaceLinkParams,
  WorkspaceListResult,
  WorkspaceNameParams,
  WorkspaceRemoveWorktreeParams,
  WorkspaceRenameParams,
  WorkspaceTrashIdParams,
  WorkspaceTrashListResult,
  WorkspaceWorktreesParams,
  WorkspaceWorktreesResult,
} from './generated.js'

/** C→D 请求方法。 */
export interface RequestMethods {
  'node.info': { params: Empty; result: NodeInfo }
  'node.devices': { params: Empty; result: NodeDevicesResult }
  'node.revoke_device': { params: NodeRevokeDeviceParams; result: Empty }
  'node.pair_begin': { params: NodePairBeginParams; result: NodePairBeginResult }
  'node.stats': { params: Empty; result: NodeStats }
  'node.doctor': { params: Empty; result: NodeDoctorResult }
  'node.audit': { params: NodeAuditParams; result: NodeAuditResult }
  'node.set_agent_defaults': { params: NodeSetAgentDefaultsParams; result: Empty }
  'node.enroll_list': { params: Empty; result: NodeEnrollListResult }
  'node.enroll_approve': { params: NodeEnrollApproveParams; result: NodeEnrollApproveResult }
  'node.enroll_deny': { params: NodeEnrollIdParams; result: Empty }
  'node.enroll_code': { params: Empty; result: NodeEnrollCodeResult }
  'node.enroll_status': { params: Empty; result: NodeEnrollStatusResult }
  'node.set_enroll_password': { params: NodeSetEnrollPasswordParams; result: Empty }
  'node.set_enroll_enabled': { params: NodeSetEnrollEnabledParams; result: Empty }
  'agent.list': { params: AgentListParams; result: AgentListResult }
  'agent.diagnose': { params: AgentDiagnoseParams; result: AgentDiagnoseResult }
  'workspace.list': { params: Empty; result: WorkspaceListResult }
  'workspace.create': { params: WorkspaceCreateParams; result: Project }
  'workspace.clone': { params: WorkspaceCloneParams; result: JobStarted }
  'workspace.rename': { params: WorkspaceRenameParams; result: RenamedProject }
  'workspace.trash': { params: WorkspaceNameParams; result: TrashEntry }
  'workspace.trash_list': { params: Empty; result: WorkspaceTrashListResult }
  'workspace.restore': { params: WorkspaceTrashIdParams; result: Project }
  'workspace.purge': { params: WorkspaceTrashIdParams; result: Empty }
  'workspace.link': { params: WorkspaceLinkParams; result: Project }
  'workspace.unlink': { params: WorkspaceNameParams; result: Empty }
  'workspace.browse': { params: WorkspaceBrowseParams; result: WorkspaceBrowseResult }
  'workspace.branches': { params: WorkspaceBranchesParams; result: WorkspaceBranchesResult }
  'workspace.worktrees': { params: WorkspaceWorktreesParams; result: WorkspaceWorktreesResult }
  'workspace.remove_worktree': { params: WorkspaceRemoveWorktreeParams; result: Empty }
  'job.cancel': { params: JobCancelParams; result: Empty }
  'fs.list': { params: FsListParams; result: FsListResult }
  'fs.read': { params: FsReadParams; result: FsReadResult }
  'session.list': { params: SessionListParams; result: SessionListResult }
  'session.get': { params: SessionIdParams; result: SessionSummary }
  'session.create': { params: SessionCreateParams; result: SessionSummary }
  'session.send': { params: SessionSendParams; result: SessionSendResult }
  'session.cancel': { params: SessionIdParams; result: Empty }
  'session.set_settings': { params: SessionSetSettingsParams; result: SessionSetSettingsResult }
  'session.watch': { params: SessionIdParams; result: Empty }
  'session.unwatch': { params: SessionIdParams; result: Empty }
  'session.history': { params: SessionHistoryParams; result: SessionHistoryResult }
  'session.rename': { params: SessionRenameParams; result: Empty }
  'session.archive': { params: SessionArchiveParams; result: Empty }
  'session.unarchive': { params: SessionIdParams; result: Empty }
  'session.delete': { params: SessionDeleteParams; result: Empty }
  'session.restart': { params: SessionIdParams; result: Empty }
  'session.search': { params: SessionSearchParams; result: SessionSearchResult }
  'session.promote': { params: SessionPromoteParams; result: Empty }
  'session.git_status': { params: SessionGitStatusParams; result: SessionGitStatus }
  'session.diff': { params: SessionDiffParams; result: SessionDiffResult }
  'session.sync_base': { params: SessionSyncBaseParams; result: SessionSyncBaseResult }
  'blob.put_begin': { params: BlobPutBeginParams; result: BlobPutBeginResult }
  'blob.put_chunk': { params: BlobPutChunkParams; result: Empty }
  'blob.put_end': { params: BlobIdParams; result: AttachmentRef }
  'blob.get': { params: BlobGetParams; result: BlobGetResult }
  'blob.discard': { params: BlobIdParams; result: Empty }
  'permission.respond': { params: PermissionRespondParams; result: Empty }
  'permission.list': { params: PermissionListParams; result: PermissionListResult }
}

/** D→C 通知方法（没有响应）。 */
export interface NotificationMethods {
  'pair.ack': { params: PairAck }
  'node.enroll_requested': { params: EnrollInfo }
  'node.enroll_resolved': { params: NodeEnrollResolved }
  'node.security_event': { params: NodeSecurityEvent }
  'agent.changed': { params: AgentListResult }
  'workspace.changed': { params: WorkspaceChanged }
  'job.progress': { params: JobProgress }
  'job.done': { params: JobDone }
  'session.event': { params: SessionEvent }
  'session.changed': { params: SessionChanged }
  'permission.request': { params: PermissionRequest }
  'permission.resolved': { params: PermissionResolved }
}

export type RequestMethodName = keyof RequestMethods
export type NotificationMethodName = keyof NotificationMethods
