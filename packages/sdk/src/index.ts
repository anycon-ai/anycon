/**
 * `@anycon/sdk`：anycon 的 TypeScript SDK（客户端方向）。Node ≥ 20 与浏览器同构。
 *
 * ```ts
 * import { pair, connect } from '@anycon/sdk'
 * await pair('https://hub.example.com', { code: '7K3P-Q2MX-W4NH-8CRD', name: 'ci-runner' })   // 一次性（默认 16 位短码，`--short` 是 8 位，或 #code= 链接）
 * const node = await connect('https://hub.example.com', { node: 'mac-mini' })
 * const session = await node.sessions.create({ agent: 'claude-code', project: 'app' })
 * for await (const ev of session.send('跑一下测试')) {
 *   if (ev.type === 'text_delta') process.stdout.write(ev.text)
 *   if (ev.type === 'permission') await ev.allow()
 * }
 * await node.close()
 * ```
 */
export { connect, type ConnectOptions } from './connect.js'
export { Client, selectNode, type ClientEvents, type ClientOptions } from './client.js'
export {
  PAIR_ACK_TIMEOUT_MS,
  forgetNode,
  pair,
  type ForgetNodeOptions,
  type PairOptions,
  type PairResult,
} from './pair.js'
export {
  Node,
  type CreateSessionParams,
  type Job,
  type NodeEvents,
  type NodeOptions,
} from './node.js'
export {
  Session,
  type ArchiveOptions,
  type DeleteOptions,
  type DiffOptions,
  type GitStatusOptions,
  type HistoryOptions,
  type PermissionActions,
  type PermissionEventData,
  type PermissionStreamEvent,
  type RunResult,
  type SendOptions,
  type StreamEvent,
  type StreamMeta,
  type SyncBaseOptions,
  type TurnStream,
} from './session.js'
export {
  DEFAULT_FILE_MIME,
  IMAGE_MIME_TYPES,
  MAX_CHUNKS_IN_FLIGHT,
  acceptsAttachment,
  detectImageMime,
  guessMime,
  normalizeMime,
  type DownloadOptions,
  type DownloadedAttachment,
  type ImageMime,
  type UploadFileOptions,
  type UploadImageOptions,
} from './attachments.js'
export {
  policies,
  type AllowAllOptions,
  type PermissionContext,
  type PermissionDecision,
  type PermissionPolicy,
} from './permissions.js'
export type { LinkState } from './link.js'
export type { BackoffOptions } from './hub.js'

export {
  AgentUnavailable,
  AnyconError,
  AttachmentError,
  type AttachmentErrorReason,
  ConnectionError,
  NodeBusy,
  NodeKeyChanged,
  NodeOffline,
  NodeRejected,
  PairingFailed,
  PairingRequired,
  ProtocolMismatch,
  RpcError,
  SettingsInvalid,
} from './errors.js'

export {
  MemoryStorage,
  assertSafeProfile,
  hubHostKey,
  resolveProfile,
  type SdkStorage,
  type StoredIdentity,
  type StoredNode,
} from './storage.js'
export { FileStorage, defaultSdkDir } from './storage-file.js'
export { IndexedDBStorage } from './storage-idb.js'
export { defaultStorage } from './default-storage.js'
export { toWsUrl, type WebSocketFactory, type WebSocketInit, type WebSocketLike } from './ws.js'
export { SDK_VERSION } from './version.js'

// 协议层类型和常用值，省得使用方再装一个包。
export { PROTOCOL_VERSION, METHODS, CLOSE_CODE, ATTACHMENT_ANY_FILE } from '@anycon/protocol'
export type {
  AgentCapabilities,
  AgentInfo,
  AttachmentRef,
  BrowseEntry,
  ClearableSetting,
  DiffAgainst,
  DiffFile,
  DiffFileStatus,
  Event,
  GitAction,
  LinkBlock,
  ModelInfo,
  ModeInfo,
  NodeInfo,
  PairFailReason,
  PermissionOption,
  PermissionRequest,
  PermissionResolved,
  Project,
  ProjectKind,
  PullRequestInfo,
  PullRequestState,
  SessionChanged,
  SessionDiffResult,
  SessionEvent,
  SessionGitStatus,
  SessionSettings,
  SessionSummary,
  SessionSyncBaseResult,
  SessionWorkspace,
  SessionWorktree,
  SyncOutcome,
  TurnDoneReason,
  Usage,
  WorkspaceBranchesResult,
  WorkspaceBrowseResult,
  WorktreeDisposal,
  WorktreeInfo,
  WorktreeState,
} from '@anycon/protocol'
