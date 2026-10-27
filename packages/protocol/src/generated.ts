/* eslint-disable */
// 由 `just gen` 从 schema/protocol.json 生成，不要手改。

export type ModeRisk = "normal" | "safe" | "dangerous";
export type ConfigOptionKind = "bool" | "select" | "text";
export type AgentStatus =
  ("ready" | "disabled" | "not_installed" | "not_authenticated" | "unsupported_version" | "error") | "loading";
/**
 * 一个目录为什么不能关联为项目（ADR-33 修订，daemon.md § 10）。
 */
export type LinkBlock = "hidden" | "sensitive" | "workspace" | "data_dir" | "filesystem_root";
export type DeviceKind = "daemon" | "web" | "cli" | "sdk";
/**
 * 控制消息集合。方向见注释：D = 设备，H = hub。
 */
export type Control =
  | {
      client: ClientInfo;
      device_token?: string | null;
      preview_token?: string | null;
      protocol_version: number;
      type: "auth";
    }
  | {
      device_id: string;
      /**
       * 已配对且当前在线的对端。
       */
      peers_online: string[];
      quota?: Quota | null;
      server_time_ms: number;
      type: "auth_ok";
    }
  | {
      reason: AuthFailReason;
      type: "auth_fail";
    }
  | {
      match_id: string;
      type: "pair_begin";
    }
  | {
      expires_at_ms: number;
      type: "pair_registered";
    }
  | {
      match_id: string;
      type: "pair_join";
    }
  | {
      peer: string;
      role: PairRole;
      type: "pair_matched";
    }
  | {
      peer: string;
      type: "pair_confirm";
    }
  | {
      device_id?: string | null;
      device_token?: string | null;
      peer: string;
      type: "pair_done";
    }
  | {
      reason: PairFailReason;
      type: "pair_failed";
    }
  | {
      node: string;
      type: "enroll_begin";
    }
  | {
      enroll_id: string;
      expires_at_ms: number;
      node: string;
      type: "enroll_started";
    }
  | {
      enroll_id: string;
      expires_at_ms: number;
      peer: string;
      type: "enroll_request";
    }
  | {
      enroll_id: string;
      type: "enroll_cancel";
    }
  | {
      enroll_id?: string | null;
      reason: EnrollFailReason;
      type: "enroll_failed";
    }
  | {
      device: string;
      type: "revoke";
    }
  | {
      device: string;
      type: "peer_online";
    }
  | {
      device: string;
      type: "peer_offline";
    }
  | {
      label: string;
      peer: string;
      type: "preview_peer";
    }
  | {
      seq: number;
      to: string;
      type: "undeliverable";
    }
  | {
      dropped_seqs: number[];
      to: string;
      type: "queue_overflow";
    }
  | {
      seq: number;
      type: "queue_drop";
    }
  | {
      seq: number;
      to: string;
      type: "backpressure";
    }
  | {
      reason: DropReason;
      seq: number;
      type: "dropped";
    }
  | {
      retry_after_ms: number;
      to?: string | null;
      type: "rate_limited";
    }
  | {
      quota: Quota;
      type: "quota_update";
    }
  | {
      type: "rotate_token";
    }
  | {
      device_token: string;
      type: "token_rotated";
    };
export type AuthFailReason = "invalid" | "revoked" | "version" | "quota" | "protocol";
/**
 * 配对中的角色：daemon（或已配对设备代发）是 `host`，新设备是 `joiner`。
 * Noise XXpsk3 由 **joiner 发起**（initiator），host 是 responder。
 */
export type PairRole = "host" | "joiner";
export type PairFailReason = "not_found" | "timeout" | "peer_gone" | "rate_limited" | "quota" | "forbidden";
/**
 * `enroll_failed.reason`（ADR-36，protocol.md § 4.4）。
 */
export type EnrollFailReason =
  "forbidden" | "not_found" | "offline" | "rate_limited" | "quota" | "busy" | "timeout" | "peer_gone" | "cancelled";
export type DropReason = "too_large" | "not_paired" | "rate_limited";
/**
 * `session.diff` 比较的起点。
 */
export type DiffAgainst = "base" | "uncommitted";
export type DiffFileStatus = "added" | "modified" | "deleted" | "renamed";
/**
 * 接入期间 `handshake` 信封的 body。N = 新设备（initiator），D = daemon（responder）。
 */
export type EnrollFrame =
  | {
      /**
       * 仅 `code`：`true` = 请求已配对设备批准；`false` = 用「添加设备」主动生成的码。
       */
      approval?: boolean;
      hint: EnrollHint;
      method: EnrollMethod;
      /**
       * 16 字节随机数，hex。CPace 的 sid = N 的 nonce ‖ D 的 nonce。
       */
      nonce: string;
      type: "hello";
      v: number;
    }
  | {
      attempts_left?: number | null;
      /**
       * `code` / `password`：D 的 CPace 份额（32 字节，hex）。
       */
      cpace?: string | null;
      /**
       * D 的 16 字节随机数，hex。每条 `ready` 都带（和 N 本轮 `hello` 的 nonce 组成 sid）。
       */
      nonce?: string | null;
      reason?: EnrollRejectReason | null;
      retry_after_ms?: number | null;
      /**
       * 仅 `password`：节点盐（16 字节，hex）。
       */
      salt?: string | null;
      status: EnrollReplyStatus;
      type: "reply";
    }
  | {
      share: string;
      type: "cpace";
    }
  | {
      msg: string;
      type: "noise";
    };
/**
 * 接入方法。
 */
export type EnrollMethod = "scan" | "code" | "password";
/**
 * `EnrollReply.reason`（`status = rejected` 时）。
 */
export type EnrollRejectReason =
  | "method_disabled"
  | "password_not_set"
  | "backoff"
  | "locked"
  | "no_code"
  | "busy"
  | "denied"
  | "wrong_secret"
  | "expired"
  | "invalid";
/**
 * `EnrollReply.status`。
 */
export type EnrollReplyStatus = "wait_approval" | "ready" | "rejected";
/**
 * 进行中的接入会话的状态（daemon 视角）。
 */
export type EnrollSessionStatus = "waiting_hello" | "wait_approval" | "ready";
/**
 * 节点口令解锁的状态。
 */
export type EnrollPasswordState = "enabled" | "disabled" | "locked";
/**
 * `node.enroll_resolved.result`。
 */
export type EnrollResult = "approved" | "denied" | "completed" | "failed" | "expired";
export type ErrorCode =
  | ("exists" | "not_found" | "internal")
  | "invalid"
  | "invalid_settings"
  | "agent_unavailable"
  | "node_busy"
  | "busy"
  | "already_resolved"
  | "forbidden"
  | "unsupported"
  | "protocol_mismatch";
/**
 * 会话事件。adapter 把各 agent 的原生事件翻译成这个集合（docs/spec/daemon.md § 8）。
 */
