/**
 * 常量。全部镜像 `crates/protocol`，`constants.test.ts` 会解析 Rust 源码做漂移检查：
 * 改了 Rust 侧的方法名 / 版本号 / close code / 错误码数值，这里不同步测试就会红。
 */

/** 内层 JSON-RPC 协议版本。相邻 minor 兼容，0.x 阶段可 break。 */
export const PROTOCOL_VERSION = 1
/** 外层信封版本。极少改动。 */
export const ENVELOPE_VERSION = 1

/** 短码字母表：大写字母数字去掉 0 O 1 I L。 */
export const PAIR_CODE_ALPHABET = '23456789ABCDEFGHJKMNPQRSTUVWXYZ'
/** 短码长度：8 位（约 40 bit），`anycon pair --short` 生成的；只适合自建 hub，经不信任的 hub 配对不要用。 */
export const PAIR_CODE_LEN = 8
/** 长短码长度：16 位（约 79 bit），`anycon pair` 的**默认**长度，链接 / 二维码 / `--json` 都是它。与 `crates/crypto` 的 `PAIR_CODE_LEN_LONG` 一致。 */
export const PAIR_CODE_LEN_LONG = 16

/** WebSocket close code（hub → 设备）。 */
export const CLOSE_CODE = {
  /** 同一设备的新连接顶掉了这条。不要自动重连，否则两条连接会互相踢。 */
  SUPERSEDED: 4001,
  /** hub 优雅关停。1–3 秒抖动后重连，而不是指数退避。 */
  RESTARTING: 4002,
  /** 接收太慢，发送缓冲持续满。 */
  SLOW_CONSUMER: 4003,
  /** 设备已被吊销。 */
  REVOKED: 4004,
  /** 第一帧 auth 超时。 */
  AUTH_TIMEOUT: 4005,
} as const

