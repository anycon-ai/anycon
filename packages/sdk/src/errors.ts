/**
 * SDK 错误类。协议层的 JSON-RPC 错误（`AnyconRpcError`）按稳定码映射到具体子类：
 * `agent_unavailable` → `AgentUnavailable`，`invalid_settings` → `SettingsInvalid`，
 * `node_busy` → `NodeBusy`，`protocol_mismatch` → `ProtocolMismatch`，附件的拒绝（`data.reason` 是
 * `AttachmentErrorReason` 之一）→ `AttachmentError`，其余 → `RpcError`。
 */
import {
  AnyconRpcError,
  ERROR_CODE_NUMBERS,
  type ErrorCode,
  type PairFailReason,
  type RpcErrorData,
} from '@anycon/protocol'

/** SDK 本地错误（非 RPC 答复）的基类。 */
export class AnyconError extends Error {
  override readonly name: string = 'AnyconError'
  /** 稳定的 snake_case 码，便于程序判断和 i18n。 */
  readonly code: string

  constructor(code: string, message: string, options?: { cause?: unknown }) {
    super(message, options)
    this.code = code
  }
}

/** 本地没有该 hub 的设备身份，或 hub 不认这个设备 token：需要先 `pair()`。 */
export class PairingRequired extends AnyconError {
  override readonly name: string = 'PairingRequired'
  constructor(message = 'this device is not paired with the hub yet: call pair() first') {
    super('pairing_required', message)
  }
}

/** 配对失败（短码不存在 / 过期、超时、对端断开、握手错误……）。 */
export class PairingFailed extends AnyconError {
  override readonly name: string = 'PairingFailed'
  readonly reason: PairFailReason | 'handshake' | 'protocol' | 'cancelled'
  constructor(
    reason: PairFailReason | 'handshake' | 'protocol' | 'cancelled',
    message?: string,
    options?: { cause?: unknown },
  ) {
    super(`pairing_${reason}`, message ?? `pairing failed: ${reason}`, options)
    this.reason = reason
  }
}

/**
 * 配对时节点给出的公钥与本地已记录的同 id 节点不同。SDK 不会静默覆盖已固定的公钥——这可能是中间人，
 * 也可能是节点重装。确认可信后先 `forgetNode()` 再重新配对。
 */
export class NodeKeyChanged extends AnyconError {
  override readonly name: string = 'NodeKeyChanged'
  readonly nodeId: string
  /** 本地已固定的公钥指纹。 */
  readonly storedFingerprint: string | undefined
  /** 这次配对看到的公钥指纹。 */
  readonly presentedFingerprint: string | undefined
  constructor(
    nodeId: string,
    storedFingerprint?: string,
    presentedFingerprint?: string,
    options?: { cause?: unknown },
  ) {
    super(
      'node_key_changed',
      `node ${nodeId.slice(0, 8)} is already paired with a different key ` +
        `(stored ${storedFingerprint ?? 'unknown'}, presented ${presentedFingerprint ?? 'unknown'}); ` +
        'it may be a man-in-the-middle or a reinstalled node. Nothing was saved. ' +
        'If you trust the new key, call forgetNode() for this node and pair again.',
      options,
    )
    this.nodeId = nodeId
    this.storedFingerprint = storedFingerprint
    this.presentedFingerprint = presentedFingerprint
  }
}

/** 节点不在线（hub 报 `peer_offline` / `undeliverable`，或 hub 连接断开）。 */
export class NodeOffline extends AnyconError {
  override readonly name: string = 'NodeOffline'
  readonly node: string | undefined
  constructor(node?: string, message?: string) {
    super('node_offline', message ?? `node ${node ?? ''} is offline`.replace('  ', ' '))
    this.node = node
  }
}

/** Noise 握手失败：设备被吊销，或 daemon 重装后不认识这台设备的公钥。需要重新配对。 */
export class NodeRejected extends AnyconError {
  override readonly name: string = 'NodeRejected'
  constructor(message = 'the node rejected the Noise handshake: re-pair this device') {
    super('node_rejected', message)
  }
}

/** 与 hub 的连接问题（连不上、认证被拒、被新连接顶掉……）。 */
export class ConnectionError extends AnyconError {
  override readonly name: string = 'ConnectionError'
  /** WebSocket close code（如有）。 */
  readonly closeCode: number | undefined
  constructor(message: string, options?: { closeCode?: number; cause?: unknown }) {
    super('connection_failed', message, options)
    this.closeCode = options?.closeCode
  }
}

/** 其他 JSON-RPC 错误。`code` 与 protocol.md 的稳定错误码一致。 */
export class RpcError extends AnyconRpcError {
  override readonly name: string = 'RpcError'
}