export type Event =
  | {
      attachments?: AttachmentRef[];
      by_device: string;
      content: string;
      message_id: string;
      type: "user_message";
    }
  | {
      message_id: string;
      turn_id: number;
      type: "turn_start";
    }
  | {
      text: string;
      type: "text_delta";
    }
  | {
      type: "text_done";
    }
  | {
      text: string;
      type: "thinking_delta";
    }
  | {
      duration_ms: number;
      type: "thinking_done";
    }
  | {
      category: ToolCategory;
      detail?: ToolCallDetail | null;
      id: string;
      input?: unknown;
      name: string;
      /**
       * 子 agent 内部的调用：启动这个子 agent 的那次调用（`category = subagent`）的 `id`。
       * 子 agent 再启动子 agent 时一层层指上去；主 agent 自己的调用不带。
       */
      parent_id?: string | null;
      title: string;
      type: "tool_call";
    }
  | {
      id: string;
      text: string;
      type: "tool_output_delta";
    }
  | {
      detail?: ToolResultDetail | null;
      id: string;
      is_error?: boolean;
      output?: unknown;
      type: "tool_result";
    }
  | {
      kind: FileChangeKind;
      path: string;
      type: "file_changed";
    }
  | {
      entries: PlanEntry[];
      type: "plan";
    }
  | {
      detail: PermissionDetail;
      expires_at_ms?: number | null;
      kind: PermissionKind;
      options: PermissionOption[];
      request_id: string;
      title: string;
      type: "permission";
    }
  | {
      /**
       * 设备答复时带的 `answers`（kind = question：`{ [question_id]: 字符串或字符串数组 }`），原样记下，
       * 客户端据此在历史里显示问题和回答。daemon 自己答复的（超时、取消、自动拒绝）没有。
       */
      answers?: {
        [k: string]: unknown;
      } | null;
      by_device: string;
      /**
       * 设备答复时附的话（拒绝的理由、跳过问题时的说明）。daemon 自己答复的没有。
       */
      message?: string | null;
      option_id?: string | null;
      request_id: string;
      type: "permission_resolved";
    }
  | {
      reason: TurnDoneReason;
      turn_id: number;
      type: "turn_done";
      usage?: Usage | null;
    }
  | {
      position?: number | null;
      queued_messages?: number | null;
      state: SessionStatus;
      type: "status";
    }
  | {
      settings: SessionSettings;
      type: "settings_changed";
    }
  | {
      title: string;
      type: "title_changed";
    }
  | {
      action: GitAction;
      /**
       * `sync_*` 和 `worktree_created` 时给 base。
       */
      base?: string | null;
      /**
       * 会话 worktree 的分支。
       */
      branch: string;
      /**
       * `sync_merged`：合进来的提交数；`worktree_created` / `worktree_restored`：从 `.worktreeinclude` 复制的文件数。
       */
      count?: number | null;
      /**
       * `sync_conflicts`：冲突的文件（最多 50 个）。
       */
      files?: string[];
      /**
       * `worktree_removed` 时是稳定取值：`branch_deleted`（分支一起删了，或者本来就不在了）/ `branch_kept`。
       * 其他动作是补充说明（英文，调试用；比如 `worktree_failed` 的原因、`.worktreeinclude` 超过上限没复制完）。
       */
      note?: string | null;
      type: "git";
    }
  | {
      code: string;
      message: string;
      recoverable: boolean;
      type: "error";
    }
  | {
      /**
       * workflow 里的各个 agent，按 `index` 升序；非 workflow 为空。
       */
      agents?: TaskAgent[];
      /**
       * agent 给任务的 id（Claude Code 的 `task_id`；Codex 子 agent 的 thread id）。会话内唯一。
       */
      id: string;
      kind: TaskKind;
      /**
       * 任务最近一次调用的工具名。
       */
      last_tool?: string | null;
      /**
       * workflow 的阶段，按 `index` 升序；非 workflow 为空。
       */
      phases?: TaskPhase[];
      status: TaskStatus;
      /**
       * agent 给的进度 / 结果摘要。
       */
      summary?: string | null;
      /**
       * workflow 的名字 / 任务描述。
       */
      title: string;
      /**
       * 启动这个任务的 `tool_call` 的 `id`（Claude Code 的 `tool_use_id`；Codex 的 collab agent 调用 item id）。
       * 客户端据此把进度挂在那个工具块上；对不上时单独成块。
       */
      tool_id?: string | null;
      type: "task";
      usage?: TaskUsage | null;
    }
  | {
      code: string;
      level: NoticeLevel;
      /**
       * agent 给的原文（通常是英文），可能是空串。`command_output` 里是本地命令的输出，可能多行。
       */
      message: string;
      /**
       * `code = model_fallback`：原来的模型。
       */
      model_from?: string | null;
      /**
       * `code = model_fallback`：实际使用的模型。
       */
      model_to?: string | null;
      /**
       * `code = rate_limit`：限额恢复的时间。
       */
      resets_at_ms?: number | null;
      type: "notice";
    }
  | {
      /**
       * `retrying`：这是第几次重试（从 1 开始）。
       */
      attempt?: number | null;
      /**
       * `retrying`：距下一次重试的毫秒数。
       */
      delay_ms?: number | null;
      /**
       * `retrying`：最多重试几次。
       */
      max_attempts?: number | null;
      /**
       * `retrying`：触发重试的错误摘要，如 `529 overloaded_error`。
       */
      reason?: string | null;
      state: ActivityState;
      type: "activity";
    };
export type ToolCategory = "shell" | "file_edit" | "file_read" | "search" | "web" | "subagent" | "mcp" | "other";
export type EditKind = "edit" | "write" | "delete" | "move";
export type FileChangeKind = "created" | "modified" | "deleted";
export type PlanStatus = "pending" | "in_progress" | "completed";
export type PermissionKind = "tool" | "plan" | "question" | "mode_change" | "other";
/**
 * 借用 ACP 的枚举，客户端据此选图标和颜色。
 */
export type PermissionOptionKind = "allow_once" | "allow_always" | "reject_once" | "reject_always";
export type TurnDoneReason = "completed" | "cancelled" | "error" | "max_turns" | "budget";
export type SessionStatus =
  ("starting" | "running" | "waiting_permission" | "idle" | "ended" | "error") | "queued" | "interrupted";
/**
 * `Event::Git` 的动作。
 */
export type GitAction =
  | ("sync_up_to_date" | "sync_merged" | "sync_conflicts" | "sync_aborted")
  | "worktree_created"
  | "worktree_failed"
  | "worktree_removed"
  | "worktree_kept"
  | "worktree_restored"
  | "worktree_missing";
/**
 * workflow 里一个 agent 的状态。
 */
export type TaskAgentState = ("running" | "done" | "failed") | "queued" | "skipped";
/**
 * `Event::Task` 的任务种类。
 */
export type TaskKind = "other" | "workflow" | "subagent" | "shell";
/**
 * `Event::Task` 的任务状态。`Running` 以外的三个是收尾状态（终态）：Claude Code 的任务进入终态后不会再变；
 * Codex 的子 agent 是一个可以再收到输入的 thread，`Completed` 之后收到新输入、开始新 turn 就会回到 `Running`。
 * 所以终态只说明「这一刻不在跑」，不保证同一个 `id` 以后不会再有快照；客户端始终以最新的一条为准。
 */
export type TaskStatus = ("running" | "completed" | "failed") | "cancelled";
/**
 * `Event::Notice` 的级别。
 */
export type NoticeLevel = "info" | "warning";
/**
 * `Event::Activity` 的状态。
 */
export type ActivityState = "none" | "retrying" | "compacting";
/**
 * 信封种类。hub 对每种的处理见 docs/protocol.md § 3。
 */
export type Kind = "handshake" | "data" | "notify" | "control";
/**
 * 节点配置 `worktree.default` 的取值，也是 `SessionWorkspace` 的两种模式。
 */
export type SessionWorkspaceMode = "local" | "worktree";
/**
 * `node.security_event.kind`。
 */
export type SecurityEventKind =
  "device_enrolled" | "password_failures" | "password_locked" | "password_changed" | "settings_changed";
export type NotifyCategory =
  ("permission_requested" | "turn_finished" | "error" | "session_idle") | "enroll_requested" | "security";
/**
 * 隧道里响应体的编码。`gzip`：`DATA` 帧拼起来是 gzip 流，预览页要先解压；`headers` 里已经去掉
 * `content-encoding` / `content-length`。
 */
export type PreviewBodyEncoding = "identity" | "gzip";
/**
 * 端口探测是否可用。`unavailable`：这台机器上没有可用的探测手段（如没有 `lsof`），只能预览配置里允许的端口。
 */
export type PreviewDetection = "ok" | "unavailable";
export type PreviewGoawayCode = "expired" | "closed" | "revoked" | "disabled" | "port_not_allowed" | "shutdown";
/**
 * 这个端口为什么允许预览（spec/daemon.md § 19.3）。
 */
export type PreviewPortSource = "session" | "workspace" | "config";
/**
 * daemon 拒绝隧道握手的原因（握手回复 `0x01` 后面的 UTF-8 串）。这一条不受 Noise 保护，只能当提示用。
 */
export type PreviewRejectReason =
  ("expired" | "port_not_allowed" | "disabled" | "malformed") | "unknown_ticket" | "label_mismatch" | "busy";
export type PreviewResetCode =
  "internal" | "refused" | "timeout" | "too_large" | "cancelled" | "flow_control" | "protocol" | "limit";
/**
 * 项目从哪来。
 */
export type ProjectKind = "folder" | "linked";
export type PullRequestState = "open" | "draft" | "merged" | "closed";
/**
 * 一条内层消息。解码时按字段判别：有 `method` 且有 `id` 是请求，有 `method` 无 `id` 是通知，否则是响应。
 */
export type RpcMessage = RpcRequest | RpcNotification | RpcResponse;
export type RpcId = number | string;
/**
 * 归档 / 删除会话时怎么处理它的 worktree。
 */
export type WorktreeDisposal = "auto" | "keep" | "remove";
/**
 * anycon 建的 worktree 现在的状态。
 */
export type WorktreeState = "creating" | "active" | "kept" | "removed" | "missing" | "failed";
/**
 * 会话在哪里工作。`session.create` 不带时是 `local`（线路上不猜默认值；客户端按节点的
 * `worktree.default` 和用户上次的选择决定要不要带 `worktree`）。
 */
export type SessionWorkspace =
  | {
      mode: "local";
    }
  | {
      /**
       * 从哪个 ref 开分支：本地分支名、`origin/main` 这样的远端跟踪分支、标签，或提交 SHA。daemon 把分支 / 标签写法
       * 规范成完整引用名记下（`refs/heads/main`、`refs/remotes/origin/main`、`refs/tags/v1.0`；展示时去掉前缀），其他能解析成提交的
       * 写法（`HEAD`、`HEAD~1`…）只记解析出的提交，不再按名字重新解析。缺省 = `workspace.branches` 的 `default_base`。
       */
      base?: string | null;
      /**
       * 新分支的完整名字，必须是还不存在的分支。缺省 = `<worktree.branch_prefix>/<slug>`，
       * slug 由 daemon 从第一条消息里的英文单词生成，取不到就随机两个词。
       */
      branch?: string | null;
      mode: "worktree";
    };
/**
 * `session.set_settings` 里可以被「清除」的字段。`SessionSettings` 的 `Option` 字段在线路上
 * 缺省和 `null` 等价、都表示「保持不变」，所以要清掉一个值得在 `clear` 里点名；`options` 不在
 * 这里：它的键值为 `null` 就是删键。
 */
export type ClearableSetting =
  ("thinking" | "mode" | "system_prompt_append" | "max_turns" | "max_budget_usd") | "model" | "env";