/** 内层方法名。`域.动作`。键名与 `crates/protocol/src/methods.rs` 的常量名一致。 */
export const METHODS = {
  // pairing（握手刚完成、PairDone 之后，daemon → joiner 的第一条加密通知；joiner 收到并校验后才落盘）
  /** D→C notification，params = PairAck */
  PAIR_ACK: 'pair.ack',

  // node
  NODE_INFO: 'node.info',
  NODE_DEVICES: 'node.devices',
  NODE_REVOKE_DEVICE: 'node.revoke_device',
  NODE_PAIR_BEGIN: 'node.pair_begin',
  NODE_STATS: 'node.stats',
  NODE_DOCTOR: 'node.doctor',
  NODE_AUDIT: 'node.audit',
  NODE_SET_AGENT_DEFAULTS: 'node.set_agent_defaults',

  // 账号内接入（ADR-36）
  /** 进行中的接入会话。 */
  NODE_ENROLL_LIST: 'node.enroll_list',
  /** 批准一个接入请求：`code` 返回 6 位码，`scan` 带二维码里的密钥。 */
  NODE_ENROLL_APPROVE: 'node.enroll_approve',
  NODE_ENROLL_DENY: 'node.enroll_deny',
  /** 主动生成 6 位接入码（「添加设备」、`anycon pair --enroll`）。 */
  NODE_ENROLL_CODE: 'node.enroll_code',
  NODE_ENROLL_STATUS: 'node.enroll_status',
  NODE_SET_ENROLL_PASSWORD: 'node.set_enroll_password',
  NODE_SET_ENROLL_ENABLED: 'node.set_enroll_enabled',
  /** D→C notification，params = EnrollInfo */
  NODE_ENROLL_REQUESTED: 'node.enroll_requested',
  /** D→C notification，params = NodeEnrollResolved */
  NODE_ENROLL_RESOLVED: 'node.enroll_resolved',
  /** D→C notification，params = NodeSecurityEvent */
  NODE_SECURITY_EVENT: 'node.security_event',

  // agent
  AGENT_LIST: 'agent.list',
  AGENT_DIAGNOSE: 'agent.diagnose',
  /** D→C notification，params = AgentListResult */
  AGENT_CHANGED: 'agent.changed',

  // workspace
  WORKSPACE_LIST: 'workspace.list',
  WORKSPACE_CREATE: 'workspace.create',
  WORKSPACE_CLONE: 'workspace.clone',
  WORKSPACE_RENAME: 'workspace.rename',
  WORKSPACE_TRASH: 'workspace.trash',
  WORKSPACE_TRASH_LIST: 'workspace.trash_list',
  WORKSPACE_RESTORE: 'workspace.restore',
  WORKSPACE_PURGE: 'workspace.purge',
  /** 把本机已有目录关联为项目（params = WorkspaceLinkParams → Project）。 */
  WORKSPACE_LINK: 'workspace.link',
  /** 解除关联（不删除目录；params = WorkspaceNameParams）。 */
  WORKSPACE_UNLINK: 'workspace.unlink',
  /** 列本机目录供选择（params = WorkspaceBrowseParams → WorkspaceBrowseResult）。 */
  WORKSPACE_BROWSE: 'workspace.browse',
  /** 列项目的分支（params = WorkspaceBranchesParams → WorkspaceBranchesResult，ADR-34）。 */
  WORKSPACE_BRANCHES: 'workspace.branches',
  /** 列 anycon 建的 worktree（params = WorkspaceWorktreesParams → WorkspaceWorktreesResult）。 */
  WORKSPACE_WORKTREES: 'workspace.worktrees',
  /** 删掉一个已保留 / 没有会话的 worktree（params = WorkspaceRemoveWorktreeParams）。 */
  WORKSPACE_REMOVE_WORKTREE: 'workspace.remove_worktree',
  /** D→C notification */
  WORKSPACE_CHANGED: 'workspace.changed',
  /** D→C notification */
  JOB_PROGRESS: 'job.progress',
  /** D→C notification */
  JOB_DONE: 'job.done',
  JOB_CANCEL: 'job.cancel',

  // fs
  FS_LIST: 'fs.list',
  FS_READ: 'fs.read',

  // session
  SESSION_LIST: 'session.list',
  SESSION_GET: 'session.get',
  SESSION_CREATE: 'session.create',
  SESSION_SEND: 'session.send',
  SESSION_CANCEL: 'session.cancel',
  SESSION_SET_SETTINGS: 'session.set_settings',
  SESSION_WATCH: 'session.watch',
  SESSION_UNWATCH: 'session.unwatch',
  SESSION_HISTORY: 'session.history',
  /** D→C notification，params = SessionEvent */
  SESSION_EVENT: 'session.event',
  /** D→C notification，params = SessionChanged */
  SESSION_CHANGED: 'session.changed',
  SESSION_RENAME: 'session.rename',
  SESSION_ARCHIVE: 'session.archive',
  SESSION_UNARCHIVE: 'session.unarchive',
  SESSION_DELETE: 'session.delete',
  SESSION_RESTART: 'session.restart',
  SESSION_SEARCH: 'session.search',
  SESSION_PROMOTE: 'session.promote',
  /** params = SessionGitStatusParams → SessionGitStatus（ADR-34） */
  SESSION_GIT_STATUS: 'session.git_status',
  /** params = SessionDiffParams → SessionDiffResult */
  SESSION_DIFF: 'session.diff',
  /** params = SessionSyncBaseParams → SessionSyncBaseResult */
  SESSION_SYNC_BASE: 'session.sync_base',

  // blob（附件，ADR-35）
  /** params = BlobPutBeginParams → BlobPutBeginResult */
  BLOB_PUT_BEGIN: 'blob.put_begin',
  /** params = BlobPutChunkParams */
  BLOB_PUT_CHUNK: 'blob.put_chunk',
  /** params = BlobIdParams → AttachmentRef */
  BLOB_PUT_END: 'blob.put_end',
  /** params = BlobGetParams → BlobGetResult */
  BLOB_GET: 'blob.get',
  /** params = BlobIdParams */
  BLOB_DISCARD: 'blob.discard',

  // permission
  /** D→C notification，params = PermissionRequest */
  PERMISSION_REQUEST: 'permission.request',
  PERMISSION_RESPOND: 'permission.respond',
  /** D→C notification，params = PermissionResolved */
  PERMISSION_RESOLVED: 'permission.resolved',
  PERMISSION_LIST: 'permission.list',

  // dev server 预览（ADR-39）。隧道本身不走 JSON-RPC，见 `preview.ts`。
  /** 为本机端口签发预览票据（params = PreviewOpenParams → PreviewOpenResult）。 */
  PREVIEW_OPEN: 'preview.open',
  /** 作废一个 label 的全部票据并断开隧道（params = PreviewCloseParams）。 */
  PREVIEW_CLOSE: 'preview.close',
  /** 未过期的票据（→ PreviewListResult）。 */
  PREVIEW_LIST: 'preview.list',
  /** 可以预览的本机端口（params = PreviewPortsParams → PreviewPortsResult）。 */
  PREVIEW_PORTS: 'preview.ports',
} as const

export type MethodName = (typeof METHODS)[keyof typeof METHODS]

/**
 * `ErrorCode` → JSON-RPC 数字 code（`crates/protocol/src/error.rs` 的 `rpc_code`）。
 * 客户端只应依赖 `error.data.code`；数字 code 仅在对端没带 `data` 时做兜底。
 */
export const ERROR_CODE_NUMBERS = {
  invalid: -32602,
  invalid_settings: -32001,
  agent_unavailable: -32002,
  node_busy: -32003,
  busy: -32004,
  already_resolved: -32005,
  exists: -32006,
  not_found: -32007,
  forbidden: -32008,
  unsupported: -32009,
  protocol_mismatch: -32010,
  internal: -32603,
} as const

/** `AgentCapabilities.attachments` 里的通配项：agent 收任意类型的文件（ADR-35「任意文件」）。与 `crates/protocol` 的 `ATTACHMENT_ANY_FILE` 一致。 */
export const ATTACHMENT_ANY_FILE = '*/*'