/** agent 未安装 / 未登录 / 被禁用。`reason` 是 `AgentStatus`。 */
export class AgentUnavailable extends RpcError {
  override readonly name: string = 'AgentUnavailable'
}

/** 模型 / 思考 / 模式 / 选项不被该 agent 接受。`field` 指出字段。 */
export class SettingsInvalid extends RpcError {
  override readonly name: string = 'SettingsInvalid'
}

/** 节点并发上限且未排队。 */
export class NodeBusy extends RpcError {
  override readonly name: string = 'NodeBusy'
}

/**
 * 附件（blob 通道，ADR-35）被 daemon 拒绝时的 `data.reason`（protocol.md § 5.6）：
 *
 * - `too_large`（`invalid`）：单个文件超过 `sessions.max_attachment_bytes`（默认 10 MiB），或一条消息的附件合计超过 20 MiB
 * - `quota`（`busy`）：完成但没被引用的 blob 全节点合计超过 1 GiB——先发出去或 `discardAttachment()` 一些
 * - `disk_full`（`busy`）：daemon 数据目录所在卷剩余空间不够
 * - `blob`（`not_found`）：消息引用的 blob 不存在（24 小时没被引用已被回收、被丢掉了，或文件不见了）——重新上传
 * - `blob_in_use`（`invalid`）：blob 已经绑定到**别的**会话的消息上——一个附件只能属于一个会话，重新上传
 * - `in_use`（`invalid`）：`blob.discard` 一个已被消息引用的 blob
 * - `finalizing`（`invalid`）：`put_end` 已经开始收尾之后又来了块
 * - `incomplete` / `hash_mismatch` / `mime_mismatch`（`invalid`）：`put_end` 校验失败（缺块 / SHA-256 不符 / 文件头和 mime 不符）
 */
export type AttachmentErrorReason =
  | 'too_large'
  | 'quota'
  | 'disk_full'
  | 'blob'
  | 'blob_in_use'
  | 'in_use'
  | 'finalizing'
  | 'incomplete'
  | 'hash_mismatch'
  | 'mime_mismatch'

const ATTACHMENT_REASONS: ReadonlySet<string> = new Set<AttachmentErrorReason>([
  'too_large',
  'quota',
  'disk_full',
  'blob',
  'blob_in_use',
  'in_use',
  'finalizing',
  'incomplete',
  'hash_mismatch',
  'mime_mismatch',
])
/** 附件的拒绝只会带这几个稳定码。 */
const ATTACHMENT_CODES: ReadonlySet<string> = new Set(['invalid', 'busy', 'not_found'])

/**
 * 附件被 daemon 拒绝（`blob.*`，或 `session.send` / `session.create` 核对附件时）。`code` 仍是稳定码
 * （`invalid` / `busy` / `not_found`），`reason` 是 `AttachmentErrorReason`，按它决定重传、换文件还是提示用户。
 */
export class AttachmentError extends RpcError {
  override readonly name: string = 'AttachmentError'
  override get reason(): AttachmentErrorReason {
    return super.reason as AttachmentErrorReason
  }
}

/** 协议版本不兼容。 */
export class ProtocolMismatch extends RpcError {
  override readonly name: string = 'ProtocolMismatch'
  readonly ours: number
  readonly theirs: number | undefined
  constructor(ours: number, theirs?: number, message?: string, data?: RpcErrorData | null) {
    super(
      ERROR_CODE_NUMBERS.protocol_mismatch,
      message ?? `protocol mismatch: ours=${ours} theirs=${theirs ?? 'unknown'}`,
      data ?? { code: 'protocol_mismatch' },
    )
    this.ours = ours
    this.theirs = theirs
  }
}

/** 把协议层 RPC 错误转成对应的 SDK 子类；不是 RPC 错误的原样返回。 */
export function mapRpcError(err: unknown, ours = 1): unknown {
  if (!(err instanceof AnyconRpcError) || err instanceof RpcError) return err
  const code: ErrorCode | (string & {}) = err.code
  switch (code) {
    case 'agent_unavailable':
      return new AgentUnavailable(err.rpcCode, err.message, err.data)
    case 'invalid_settings':
      return new SettingsInvalid(err.rpcCode, err.message, err.data)
    case 'node_busy':
      return new NodeBusy(err.rpcCode, err.message, err.data)
    case 'protocol_mismatch':
      return new ProtocolMismatch(ours, undefined, err.message, err.data)
    default:
      if (
        err.reason !== undefined &&
        ATTACHMENT_REASONS.has(err.reason) &&
        ATTACHMENT_CODES.has(code)
      ) {
        return new AttachmentError(err.rpcCode, err.message, err.data)
      }
      return new RpcError(err.rpcCode, err.message, err.data)
  }
}