export type SyncOutcome = "up_to_date" | "merged" | "conflicts" | "aborted";

/**
 * 仅用于导出 schema，不在线上传输。字段名即 TS / Python 的类型名。
 */
export interface ProtocolSchema {
  agent_diagnose_params: AgentDiagnoseParams;
  agent_diagnose_result: AgentDiagnoseResult;
  agent_info: AgentInfo;
  agent_list_params: AgentListParams;
  agent_list_result: AgentListResult;
  attachment_ref: AttachmentRef;
  blob_get_params: BlobGetParams;
  blob_get_result: BlobGetResult;
  blob_id_params: BlobIdParams;
  blob_put_begin_params: BlobPutBeginParams;
  blob_put_begin_result: BlobPutBeginResult;
  blob_put_chunk_params: BlobPutChunkParams;
  browse_entry: BrowseEntry;
  client_info: ClientInfo;
  control: Control;
  diff_against: DiffAgainst;
  diff_file: DiffFile;
  diff_file_status: DiffFileStatus;
  empty: Empty;
  enroll_fail_reason: EnrollFailReason;
  enroll_frame: EnrollFrame;
  enroll_hint: EnrollHint;
  enroll_info: EnrollInfo;
  enroll_method: EnrollMethod;
  enroll_password_state: EnrollPasswordState;
  enroll_password_status: EnrollPasswordStatus;
  enroll_reject_reason: EnrollRejectReason;
  enroll_reply_status: EnrollReplyStatus;
  enroll_result: EnrollResult;
  enroll_session_status: EnrollSessionStatus;
  error_code: ErrorCode;
  event: Event;
  fs_list_params: FsListParams;
  fs_list_result: FsListResult;
  fs_read_params: FsReadParams;
  fs_read_result: FsReadResult;
  git_action: GitAction;
  job_cancel_params: JobCancelParams;
  job_done: JobDone;
  job_progress: JobProgress;
  job_started: JobStarted;
  kind: Kind;
  link_block: LinkBlock;
  node_audit_params: NodeAuditParams;
  node_audit_result: NodeAuditResult;
  node_devices_result: NodeDevicesResult;
  node_doctor_result: NodeDoctorResult;
  node_enroll_approve_params: NodeEnrollApproveParams;
  node_enroll_approve_result: NodeEnrollApproveResult;
  node_enroll_code_result: NodeEnrollCodeResult;
  node_enroll_id_params: NodeEnrollIdParams;
  node_enroll_list_result: NodeEnrollListResult;
  node_enroll_resolved: NodeEnrollResolved;
  node_enroll_status_result: NodeEnrollStatusResult;
  node_info: NodeInfo;
  node_pair_begin_params: NodePairBeginParams;
  node_pair_begin_result: NodePairBeginResult;
  node_revoke_device_params: NodeRevokeDeviceParams;
  node_security_event: NodeSecurityEvent;
  node_set_agent_defaults_params: NodeSetAgentDefaultsParams;
  node_set_enroll_enabled_params: NodeSetEnrollEnabledParams;
  node_set_enroll_password_params: NodeSetEnrollPasswordParams;
  node_stats: NodeStats;
  node_worktree_settings: NodeWorktreeSettings;
  notify_body: NotifyBody;
  notify_detail: NotifyDetail;
  pair_ack: PairAck;
  permission_list_params: PermissionListParams;
  permission_list_result: PermissionListResult;
  permission_request: PermissionRequest;
  permission_resolved: PermissionResolved;
  permission_respond_params: PermissionRespondParams;
  preview_body_encoding: PreviewBodyEncoding;
  preview_close_params: PreviewCloseParams;
  preview_detection: PreviewDetection;
  preview_goaway: PreviewGoaway;
  preview_goaway_code: PreviewGoawayCode;
  preview_hello: PreviewHello;
  preview_info: PreviewInfo;
  preview_launch: PreviewLaunch;
  preview_list_result: PreviewListResult;
  preview_open_params: PreviewOpenParams;
  preview_open_result: PreviewOpenResult;
  preview_port: PreviewPort;
  preview_port_source: PreviewPortSource;
  preview_ports_params: PreviewPortsParams;
  preview_ports_result: PreviewPortsResult;
  preview_reject_reason: PreviewRejectReason;
  preview_request_head: PreviewRequestHead;
  preview_reset: PreviewReset;
  preview_reset_code: PreviewResetCode;
  preview_response_head: PreviewResponseHead;
  project: Project;
  project_kind: ProjectKind;
  pull_request_info: PullRequestInfo;
  pull_request_state: PullRequestState;
  quota: Quota;
  rpc_error: RpcError;
  rpc_message: RpcMessage;
  security_event_kind: SecurityEventKind;
  session_archive_params: SessionArchiveParams;
  session_changed: SessionChanged;
  session_create_params: SessionCreateParams;
  session_delete_params: SessionDeleteParams;
  session_diff_params: SessionDiffParams;
  session_diff_result: SessionDiffResult;
  session_event: SessionEvent;
  session_git_status: SessionGitStatus;
  session_git_status_params: SessionGitStatusParams;
  session_history_params: SessionHistoryParams;
  session_history_result: SessionHistoryResult;
  session_id_params: SessionIdParams;
  session_list_params: SessionListParams;
  session_list_result: SessionListResult;
  session_promote_params: SessionPromoteParams;
  session_rename_params: SessionRenameParams;
  session_search_params: SessionSearchParams;
  session_search_result: SessionSearchResult;
  session_send_params: SessionSendParams;
  session_send_result: SessionSendResult;
  session_set_settings_params: SessionSetSettingsParams;
  session_set_settings_result: SessionSetSettingsResult;
  session_settings: SessionSettings;
  session_summary: SessionSummary;
  session_sync_base_params: SessionSyncBaseParams;
  session_sync_base_result: SessionSyncBaseResult;
  session_workspace: SessionWorkspace;
  session_workspace_mode: SessionWorkspaceMode;
  session_worktree: SessionWorktree;
  sync_outcome: SyncOutcome;
  trash_entry: TrashEntry;
  workspace_branches_params: WorkspaceBranchesParams;
  workspace_branches_result: WorkspaceBranchesResult;
  workspace_browse_params: WorkspaceBrowseParams;
  workspace_browse_result: WorkspaceBrowseResult;
  workspace_changed: WorkspaceChanged;
  workspace_clone_params: WorkspaceCloneParams;
  workspace_create_params: WorkspaceCreateParams;
  workspace_link_params: WorkspaceLinkParams;
  workspace_list_result: WorkspaceListResult;
  workspace_name_params: WorkspaceNameParams;
  workspace_remove_worktree_params: WorkspaceRemoveWorktreeParams;
  workspace_rename_params: WorkspaceRenameParams;
  workspace_trash_id_params: WorkspaceTrashIdParams;
  workspace_trash_list_result: WorkspaceTrashListResult;
  workspace_worktrees_params: WorkspaceWorktreesParams;
  workspace_worktrees_result: WorkspaceWorktreesResult;
  worktree_disposal: WorktreeDisposal;
  worktree_info: WorktreeInfo;
  worktree_state: WorktreeState;
}
export interface AgentDiagnoseParams {
  id: string;
}
export interface AgentDiagnoseResult {
  /**
   * 结构化：命令来源、daemon PATH、shell、which -a、解析路径、版本、认证、模型数、状态。
   */
  fields: DiagnosticField[];
  id: string;
  /**
   * 人类可读全文（`anycon doctor` 直接打印）。
   */
  text: string;
}
export interface DiagnosticField {
  label: string;
  value: string;
}
export interface AgentInfo {
  allow_custom_model?: boolean;
  capabilities?: AgentCapabilities;
  /**
   * `claude-stream-json` | `codex-app-server` | `acp` | `sdk` | `mock`
   */
  driver: string;
  enabled: boolean;
  /**
   * adapter id：`claude-code`、`codex`、`mock`、ACP agent 用自己的名字。
   */
  id: string;
  issue?: AgentIssue | null;
  label: string;
  models?: ModelInfo[];
  modes?: ModeInfo[];
  options?: ConfigOption[];
  path?: string | null;
  probed_at_ms: number;
  status: AgentStatus;
  version?: string | null;
}
export interface AgentCapabilities {
  /**
   * 可接受的附件 mime，如 `image/png`。
   */
  attachments?: string[];
  /**
   * 支持每会话 env。
   */
  env: boolean;
  /**
   * turn 进行中能否插话。
   */
  interject: boolean;
  interrupt: boolean;
  /**
   * 进程退出后能否续接。
   */
  resume: boolean;
  rewind: boolean;
  structured_output: boolean;
  /**
   * 会不会发 `thinking_delta`。
   */
  thinking_stream: boolean;
  /**
   * `turn_done` 带不带 usage。
   */
  usage: boolean;
}
export interface AgentIssue {
  /**
   * 稳定码，如 `binary_not_found`、`login_required`、`version_too_old`、`probe_timeout`。
   */
  code: string;
  /**
   * 修复建议，通常是一条可复制的命令。
   */
  hint?: string | null;
  message: string;
}
export interface ModelInfo {
  default?: boolean;
  description?: string | null;
  /**
   * 原样传给 agent 的字符串。
   */
  id: string;
  label: string;
  /**
   * 如 `1m`。
   */
  tags?: string[];
  thinking?: ThinkingOptions | null;
}
/**
 * 某个模型支持的思考等级。**挂在模型上**，不同模型不同。
 */
export interface ThinkingOptions {
  default?: string | null;
  options: Choice[];
}
export interface Choice {
  description?: string | null;
  id: string;
  label: string;
}
/**
 * 权限模式。id 是 agent 自己的（Claude Code 的 `acceptEdits`、Codex 的 `workspace`…），不翻译不归一。
 */
export interface ModeInfo {
  default?: boolean;
  description?: string | null;
  id: string;
  label: string;
  risk: ModeRisk;
}
/**
 * agent 特定的其他选项（Codex fast mode、ACP 自定义 configOptions…）。客户端用通用控件渲染。
 */
export interface ConfigOption {
  choices?: Choice[];
  default?: unknown;
  description?: string | null;
  id: string;
  kind: ConfigOptionKind;
  label: string;
}
export interface AgentListParams {
  /**
   * 等于当前 etag 时返回 `not_modified = true` 且 `agents` 为空。
   */
  if_none_match?: string | null;
  /**
   * 强制重探（含模型列表那一步）。
   */
  refresh?: boolean;
}
export interface AgentListResult {
  agents: AgentInfo[];
  etag: string;
  not_modified?: boolean;
}
/**
 * 消息里的一个附件（ADR-35）：指向 daemon 上已经传完的 blob。客户端从 `blob.put_end` 拿到它、原样放进
 * `session.send` / `session.create`；daemon 以自己记录的元数据为准写进 `user_message` 事件。
 */
export interface AttachmentRef {
  blob_id: string;
  bytes: number;
  height?: number | null;
  mime: string;
  name: string;
  /**
   * 图片像素宽高（daemon 读文件头得出；读不出来时省略）。客户端用来在下载前占好位置。
   */
  width?: number | null;
}
/**
 * `blob.get`：按偏移读一段。
 */
export interface BlobGetParams {
  blob_id: string;
  /**
   * 缺省和上限都是 512 KiB。
   */
  max_bytes?: number | null;
  offset?: number;
}
export interface BlobGetResult {
  /**
   * daemon 记录的元数据（和 `user_message.attachments` 里的一致）。
   */
  blob: AttachmentRef;
  /**
   * 标准 base64（带 padding）。
   */
  data: string;
  /**
   * 这一段读到了文件末尾。
   */
  eof: boolean;
  offset: number;
}
export interface BlobIdParams {
  blob_id: string;
}
/**
 * `blob.put_begin`：开始一次上传。
 */
export interface BlobPutBeginParams {
  /**
   * 只接受 `image/png` `image/jpeg` `image/gif` `image/webp`，其它 → `unsupported`。
   */
  mime: string;
  /**
   * 展示用的文件名（1–255 字节，不能含 `/` `\` 和控制字符）。daemon 不用它拼路径。
   */
  name: string;
  /**
   * 整个文件的 SHA-256，64 位小写 hex；`put_end` 校验。
   */
  sha256: string;
  /**
   * 总字节数，`1 ..= sessions.max_attachment_bytes`。
   */
  size: number;
}
export interface BlobPutBeginResult {
  /**
   * 32 位小写 hex。
   */
  blob_id: string;
  /**
   * 分块字节数（目前 512 KiB）。除最后一块外每块恰好这么大；块数 = `ceil(size / chunk_size)`。
   */
  chunk_size: number;
  /**
   * 这次上传在此之前没有 `put_end` 就作废（已收的分块丢弃）。每收到一块顺延。
   */
  expires_at_ms: number;
}
/**
 * `blob.put_chunk`：一块数据。偏移 = `index * chunk_size`。同一块重发是幂等的。
 */
export interface BlobPutChunkParams {
  blob_id: string;
  /**
   * 标准 base64（带 padding）。解码后的长度必须正好是这一块应有的长度。
   */
  data: string;
  index: number;
}
export interface BrowseEntry {
  /**
   * 不能关联的原因（已经是项目时为空，看 `project`）。
   */
  blocked?: LinkBlock | null;
  /**
   * 目录下有 `.git`。
   */
  git?: boolean;
  /**
   * 能不能关联：不是项目，并且 `blocked` 为空。
   */
  linkable: boolean;
  name: string;
  /**
   * 绝对路径，可以直接传给 `workspace.browse` / `workspace.link`。
   */
  path: string;
  /**
   * 已经是项目（根目录下的或已关联的）时给出项目名。
   */
  project?: string | null;
}
/**
 * 设备自报信息。配对和每次 `auth` 都带。
 */
export interface ClientInfo {
  kind: DeviceKind;
  /**
   * 显示名，如 "mac-mini"、"Chrome · macOS · hub.example.com"。
   */
  name: string;
  /**
   * 仅 web：访问 hub 的 origin，用于区分局域网 / 公网两份设备。
   */
  origin?: string | null;
  /**
   * 如 "macos-aarch64"、"linux-x86_64"、"browser"。
   */
  platform: string;
  /**
   * 客户端版本。
   */
  version: string;
}
export interface Quota {
  connections: number;
  connections_max?: number | null;
  daemons: number;
  daemons_max?: number | null;
  devices: number;
  devices_max?: number | null;
}
export interface DiffFile {
  additions: number;
  binary?: boolean;
  deletions: number;
  /**
   * 改名前的路径（`status = renamed`）。
   */
  old_path?: string | null;
  /**
   * 只在请求带 `path` 时给：这个文件的 unified diff（含 `diff --git` 头），最多 256 KiB。
   */
  patch?: string | null;
  /**
   * `patch` 被截断了。
   */
  patch_truncated?: boolean;
  /**
   * 仓库内的相对路径（`/` 分隔）。
   */
  path: string;
  status: DiffFileStatus;
  /**
   * 未跟踪的新文件（`status = added`）。
   */
  untracked?: boolean;
}
/**
 * 空参数 / 空结果。
 */
export interface Empty {}
/**
 * 新设备自报的提示信息。**未经认证**（hub 能改），只用于批准界面；认证过的 `ClientInfo` 以 XXpsk3 第 3 条为准。
 */
export interface EnrollHint {
  kind: DeviceKind;
  name: string;
  platform: string;
}
/**
 * 一条进行中的接入（`node.enroll_list` 的元素、`node.enroll_requested` 的参数）。
 */
export interface EnrollInfo {
  approval?: boolean;
  created_at_ms: number;
  enroll_id: string;
  expires_at_ms: number;
  /**
   * 新设备自报，未经认证。还没收到 `hello` 时为空。
   */
  hint?: EnrollHint | null;
  /**
   * 还没收到 `hello` 时为空。
   */
  method?: EnrollMethod | null;
  status: EnrollSessionStatus;
}
export interface EnrollPasswordStatus {
  /**
   * 连续失败次数。
   */
  failures: number;
  /**
   * 退避中：还要等多久才接受下一次尝试。
   */
  retry_after_ms?: number | null;
  /**
   * 是否设了口令。
   */
  set: boolean;
  set_at_ms?: number | null;
  /**
   * 设置者：设备 id，本机 CLI 设置的是 `"local"`。
   */
  set_by?: string | null;
  state: EnrollPasswordState;
}
/**
 * 按 category 归一化的渲染字段。全部可选；`input` 仍原样保留。
 */
export interface ToolCallDetail {
  command?: string | null;
  cwd?: string | null;
  description?: string | null;
  edit_kind?: EditKind | null;
  path?: string | null;
  query?: string | null;
  server?: string | null;
  tool?: string | null;
  url?: string | null;
}
export interface ToolResultDetail {
  /**
   * 调用已经返回，但它启动的工作还在后台跑（后台子 agent）。结束时同一个 `id` 会再来一条
   * `tool_result`（不带这个标记），取代这一条。
   */
  background?: boolean | null;
  exit_code?: number | null;
  /**
   * 这次调用的流式 `tool_output_delta` 累计超过上限、后续 delta 没有转发；最终 `output` 仍是
   * 头尾截断的完整输出。
   */
  live_output_truncated?: boolean | null;
  match_count?: number | null;
  /**
   * daemon 对 `output` 做头尾截断时丢掉的字节数（spec/daemon.md § 12）；没截断就缺省。
   */
  output_truncated_bytes?: number | null;
  /**
   * 子 agent 结束时 agent 报的它一共用掉的 token（`category = subagent`；agent 不报就缺省）。
   * 主 agent 的 `turn_done.usage` 不含这部分。
   */
  total_tokens?: number | null;
  unified_diff?: string | null;
}
export interface PlanEntry {
  content: string;
  status: PlanStatus;
}
export interface PermissionDetail {
  /**
   * kind = mode_change：目标模式 id。
   */
  mode?: string | null;
  /**
   * kind = question
   */
  questions?: Question[];
  /**
   * kind = plan：计划全文（Markdown）。kind = other：说明。
   */
  text?: string | null;
  /**
   * kind = tool
   */
  tool?: ToolPermissionDetail | null;
  /**
   * kind = other：要用户在浏览器里打开的链接（MCP server 的 URL 模式 elicitation）。只会是 `http` / `https`；
   * 客户端只对这两种协议提供「打开」入口，并且显示完整 URL。
   */
  url?: string | null;
}
export interface Question {
  choices?: QuestionChoice[];
  free_text?: boolean;
  id: string;
  multi?: boolean;
  /**
   * 可以不答：客户端允许留空提交，答复里就没有这一题的答案。默认 `false`（必答），
   * 所以 AskUserQuestion 这类每题都要答的行为不变；MCP elicitation 表单里 schema 没列进 `required` 的字段是 `true`。
   */
  optional?: boolean;
  text: string;
}
export interface QuestionChoice {
  id: string;
  label: string;
}
/**
 * `kind = tool` 的详情：和 `tool_call` 同一套字段。
 */
export interface ToolPermissionDetail {
  category: ToolCategory;
  cwd?: string | null;
  detail?: ToolCallDetail | null;
  input?: unknown;
  tool: string;
}
export interface PermissionOption {
  /**
   * adapter 定义；Claude Code 固定 `allow` / `allow_session` / `deny`，ACP 透传 optionId。
   */
  id: string;
  kind: PermissionOptionKind;
  label: string;
  /**
   * 可附一句话给 agent（通常 reject 类）。
   */
  needs_message?: boolean;
}
export interface Usage {
  cache_read_tokens?: number | null;
  cache_write_tokens?: number | null;
  /**
   * turn 结束时上下文已经占用的 token：最近一次模型请求的输入 + 缓存读写。和 `input_tokens` 等
   * 累计用量不同，它是「此刻窗口里有多少」，客户端拿它和 `context_window` 算占用比例。agent 不报就缺省。
   */
  context_tokens?: number | null;
  /**
   * 模型的上下文窗口大小（token）。agent 不报就缺省。
   */
  context_window?: number | null;
  cost_usd?: number | null;
  duration_ms: number;
  input_tokens?: number | null;
  model?: string | null;
  output_tokens?: number | null;
}
/**
 * 会话设置。`session.create` 和 `session.set_settings` 都收它；值来自 `AgentInfo`。
 */
export interface SessionSettings {
  env?: {
    [k: string]: string;
  };
  max_budget_usd?: number | null;
  max_turns?: number | null;
  /**
   * `AgentInfo.modes[].id`。
   */
  mode?: string | null;
  model?: string | null;
  /**
   * `AgentInfo.options[]` 里的，按 id。
   */
  options?: {
    [k: string]: unknown;
  };
  system_prompt_append?: string | null;
  /**
   * 所选模型的 `thinking.options[].id`。
   */
  thinking?: string | null;
}
/**
 * workflow 里的一个 agent。
 */
export interface TaskAgent {
  /**
   * 第几次尝试（重试过才有）。
   */
  attempt?: number | null;
  /**
   * 结果来自缓存，没有真的运行。
   */
  cached?: boolean | null;
  duration_ms?: number | null;
  /**
   * `state = failed` 时的错误信息。
   */
  error?: string | null;
  /**
   * 在 workflow 里的序号，同一个任务内唯一。
   */
  index: number;
  label: string;
  /**
   * 最近一次调用的工具名。
   */
  last_tool?: string | null;
  model?: string | null;
  /**
   * 所属阶段的 `TaskPhase.index`。
   */
  phase_index?: number | null;
  state: TaskAgentState;
  tokens?: number | null;
  tool_calls?: number | null;
}
/**
 * workflow 的一个阶段。
 */
export interface TaskPhase {
  index: number;
  title: string;
}
/**
 * 任务的用量。每个字段 agent 不报就缺省。
 */
export interface TaskUsage {
  duration_ms?: number | null;
  tool_calls?: number | null;
  total_tokens?: number | null;
}
export interface FsListParams {
  /**
   * `<项目名>/<项目内的相对路径>`，如 `my-app/src`。第一段是关联项目的名字时，其余部分相对那个目录解析，
   * 并且不能走出它；否则相对 workspace 根解析（`""` 列根目录本身，不含关联项目）。
   */
  path: string;
}
export interface FsListResult {
  entries: FsEntry[];
}
export interface FsEntry {
  bytes: number;
  is_dir: boolean;
  modified_at_ms: number;
  name: string;
}
export interface FsReadParams {
  /**
   * 默认 1 MiB，上限 10 MiB。
   */
  max_bytes?: number | null;
  /**
   * 解析规则同 `FsListParams.path`。
   */
  path: string;
}
export interface FsReadResult {
  binary: boolean;
  bytes: number;
  /**
   * UTF-8 文本；二进制文件 `text` 为空且 `binary = true`。
   */
  text: string;
  truncated: boolean;
}
export interface JobCancelParams {
  job_id: string;
}
export interface JobDone {
  error?: string | null;
  job_id: string;
  ok: boolean;
}
export interface JobProgress {
  job_id: string;
  log_line?: string | null;
  percent?: number | null;
  stage: string;
}
export interface JobStarted {
  job_id: string;
}
export interface NodeAuditParams {
  device_id?: string | null;
  limit?: number | null;
  since_ms?: number | null;
}
export interface NodeAuditResult {
  entries: AuditEntry[];
}
export interface AuditEntry {
  action: string;
  detail?: unknown;
  device_id: string;
  device_name: string;
  target?: string | null;
  ts_ms: number;
}
export interface NodeDevicesResult {
  devices: PeerDevice[];
}
/**
 * 已配对的客户端设备（daemon 视角）。
 */
export interface PeerDevice {
  device_id: string;
  /**
   * Noise 静态公钥的指纹（短 hex）。
   */
  fingerprint: string;
  kind: DeviceKind;
  last_seen_ms?: number | null;
  name: string;
  online: boolean;
  paired_at_ms: number;
}
export interface NodeDoctorResult {
  checks: DoctorCheck[];
}
export interface DoctorCheck {
  detail: string;
  fix?: string | null;
  id: string;
  ok: boolean;
}
/**
 * `node.enroll_approve` 参数。
 */
export interface NodeEnrollApproveParams {
  enroll_id: string;
  /**
   * `scan` 必填：二维码里的 32 字节密钥（hex）。`code` 不带。
   */
  secret?: string | null;
}
/**
 * `node.enroll_approve` 结果。`code` 请求返回 6 位码（只给批准的这一台）；`scan` 两个字段都为空。
 */
export interface NodeEnrollApproveResult {
  code?: string | null;
  expires_at_ms?: number | null;
}
/**
 * `node.enroll_code` 结果：主动生成的 6 位接入码。
 */
export interface NodeEnrollCodeResult {
  code: string;
  expires_at_ms: number;
}
/**
 * `node.enroll_deny` 参数。
 */
export interface NodeEnrollIdParams {
  enroll_id: string;
}
/**
 * `node.enroll_list` 结果。
 */
export interface NodeEnrollListResult {
  enrolls: EnrollInfo[];
}
/**
 * D→C notification `node.enroll_resolved`：发给所有在线的已配对设备，让别的设备收起批准卡片。
 */
export interface NodeEnrollResolved {
  /**
   * 批准 / 拒绝的设备；daemon 自己判定的（过期、失败、完成）为空。
   */
  by_device?: string | null;
  enroll_id: string;
  result: EnrollResult;
}
/**
 * `node.enroll_status` 结果。
 */
export interface NodeEnrollStatusResult {
  /**
   * `enroll.approval`：是否接受扫码批准和接入码。
   */
  approval: boolean;
  password: EnrollPasswordStatus;
}
/**
 * `node.info` 结果。
 */
export interface NodeInfo {
  capabilities: NodeCapabilities;
  name: string;
  node_id: string;
  platform: string;
  protocol_version: number;
  started_at_ms: number;
  version: string;
  workspace_root?: string | null;
  /**
   * `capabilities.worktree` 为 true 时给出。
   */
  worktree?: NodeWorktreeSettings | null;
}
export interface NodeCapabilities {
  audit: boolean;
  blobs: boolean;
  fs: boolean;
  /**
   * 支持 dev server 预览（`preview.*`，ADR-39），且节点配置 `preview.enabled = true`。
   */
  preview?: boolean;
  search: boolean;
  /**
   * 有 workspace（文件夹即项目）。SDK 节点可能没有。
   */
  workspace: boolean;
  /**
   * 支持每会话 worktree、`session.git_status` / `diff` / `sync_base`（ADR-34）。daemon 找不到 `git` 时为 false。
   */
  worktree?: boolean;
}
/**
 * 节点的 worktree 配置（`config.toml` 的 `[worktree]`），客户端据此决定新建会话的初始选择和分支名预览。
 */
export interface NodeWorktreeSettings {
  /**
   * 新分支名的前缀（`anycon` → `anycon/<slug>`）；空串 = 不加前缀。
   */
  branch_prefix: string;
  /**
   * git 项目里新会话默认在哪里工作。
   */
  default_mode: SessionWorkspaceMode;
}
export interface NodePairBeginParams {
  /**
   * `true`（缺省）= 16 位短码（约 79 bit；链接、二维码、默认都是它）；`false` = 8 位（约 40 bit），
   * 只适合在自建 hub 上手输（ADR-29 修订：拿到 match_id 的恶意 hub 可以离线穷举 8 位短码）。
   */
  long?: boolean;
}
/**
 * `node.pair_begin` 结果。短码由 daemon 本地生成，hub 只拿到 match id。
 */
export interface NodePairBeginResult {
  /**
   * `XXXX-XXXX` 或 `XXXX-XXXX-XXXX-XXXX`。
   */
  code: string;
  expires_at_ms: number;
  /**
   * `${hub_public_url}/pair#code=XXXX-XXXX`。短码放在 URL fragment 里，浏览器不会把它发给 hub。
   */
  link: string;
}
export interface NodeRevokeDeviceParams {
  device_id: string;
}
/**
 * D→C notification `node.security_event`（同时发 `notify { category: security }`）。
 */
export interface NodeSecurityEvent {
  /**
   * 触发的已配对设备（改口令、改设置）；本机 CLI 为 `"local"`；接入、失败为空。
   */
  by_device?: string | null;
  /**
   * `device_enrolled`：新设备（认证过的）名称和 id。
   */
  device_id?: string | null;
  device_name?: string | null;
  /**
   * `password_failures` / `password_locked`：当时的连续失败次数。
   */
  failures?: number | null;
  kind: SecurityEventKind;
  method?: EnrollMethod | null;
  /**
   * `password_changed`：`true` = 设置 / 修改，`false` = 清除。
   */
  password_set?: boolean | null;
  ts_ms: number;
}
/**
 * `node.set_agent_defaults`：写 `config.agents.<id>.default_*`。
 */
export interface NodeSetAgentDefaultsParams {
  agent: string;
  mode?: string | null;
  model?: string | null;
  options?: {
    [k: string]: unknown;
  } | null;
  thinking?: string | null;
}
/**
 * `node.set_enroll_enabled` 参数：写 `[enroll]` 配置。`password: true` 同时解除锁定、失败计数清零。
 */
export interface NodeSetEnrollEnabledParams {
  approval?: boolean | null;
  password?: boolean | null;
}
/**
 * `node.set_enroll_password` 参数。`password` 省略 = 清除。至少 8 个字符（按 Unicode 字符计，NFC 之后）。
 */
export interface NodeSetEnrollPasswordParams {
  password?: string | null;
}
export interface NodeStats {
  agent_processes: number;
  db_bytes: number;
  disk_free_bytes: number;
  hub_connected: boolean;
  peers_online: number;
  sessions: SessionCounts;
  uptime_ms: number;
}
export interface SessionCounts {
  ended: number;
  error: number;
  idle: number;
  queued: number;
  running: number;
  waiting_permission: number;
}
/**
 * `kind = notify` 的 body（msgpack）。`category` 是外层唯一有语义的明文字段。
 */
export interface NotifyBody {
  category: NotifyCategory;
  /**
   * Noise 密文详情，base64。客户端解密后得到 [`crate::NotifyDetail`]。
   */
  detail?: string | null;
  /**
   * 会话 id，明文。随机不透明串，只用于客户端点开通知后直达。
   */
  session_id?: string | null;
}
/**
 * 通知的密文详情（JSON，解密后）。
 */
export interface NotifyDetail {
  body: string;
  session_id: string;
  title: string;
}
/**
 * `pair.ack`：握手完成且 `PairDone` 之后，daemon 经新建立的 Noise 传输发给 joiner 的第一条通知。
 * joiner 校验 `peer_id == 自己的正式 id`、`node_id == PairDone.peer` 后才落盘节点。
 */
export interface PairAck {
  /**
   * daemon 静态公钥指纹（joiner 可与握手得到的对比）。
   */
  fingerprint: string;
  name: string;
  /**
   * daemon 的正式 device id。
   */
  node_id: string;
  /**
   * hub 告诉 daemon 的 joiner 正式 id。
   */
  peer_id: string;
  version: string;
}
export interface PermissionListParams {
  session_id?: string | null;
}
export interface PermissionListResult {
  requests: PermissionRequest[];
}
/**
 * `permission.request` 通知 / `permission.list` 的元素。
 */
export interface PermissionRequest {
  created_at_ms: number;
  detail: PermissionDetail;
  expires_at_ms?: number | null;
  kind: PermissionKind;
  options: PermissionOption[];
  request_id: string;
  session_id: string;
  title: string;
}
export interface PermissionResolved {
  by_device: string;
  option_id?: string | null;
  request_id: string;
  session_id: string;
}
export interface PermissionRespondParams {
  /**
   * kind = question：`{ [question_id]: value }`，value 是字符串或字符串数组。
   */
  answers?: {
    [k: string]: unknown;
  } | null;
  message?: string | null;
  /**
   * kind = question 时可空。
   */
  option_id?: string | null;
  request_id: string;
}
/**
 * `preview.close` 参数：作废这个 label 的全部票据，断开它的全部隧道。
 */
export interface PreviewCloseParams {
  label: string;
}
/**
 * 隧道 `GOAWAY` 帧的载荷（msgpack，只有 daemon 发，stream 0）：daemon 要关掉这条隧道，之后不再处理它的帧。
 */
export interface PreviewGoaway {
  code: PreviewGoawayCode;
  message?: string | null;
}
/**
 * 隧道握手第二条消息（daemon → 预览页）的 payload，JSON。到这一条预览页才确认对端持有票据、是钉住的 daemon。
 */
export interface PreviewHello {
  expires_at_ms: number;
  port: number;
  session_id?: string | null;
}
/**
 * `preview.list` 的一项：一张未过期的票据。
 */
export interface PreviewInfo {
  created_at_ms: number;
  /**
   * 签发这张票据的设备。
   */
  created_by: string;
  created_by_name: string;
  expires_at_ms: number;
  label: string;
  port: number;
  session_id?: string | null;
  ticket_id: string;
  /**
   * 此刻用这张票据建立的隧道数。
   */
  tunnels: number;
}
/**
 * 预览页的启动参数。Web UI 把它序列化成 JSON、再 base64url（无填充），放进
 * `<预览 origin>/__anycon/enter#<…>` 的 fragment（浏览器不会把 fragment 发给 hub）。引导页读出后存进预览 origin 的
 * IndexedDB，立即从地址栏抹掉。
 */
export interface PreviewLaunch {
  /**
   * 票据和令牌里较早的那个过期时间。
   */
  expires_at_ms: number;
  label: string;
  /**
   * daemon 的设备 id。
   */
  node: string;
  /**
   * daemon 的 Noise 静态公钥，64 个小写 hex。预览页用它做 NK 握手（钉住 daemon）。
   */
  node_public: string;
  /**
   * 打开后先导航到的路径（以 `/` 开头，可带查询串）。
   */
  path: string;
  port: number;
  psk: string;
  ticket_id: string;
  /**
   * hub 签发的预览令牌（`POST /api/preview/tokens`），预览连接凭它向 hub 认证。
   */
  token: string;
  /**
   * 格式版本，目前是 1。
   */
  v: number;
}
export interface PreviewListResult {
  previews: PreviewInfo[];
}
/**
 * `preview.open` 参数：为本机 `port` 签发一张预览票据。
 */
export interface PreviewOpenParams {
  port: number;
  /**
   * 从哪个会话发起（只用于界面展示和审计；端口是否允许不看它，见 spec/daemon.md § 19.3）。
   */
  session_id?: string | null;
}
/**
 * `preview.open` 结果。`psk` 是票据的密钥，只交给预览页（放进 [`PreviewLaunch`]），不要写日志、不要发给 hub。
 */
export interface PreviewOpenResult {
  expires_at_ms: number;
  /**
   * 这个端口固定的预览 label（26 位小写 base32），预览 origin 是 `<label>.<预览域名>`。
   */
  label: string;
  port: number;
  /**
   * 票据密钥，64 个小写 hex（32 字节），Noise PSK。
   */
  psk: string;
  session_id?: string | null;
  /**
   * 票据 id，32 个小写 hex（16 字节）。
   */
  ticket_id: string;
}
export interface PreviewPort {
  pid?: number | null;
  port: number;
  /**
   * 进程名（可执行文件名，不含参数）。
   */
  process?: string | null;
  session_id?: string | null;
  source: PreviewPortSource;
}
/**
 * `preview.ports` 参数。带 `session_id` 只列这个会话的端口（外加配置里允许的端口）。
 */
export interface PreviewPortsParams {
  session_id?: string | null;
}
export interface PreviewPortsResult {
  detection: PreviewDetection;
  ports: PreviewPort[];
}
/**
 * 隧道 `REQUEST` 帧的载荷（msgpack，字段名为键）。
 */
export interface PreviewRequestHead {
  /**
   * 请求头，按原顺序，名字小写。`cookie` 来自预览页自己的 cookie jar；daemon 转发前改写哪些头见
   * spec/daemon.md § 19.5。
   */
  headers: [string, string][];
  method: string;
  /**
   * 以 `/` 开头，可带查询串；不含 scheme 和主机。
   */
  path: string;
  /**
   * `true`：这是一个 WebSocket 升级请求（`method` 为 `GET`，子协议在 `sec-websocket-protocol` 头里）。
   */
  websocket?: boolean;
}
/**
 * 隧道 `RESET` 帧的载荷（msgpack）：终止这个 stream 的两个方向。
 */
export interface PreviewReset {
  code: PreviewResetCode;
  message?: string | null;
}
/**
 * 隧道 `RESPONSE` 帧的载荷（msgpack）。WebSocket 流里 `status = 101` 表示升级成功，其他状态码表示被拒，
 * 后面跟普通响应体。
 */
export interface PreviewResponseHead {
  encoding?: PreviewBodyEncoding & string;
  headers: [string, string][];
  status: number;
}
/**
 * 一个项目 = workspace 根目录下的一个一级子文件夹，或一个关联的本机目录（`kind = linked`）。
 * 元数据全部推导；只有「关联了哪些目录」存在 daemon 的库里。
 */
export interface Project {
  active_sessions: number;
  git?: GitMeta | null;
  /**
   * git 元数据还在算（daemon 冷缓存时前台等待超时）：这时 `git` 为空，**不代表不是 git 项目**。
   * 算完之后 daemon 发 `workspace.changed { updated }`，客户端重新 `workspace.list`。
   */
  git_pending?: boolean;
  /**
   * 缺省 `folder`。
   */
  kind?: ProjectKind & string;
  /**
   * 关联的目录已经不在了（被移走、删除，或外接盘没挂上）。只有 `linked` 会是 true；
   * 这时 `git` 为空、`modified_at_ms` 为 0，不能在里面新建会话，客户端应提供「解除关联」。
   */
  missing?: boolean;
  modified_at_ms: number;
  /**
   * 项目名。根目录下的项目和关联的项目共用一个命名空间，客户端的所有操作都用它。
   */
  name: string;
  /**
   * 绝对路径，仅展示用。
   */
  path: string;
}
export interface GitMeta {
  branch?: string | null;
  dirty: boolean;
  remote?: string | null;
  /**
   * 能给这个项目的会话开 worktree：项目目录就是仓库顶层（不是仓库里的子目录），并且至少有一个提交
   * （ADR-34）。为 false 时客户端不提供「新 worktree」。
   */
  worktree_ready?: boolean;
}
/**
 * `gh pr view` 的摘要。
 */
export interface PullRequestInfo {
  /**
   * PR 的目标分支。
   */
  base_branch: string;
  checks_failed: number;
  /**
   * CI 检查：通过 / 失败 / 进行中的数量。
   */
  checks_passed: number;
  checks_pending: number;
  number: number;
  state: PullRequestState;
  title: string;
  url: string;
}
export interface RpcError {
  code: number;
  data?: RpcErrorData | null;
  message: string;
}
export interface RpcErrorData {
  code: ErrorCode;
  /**
   * `invalid_settings` 时的字段名等补充信息。
   */
  field?: string | null;
  reason?: string | null;
  retry_after_ms?: number | null;
}
export interface RpcRequest {
  id: RpcId;
  jsonrpc: string;
  method: string;
  params?: unknown;
}
export interface RpcNotification {
  jsonrpc: string;
  method: string;
  params?: unknown;
}
export interface RpcResponse {
  error?: RpcError | null;
  id: RpcId;
  jsonrpc: string;
  result?: unknown;
}
/**
 * `session.archive`。没有 worktree 的会话只看 `session_id`。
 */
export interface SessionArchiveParams {
  session_id: string;
  worktree?: WorktreeDisposal & string;
}
/**
 * `session.changed` 通知：列表级变化，发给所有在线客户端。
 */
export interface SessionChanged {
  archived?: boolean;
  /**
   * 同 [`SessionSummary::background_tasks`]。
   */
  background_tasks?: number;
  /**
   * 会话被删除。
   */
  deleted?: boolean;
  last_seq: number;
  pending_permissions: number;
  session_id: string;
  status: SessionStatus;
  title: string;
  updated_at_ms: number;
  /**
   * worktree 的当前状态（会话有 worktree 时）。
   */
  worktree?: SessionWorktree | null;
}
/**
 * 会话所在的 worktree。
 */
export interface SessionWorktree {
  /**
   * base 的展示名：分支 / 远端跟踪分支 / 标签写法是短名字（`main`、`origin/main`、`v1.0`），
   * 其他写法（`HEAD`、`HEAD~1`…）是解析出的提交的前 12 位。daemon 内部按规范化的引用名或完整 SHA 计算。
   */
  base: string;
  /**
   * 创建时 base 解析到的提交。
   */
  base_sha: string;
  /**
   * 如 `anycon/fix-login`。
   */
  branch: string;
  /**
   * `WorktreeInfo.id`。
   */
  id: string;
  /**
   * 相对项目目录的路径，如 `.anycon/worktrees/fix-login`。
   */
  path: string;
  state: WorktreeState;
}
export interface SessionCreateParams {
  agent: string;
  /**
   * 第一条消息的附件（`blob.put_end` 的结果）。agent 的 `capabilities.attachments` 不含其 mime → `unsupported`。
   */
  attachments?: AttachmentRef[];
  /**
   * 第一条消息的客户端 id（幂等），有 `prompt` 或 `attachments` 时必填。
   */
  message_id?: string | null;
  /**
   * 空 = scratch。
   */
  project?: string | null;
  /**
   * 有则创建后立即发送第一条消息（`prompt` 为空但带了 `attachments` 时也发，内容为空串）。
   */
  prompt?: string | null;
  settings?: SessionSettings;
  /**
   * 在哪里工作；缺省 = `local`（ADR-34）。
   */
  workspace?: SessionWorkspace | null;
}
/**
 * `session.delete`。
 */
export interface SessionDeleteParams {
  /**
   * 同时删掉 worktree 的分支（`git branch -D`），分支上没合并的提交会丢。只有 worktree 也被删掉时才生效。
   */
  delete_branch?: boolean;
  session_id: string;
  worktree?: WorktreeDisposal & string;
}
/**
 * `session.diff`：只读。不带 `path` 时只给文件列表和行数；带 `path` 时只给那一个文件，附 unified diff。
 */
export interface SessionDiffParams {
  against?: DiffAgainst & string;
  /**
   * 仓库内的相对路径（`DiffFile.path`）。
   */
  path?: string | null;
  session_id: string;
}
export interface SessionDiffResult {
  files: DiffFile[];
  /**
   * 比较的起点提交（merge-base 或 HEAD）。
   */
  from_sha: string;
  /**
   * 改动的文件超过 1000 个时只给前 1000 个。
   */
  truncated?: boolean;
}
export interface SessionEvent {
  event: Event;
  /**
   * 会话内单调递增，持久化，用于增量拉取。
   */
  seq: number;
  session_id: string;
  ts_ms: number;
  turn_id?: number | null;
}
/**
 * 会话工作目录的 git 状态。worktree 会话相对 base；在项目目录里工作的会话没有 base 相关字段。
 */
export interface SessionGitStatus {
  /**
   * 本分支有、base 没有的提交数。
   */
  ahead?: number | null;
  /**
   * worktree 会话的 base（同 `SessionWorktree.base`）。
   */
  base?: string | null;
  /**
   * base 现在解析到的提交；base 已经不存在时退回创建时的提交。
   */
  base_sha?: string | null;
  /**
   * base 有、本分支还没有的提交数。
   */
  behind?: number | null;
  /**
   * 当前分支；detached 时为空。
   */
  branch?: string | null;
  /**
   * 有冲突、还没解决的文件数。
   */
  conflicts: number;
  head_sha: string;
  /**
   * 正在合并（`MERGE_HEAD` 存在）。
   */
  merging?: boolean;
  pr?: PullRequestInfo | null;
  /**
   * 要了 PR 但拿不到时的原因：`gh_not_installed` / `gh_not_authenticated` / `no_remote` / `no_pr` / `error`。
   */
  pr_error?: string | null;
  /**
   * 未提交的文件数（已暂存 + 未暂存 + 未跟踪）。
   */
  uncommitted: number;
  /**
   * 有上游时，本地还没推送的提交数。
   */
  unpushed?: number | null;
  /**
   * 上游分支（`origin/anycon/fix-login`）；没推送过为空。
   */
  upstream?: string | null;
}
/**
 * `session.git_status`。
 */
export interface SessionGitStatusParams {
  /**
   * 同时用 `gh` 查这条分支的 PR（要联网，约 1–3 秒）。没装 / 没登录 `gh`、没有远端时 `pr` 为空、`pr_error` 说明原因。
   */
  include_pr?: boolean;
  session_id: string;
}
export interface SessionHistoryParams {
  /**
   * 返回 seq > after_seq 的事件。
   */
  after_seq?: number;
  /**
   * 为 true 时返回 seq < before_seq 的最近 limit 条（向上翻页）。
   */
  before_seq?: number | null;
  /**
   * 默认 200，上限 1000。
   */
  limit?: number | null;
  session_id: string;
}
export interface SessionHistoryResult {
  events: SessionEvent[];
  has_more: boolean;
}
export interface SessionIdParams {
  session_id: string;
}
export interface SessionListParams {
  include_archived?: boolean;
  limit?: number | null;
  project?: string | null;
}
export interface SessionListResult {
  sessions: SessionSummary[];
}
export interface SessionSummary {
  agent: string;
  /**
   * agent 自己的会话 id（Claude Code session_id 等），诊断用。
   */
  agent_session_id?: string | null;
  archived?: boolean;
  /**
   * agent 进程里还在跑的后台任务数（Claude Code 的后台 subagent、后台命令）。主 turn 结束后它们可能还在干活，
   * 期间 daemon 不按 `idle_timeout` 回收进程（docs/spec/daemon.md § 7.3）。只反映 daemon 内存里的当前状态，不进事件流。
   */
  background_tasks?: number;
  created_at_ms: number;
  id: string;
  /**
   * 最后一个事件的 seq；客户端据此决定要不要拉增量。
   */
  last_seq: number;
  pending_permissions: number;
  /**
   * 最后一条消息摘要（列表展示）。
   */
  preview?: string | null;
  /**
   * 空 = scratch 会话。
   */
  project?: string | null;
  /**
   * 排队位置（status = queued 时）。
   */
  queue_position?: number | null;
  settings: SessionSettings;
  status: SessionStatus;
  title: string;
  updated_at_ms: number;
  usage?: UsageTotals;
  /**
   * 会话在 anycon 建的 worktree 里工作时给出（ADR-34）；在项目目录本身工作时为空。
   */
  worktree?: SessionWorktree | null;
}
export interface UsageTotals {
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd?: number | null;
  input_tokens: number;
  output_tokens: number;
  turns: number;
}
export interface SessionPromoteParams {
  /**
   * 新项目名。
   */
  name: string;
  session_id: string;
}
export interface SessionRenameParams {
  session_id: string;
  title: string;
}
export interface SessionSearchParams {
  limit?: number | null;
  project?: string | null;
  query: string;
}
export interface SessionSearchResult {
  hits: SearchHit[];
}
export interface SearchHit {
  seq: number;
  session_id: string;
  /**
   * 带 `<mark>` 的片段。
   */
  snippet: string;
  title: string;
  ts_ms: number;
}
export interface SessionSendParams {
  /**
   * `blob.put_end` 的结果，每条至多 20 个。agent 的 `capabilities.attachments` 不含其 mime → `unsupported`。
   */
  attachments?: AttachmentRef[];
  /**
   * 带了附件时可以为空串。
   */
  content: string;
  /**
   * 客户端生成（uuid），重发不重复。
   */
  message_id: string;
  session_id: string;
}
export interface SessionSendResult {
  /**
   * 幂等命中：这条 message_id 之前已收到。
   */
  duplicate?: boolean;
  queued: boolean;
  /**
   * 立即开始的 turn id；进入队列（turn 进行中且 agent 不支持插话）时为空。
   */
  turn_id?: number | null;
}
export interface SessionSetSettingsParams {
  /**
   * 要清除（恢复为未设置）的字段，在 `settings` 合并之后生效；两边都出现时以 `clear` 为准。
   */
  clear?: ClearableSetting[];
  session_id: string;
  /**
   * 增量：只带要改的字段；缺省 / `null` 表示保持（`options` 里值为 `null` 表示删键）。
   */
  settings: SessionSettings;
}
export interface SessionSetSettingsResult {
  /**
   * 实际生效的完整设置（thinking 可能已按新模型回落）。
   */
  settings: SessionSettings;
}
/**
 * `session.sync_base`：把 base 的最新提交合并进 worktree 的分支（base 是远端跟踪分支时先 fetch）。
 */
export interface SessionSyncBaseParams {
  /**
   * 放弃一次有冲突的合并（`git merge --abort`）。
   */
  abort?: boolean;
  session_id: string;
}
export interface SessionSyncBaseResult {
  /**
   * 合进来的 base 提交数。
   */
  commits: number;
  /**
   * 冲突的文件（最多 200 个）。
   */
  conflicts?: string[];
  outcome: SyncOutcome;
}
export interface TrashEntry {
  bytes: number;
  /**
   * `.trash/` 下的目录名，如 `my-app-20261011T120000Z`。
   */
  id: string;
  original_name: string;
  trashed_at_ms: number;
}
/**
 * `workspace.branches`：给新建会话的「基于哪个分支」下拉框列分支。
 */
export interface WorkspaceBranchesParams {
  project: string;
}
export interface WorkspaceBranchesResult {
  /**
   * 项目目录当前检出的分支；HEAD 是 detached 时为空。
   */
  current?: string | null;
  /**
   * 新会话默认的 base：当前分支有上游（`origin/main`）时是上游，否则是当前分支；detached 时是 HEAD 的提交。
   */
  default_base: string;
  /**
   * 本地分支，按最近提交时间倒序，最多 200 个。不含 anycon 自己建的 worktree 分支（`<branch_prefix>/…`）。
   */
  local: string[];
  /**
   * 远端跟踪分支（`origin/main`），同样排序和上限，不含 `* /HEAD`。
   */
  remote: string[];
  /**
   * 任何一组被截断时为 true。
   */
  truncated?: boolean;
}
/**
 * `workspace.browse`：给「关联已有目录」的选择器列本机目录。只列目录，只在 `workspace.link_roots` 之内。
 */
export interface WorkspaceBrowseParams {
  /**
   * 要列的目录（绝对路径或 `~/…`）。缺省 = `roots` 的第一项。
   */
  path?: string | null;
  /**
   * 也列出以 `.` 开头的目录。默认 false。
   */
  show_hidden?: boolean;
}
export interface WorkspaceBrowseResult {
  /**
   * `path` 本身不能关联、又不是因为已经是项目时，给出原因（规则同 `BrowseEntry.blocked`）。
   */
  blocked?: LinkBlock | null;
  /**
   * 子目录，按名字排序。跟随符号链接；解析后落在 `roots` 之外的不列。
   */
  entries: BrowseEntry[];
  /**
   * `path` 本身能不能直接关联（规则同 `BrowseEntry.linkable`）。
   */
  linkable: boolean;
  /**
   * 上一级的绝对路径；`path` 已经是某个 root 时为空（不能再往上）。
   */
  parent?: string | null;
  /**
   * canonicalize 之后的绝对路径。
   */
  path: string;
  /**
   * `path` 本身已经是项目时给出项目名。
   */
  project?: string | null;
  /**
   * 可浏览、可关联的范围：`workspace.link_roots` 展开并 canonicalize 后的绝对路径。
   */
  roots: string[];
  /**
   * 子目录超过 1000 个时只给前 1000 个。
   */
  truncated?: boolean;
}
/**
 * `workspace.changed` 通知。
 */
export interface WorkspaceChanged {
  added?: string[];
  removed?: string[];
  renamed?: RenamedProject[];
  /**
   * 这些项目推导出来的元数据（`git`）有了或变了——比如 daemon 重启后冷缓存里第一次算完（ADR-34）。
   * 客户端重新 `workspace.list`。
   */
  updated?: string[];
}
export interface RenamedProject {
  from: string;
  to: string;
}
export interface WorkspaceCloneParams {
  name?: string | null;
  /**
   * 只允许 `https://`、`ssh://`、`git@host:path` 三种形式。
   */
  url: string;
}
export interface WorkspaceCreateParams {
  git_init?: boolean;
  name: string;
}
/**
 * `workspace.link`：把本机一个已有目录关联为项目。
 */
export interface WorkspaceLinkParams {
  /**
   * 项目名；缺省取目录名。规则同 `workspace.create`，不能和已有项目（根目录下的或关联的）重名。
   */
  name?: string | null;
  /**
   * 本机绝对路径；`~` / `~/…` 按 daemon 用户的 HOME 展开。必须是已存在的目录，
   * canonicalize 之后要在 `workspace.link_roots` 之内。
   */
  path: string;
}
export interface WorkspaceListResult {
  projects: Project[];
  root: string;
}
export interface WorkspaceNameParams {
  name: string;
}
/**
 * `workspace.remove_worktree`：删掉一个已保留或没有会话的 worktree。会话还没归档时返回 `busy`。
 */
export interface WorkspaceRemoveWorktreeParams {
  /**
   * 同时删掉分支（`git branch -D`）。分支上有没合并的提交也会丢。
   */
  delete_branch?: boolean;
  /**
   * 有未提交的改动也删（丢弃这些改动）。为 false 而有改动时返回 `invalid`，`data.reason = "uncommitted_changes"`。
   */
  force?: boolean;
  id: string;
}
export interface WorkspaceRenameParams {
  from: string;
  to: string;
}
export interface WorkspaceTrashIdParams {
  id: string;
}
export interface WorkspaceTrashListResult {
  entries: TrashEntry[];
}
/**
 * `workspace.worktrees`。
 */
export interface WorkspaceWorktreesParams {
  /**
   * 只列这个项目的；缺省 = 全部项目。
   */
  project?: string | null;
}
export interface WorkspaceWorktreesResult {
  worktrees: WorktreeInfo[];
}
/**
 * 一个由 anycon 建的 worktree。
 */
export interface WorktreeInfo {
  /**
   * 分支上 base 之后自己的提交数。
   */
  ahead: number;
  /**
   * base 的展示名，规则同 `SessionWorktree.base`（短引用名，或提交的前 12 位）。
   */
  base: string;
  branch: string;
  /**
   * 磁盘占用；统计超过时间预算时为空。
   */
  bytes?: number | null;
  created_at_ms: number;
  /**
   * daemon 生成的 id（`wt_` + 22 位 base62）。
   */
  id: string;
  /**
   * 相对项目目录的路径，如 `.anycon/worktrees/fix-login`。
   */
  path: string;
  project: string;
  /**
   * 所属会话；会话删掉之后为空（「没有会话的 worktree」）。
   */
  session_id?: string | null;
  session_title?: string | null;
  state: WorktreeState;
  /**
   * 未提交的文件数（含未跟踪的）；目录不在时为 0。
   */
  uncommitted: number;
}
