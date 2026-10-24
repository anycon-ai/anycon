/**
 * 外层信封与控制消息的 msgpack 编解码。手写，和 `crates/protocol/src/{envelope,control}.rs`
 * 的 `rmp_serde::to_vec_named` 输出字节一致：
 *
 * - 信封是字符串键的 map，键序 `v, from, to, kind, seq, body`；`body` 是 bin（`Uint8Array`），不是数组。
 * - 控制消息是内部标签 `type` 的 map：`type` 在最前，其余字段按 Rust 声明顺序；
 *   `skip_serializing_if = Option::is_none` 的字段为 null / undefined 时整键省略。
 *
 * 字节级一致由 `tests/vectors/*.hex`（`tools/gen-vectors` 用 Rust 生成）守住。
 */
import { decode, encode } from '@msgpack/msgpack'

import { ENVELOPE_VERSION } from './constants.js'
import { ProtocolError } from './errors.js'
import type { Control, EnrollFrame, Kind } from './generated.js'

export type { Control, EnrollFrame, Kind }

/** 外层信封。 */
export interface Envelope {
  /** 信封版本，见 `ENVELOPE_VERSION`。 */
  v: number
  /** 发送设备。hub 校验它等于当前连接的设备 id。 */
  from: string
  /** 目标设备。发给 hub 的 `control` 填空字符串。 */
  to: string
  kind: Kind
  /** 发送方单调递增。 */
  seq: number
  body: Uint8Array
}

/** 全部信封种类，对应 envelope.rs 的 `Kind`。 */
export const ENVELOPE_KINDS: readonly Kind[] = ['handshake', 'data', 'notify', 'control']

const KINDS: ReadonlySet<string> = new Set<string>(ENVELOPE_KINDS)

export function isKind(value: unknown): value is Kind {
  return typeof value === 'string' && KINDS.has(value)
}

/** 构造信封，`v` 默认当前版本。 */
export function makeEnvelope(
  from: string,
  to: string,
  kind: Kind,
  seq: number,
  body: Uint8Array,
): Envelope {
  return { v: ENVELOPE_VERSION, from, to, kind, seq, body }
}

export function encodeEnvelope(env: Envelope): Uint8Array {
  if (!Number.isSafeInteger(env.seq) || env.seq < 0) {
    throw new ProtocolError(`envelope seq must be a non-negative safe integer, got ${env.seq}`)
  }
  if (!Number.isInteger(env.v) || env.v < 0 || env.v > 255) {
    throw new ProtocolError(`envelope v must fit in u8, got ${env.v}`)
  }
  if (!(env.body instanceof Uint8Array)) {
    throw new ProtocolError('envelope body must be a Uint8Array')
  }
  // 键序固定，和 Rust 结构体声明顺序一致。
  return encode({
    v: env.v,
    from: env.from,
    to: env.to,
    kind: env.kind,
    seq: env.seq,
    body: env.body,
  })
}

export function decodeEnvelope(bytes: Uint8Array): Envelope {
  let raw: unknown
  try {
    raw = decode(bytes)
  } catch (cause) {
    throw new ProtocolError('envelope is not valid msgpack', { cause })
  }
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    throw new ProtocolError('envelope must be a msgpack map')
  }
  const m = raw as Record<string, unknown>
  const { v, from, to, kind, seq, body } = m
  if (typeof v !== 'number') throw new ProtocolError('envelope.v must be a number')
  if (typeof from !== 'string') throw new ProtocolError('envelope.from must be a string')
  if (typeof to !== 'string') throw new ProtocolError('envelope.to must be a string')
  if (!isKind(kind)) throw new ProtocolError(`envelope.kind is unknown: ${String(kind)}`)
  if (typeof seq !== 'number') throw new ProtocolError('envelope.seq must be a number')
  if (!(body instanceof Uint8Array)) throw new ProtocolError('envelope.body must be bin')
  // body 是输入 buffer 的视图；拷一份，避免调用方复用 WebSocket 缓冲时被改写。
  return { v, from, to, kind, seq, body: body.slice() }
}

// ---------- Control ----------

type ControlType = Control['type']

/** 每个变体的字段声明顺序（不含 `type`），对应 control.rs。 */
const CONTROL_FIELD_ORDER: Readonly<Record<ControlType, readonly string[]>> = {
  auth: ['device_token', 'protocol_version', 'client', 'preview_token'],
  auth_ok: ['device_id', 'server_time_ms', 'peers_online', 'quota'],
  auth_fail: ['reason'],
  pair_begin: ['match_id'],
  pair_registered: ['expires_at_ms'],
  pair_join: ['match_id'],
  pair_matched: ['peer', 'role'],
  pair_confirm: ['peer'],
  pair_done: ['peer', 'device_id', 'device_token'],
  pair_failed: ['reason'],
  enroll_begin: ['node'],
  enroll_started: ['enroll_id', 'node', 'expires_at_ms'],
  enroll_request: ['enroll_id', 'peer', 'expires_at_ms'],
  enroll_cancel: ['enroll_id'],
  enroll_failed: ['enroll_id', 'reason'],
  revoke: ['device'],
  peer_online: ['device'],
  peer_offline: ['device'],
  preview_peer: ['peer', 'label'],
  undeliverable: ['to', 'seq'],
  queue_overflow: ['to', 'dropped_seqs'],
  queue_drop: ['seq'],
  backpressure: ['to', 'seq'],
  dropped: ['seq', 'reason'],
  rate_limited: ['retry_after_ms', 'to'],
  quota_update: ['quota'],
  rotate_token: [],
  token_rotated: ['device_token'],
}

/** Rust 侧 `#[serde(skip_serializing_if = "Option::is_none")]` 的字段：None 时整键省略，而不是编码为 nil。 */
const CONTROL_SKIP_NONE: Readonly<Record<string, ReadonlySet<string>>> = {
  auth: new Set(['device_token', 'preview_token']),
  auth_ok: new Set(['quota']),
  pair_done: new Set(['device_id', 'device_token']),
  enroll_failed: new Set(['enroll_id']),
  rate_limited: new Set(['to']),
}

/** 全部控制消息的 `type`，对应 control.rs 的 `Control` 变体（snake_case）。 */
export const CONTROL_TYPES = Object.keys(CONTROL_FIELD_ORDER) as readonly ControlType[]

const CLIENT_INFO_ORDER = ['name', 'kind', 'platform', 'version', 'origin'] as const
const CLIENT_INFO_SKIP_NONE: ReadonlySet<string> = new Set(['origin'])
const QUOTA_ORDER = [
  'devices',
  'devices_max',
  'daemons',
  'daemons_max',
  'connections',
  'connections_max',
] as const

type Dict = Record<string, unknown>

function isDict(v: unknown): v is Dict {
  return typeof v === 'object' && v !== null && !Array.isArray(v) && !(v instanceof Uint8Array)
}

/** 按 `order` 重排键；`skipNone` 里的键为 null / undefined 时省略；未知键保持原序追加在后（前向兼容）。 */
function orderKeys(
  src: Dict,
  order: readonly string[],
  skipNone: ReadonlySet<string> | undefined,
  head?: Dict,
): Dict {
  const out: Dict = head ? { ...head } : {}
  for (const key of order) {
    const value = src[key]
    if (value === undefined) continue
    if (value === null && skipNone?.has(key)) continue
    out[key] = value
  }
  const known = new Set(order)
  for (const key of Object.keys(src)) {
    if (key === 'type' || known.has(key) || src[key] === undefined) continue
    out[key] = src[key]
  }
  return out
}

function canonicalControl(ctrl: Control): Dict {
  const order = CONTROL_FIELD_ORDER[ctrl.type]
  if (!order) throw new ProtocolError(`unknown control type: ${String(ctrl.type)}`)
  const src = { ...(ctrl as unknown as Dict) }
  if (isDict(src.client)) {
    src.client = orderKeys(src.client, CLIENT_INFO_ORDER, CLIENT_INFO_SKIP_NONE)
  }
  if (isDict(src.quota)) {
    src.quota = orderKeys(src.quota, QUOTA_ORDER, undefined)
  }
  return orderKeys(src, order, CONTROL_SKIP_NONE[ctrl.type], { type: ctrl.type })
}

export function encodeControl(ctrl: Control): Uint8Array {
  return encode(canonicalControl(ctrl), { ignoreUndefined: true })
}

/** 未知的 `type`（版本错位时 hub 可能发来新消息）。调用方应忽略这一帧，而不是断开。 */
export class UnknownControlError extends ProtocolError {
  override readonly name = 'UnknownControlError'
  constructor(readonly controlType: string) {
    super(`unknown control type: ${controlType}`)
  }
}

export function decodeControl(bytes: Uint8Array): Control {
  let raw: unknown
  try {
    raw = decode(bytes)
  } catch (cause) {
    throw new ProtocolError('control is not valid msgpack', { cause })
  }
  if (!isDict(raw) || typeof raw.type !== 'string') {
    throw new ProtocolError('control must be a map with a string `type`')
  }
  if (!Object.hasOwn(CONTROL_FIELD_ORDER, raw.type)) throw new UnknownControlError(raw.type)
  return raw as unknown as Control
}

// ---------- 账号内接入的 handshake body（ADR-36） ----------

type EnrollFrameType = EnrollFrame['type']

/** `EnrollFrame` 每个变体的字段声明顺序，对应 `crates/protocol/src/enroll.rs`。 */
const ENROLL_FRAME_FIELD_ORDER: Readonly<Record<EnrollFrameType, readonly string[]>> = {
  hello: ['v', 'method', 'approval', 'nonce', 'hint'],
  reply: ['status', 'nonce', 'reason', 'retry_after_ms', 'attempts_left', 'salt', 'cpace'],
  cpace: ['share'],
  noise: ['msg'],
}
const ENROLL_FRAME_SKIP_NONE: Readonly<Record<string, ReadonlySet<string>>> = {
  reply: new Set(['nonce', 'reason', 'retry_after_ms', 'attempts_left', 'salt', 'cpace']),
}
const ENROLL_HINT_ORDER = ['name', 'kind', 'platform'] as const

/** 接入期间 `handshake` 信封的 body：msgpack，`type` 在最前，其余字段按 Rust 声明顺序。 */
export function encodeEnrollFrame(frame: EnrollFrame): Uint8Array {
  const order = ENROLL_FRAME_FIELD_ORDER[frame.type]
  if (!order) throw new ProtocolError(`unknown enroll frame type: ${String(frame.type)}`)
  const src = { ...(frame as unknown as Dict) }
  if (isDict(src.hint)) src.hint = orderKeys(src.hint, ENROLL_HINT_ORDER, undefined)
  const out = orderKeys(src, order, ENROLL_FRAME_SKIP_NONE[frame.type], { type: frame.type })
  return encode(out, { ignoreUndefined: true })
}

/** 解 `encodeEnrollFrame` 的输出。不是 map、没有 `type` 或 `type` 未知都抛 `ProtocolError`。 */
export function decodeEnrollFrame(bytes: Uint8Array): EnrollFrame {
  let raw: unknown
  try {
    raw = decode(bytes)
  } catch (cause) {
    throw new ProtocolError('enroll frame is not valid msgpack', { cause })
  }
  if (!isDict(raw) || typeof raw.type !== 'string') {
    throw new ProtocolError('enroll frame must be a map with a string `type`')
  }
  if (!Object.hasOwn(ENROLL_FRAME_FIELD_ORDER, raw.type)) {
    throw new ProtocolError(`unknown enroll frame type: ${raw.type}`)
  }
  return raw as unknown as EnrollFrame
}

/** 构造 `kind = control` 的信封（发往 hub，`to` 为空串）。 */
export function controlEnvelope(from: string, seq: number, ctrl: Control): Envelope {
  return makeEnvelope(from, '', 'control', seq, encodeControl(ctrl))
}

/** 解出 `kind = control` 信封里的控制消息。 */
export function controlBody(env: Envelope): Control {
  if (env.kind !== 'control') throw new ProtocolError(`envelope kind is ${env.kind}, not control`)
  return decodeControl(env.body)
}

// ---------- data 信封的 body 框架（ADR-30） ----------

/**
 * `kind = data` 的 body 框架，和 `crates/protocol/src/envelope.rs` 的 `mod frame` 字节一致
 * （`tests/vectors/frames.json` 由 `tools/gen-vectors` 用 Rust 生成，TS 侧逐字节比对）。
 *
 * body 第一个字节是 `flags`：
 * - `0x00` WHOLE：后面是一整条 `Transport.encrypt` 的输出；
 * - `0x01` FRAGMENT：`[u32 be msg_id][u16 be index][u16 be count]` + 密文切片。同一 `msg_id` 的分片按
 *   `index` 顺序、不与其他消息交错地发送；接收方收齐 `count` 片后拼接再解密；
 * - `0x02` RESET：无载荷。发送方发现自己的传输层失步（解密失败 / 丢片）时发给对端，对端丢弃传输并重新握手。
 *
 * Noise 的 nonce 是顺序的，所以**任何一片丢失都意味着整条传输失步**：收到 `backpressure`（hub 指明了被拒的 seq）
 * 时发送方必须原样重发同一片（不能重新加密）；`dropped`（hub 每连接每秒至多回一条）是「发错东西」的信号
 * （`too_large` = 没分片 / 分片后仍超限），不据此逐条重发；接收方解密失败时按 RESET 处理。
 */
export const FRAME_WHOLE = 0x00
export const FRAME_FRAGMENT = 0x01
export const FRAME_RESET = 0x02
/** 每个 `data` 信封里最多放多少密文字节（留出信封头和 hub 上限的余量）。 */
export const MAX_FRAGMENT = 200 * 1024
/** 重组时单条消息密文总长上限，防内存耗尽。 */
export const MAX_MESSAGE = 16 * 1024 * 1024
/** FRAGMENT 头长度：flags(1) + msg_id(4) + index(2) + count(2)。 */
const FRAGMENT_HEADER = 1 + 4 + 2 + 2

/** 解出来的一帧。`data` 是 `body` 的视图（零拷贝）；需要长期持有时由调用方拷贝（`Reassembler` 会拷）。 */
export type DataFrame =
  | { readonly kind: 'whole'; readonly data: Uint8Array }
  | {
      readonly kind: 'fragment'
      readonly msgId: number
      readonly index: number
      readonly count: number
      readonly data: Uint8Array
    }
  | { readonly kind: 'reset' }

/** 与 Rust `FrameError` 的变体一一对应（snake_case）。 */
export type FrameErrorCode =
  | 'empty'
  | 'unknown_flags'
  | 'truncated'
  | 'out_of_order'
  | 'count_mismatch'
  | 'too_large'

export class FrameError extends ProtocolError {
  override readonly name = 'FrameError'
  constructor(
    readonly code: FrameErrorCode,
    message: string,
  ) {
    super(message)
  }
}

/** 把一条密文切成若干 body。短消息一个 `WHOLE`，长消息若干 `FRAGMENT`（每片 ≤ `MAX_FRAGMENT`）。 */
export function encodeFrames(ciphertext: Uint8Array, msgId: number): Uint8Array[] {
  if (!Number.isInteger(msgId) || msgId < 0 || msgId > 0xffff_ffff) {
    throw new ProtocolError(`frame msg_id must fit in u32, got ${msgId}`)
  }
  if (ciphertext.length <= MAX_FRAGMENT) {
    const body = new Uint8Array(1 + ciphertext.length)
    body[0] = FRAME_WHOLE
    body.set(ciphertext, 1)
    return [body]
  }
  const count = Math.ceil(ciphertext.length / MAX_FRAGMENT)
  if (count > 0xffff) throw new ProtocolError(`message needs ${count} fragments, more than u16`)
  const bodies: Uint8Array[] = []
  for (let index = 0; index < count; index++) {
    const slice = ciphertext.subarray(index * MAX_FRAGMENT, (index + 1) * MAX_FRAGMENT)
    const body = new Uint8Array(FRAGMENT_HEADER + slice.length)
    const view = new DataView(body.buffer)
    body[0] = FRAME_FRAGMENT
    view.setUint32(1, msgId, false)
    view.setUint16(5, index, false)
    view.setUint16(7, count, false)
    body.set(slice, FRAGMENT_HEADER)
    bodies.push(body)
  }
  return bodies
}

/** `RESET` 帧的 body：请求对端丢弃传输并重新握手。 */
export function resetBody(): Uint8Array {
  return Uint8Array.of(FRAME_RESET)
}

/** 解一个 `data` 信封的 body。格式不对抛 `FrameError`。 */
export function decodeFrame(body: Uint8Array): DataFrame {
  if (body.length === 0) throw new FrameError('empty', 'empty body')
  const flags = body[0]!
  switch (flags) {
    case FRAME_WHOLE:
      return { kind: 'whole', data: body.subarray(1) }
    case FRAME_RESET:
      return { kind: 'reset' }
    case FRAME_FRAGMENT: {
      if (body.length < FRAGMENT_HEADER) {
        throw new FrameError('truncated', 'truncated fragment header')
      }
      const view = new DataView(body.buffer, body.byteOffset, body.byteLength)
      return {
        kind: 'fragment',
        msgId: view.getUint32(1, false),
        index: view.getUint16(5, false),
        count: view.getUint16(7, false),
        data: body.subarray(FRAGMENT_HEADER),
      }
    }
    default:
      throw new FrameError('unknown_flags', `unknown flags ${flags}`)
  }
}

/**
 * 每个发送方（对端）一个。`push` 返回整条密文表示可以解密了，返回 `undefined` 表示还在等后续分片。
 * 行为与 Rust `Reassembler` 一致：严格按序（`index` 必须等于期望的下一片），`index < count`，同一 `msg_id`
 * 的 `count` 中途不能变；`count == 1` 的单片 FRAGMENT 也能收；一条收完后同一 `msg_id` 再来要从 0 开始。
 * `WHOLE` / `RESET` 丢弃进行中的重组；分片报错时进行中的重组也被丢弃（`count == 0` 例外：头部非法，不动状态）。
 * 报错后调用方应当按「传输失步」处理（丢传输、重新握手）。`index` / `count` 都是 u16，这里的比较不会溢出。
 */
export class Reassembler {
  #current:
    | { msgId: number; count: number; next: number; chunks: Uint8Array[]; length: number }
    | undefined

  push(frame: DataFrame): Uint8Array | undefined {
    switch (frame.kind) {
      case 'whole':
        this.#current = undefined
        return frame.data.slice()
      case 'reset':
        this.#current = undefined
        return undefined
      case 'fragment': {
        const { msgId, index, count, data } = frame
        if (count === 0) throw new FrameError('count_mismatch', 'fragment count mismatch')
        if (index >= count) {
          this.#current = undefined
          throw new FrameError(
            'out_of_order',
            `fragment index ${index} is not below its count ${count}`,
          )
        }
        let cur = this.#current
        this.#current = undefined
        if (cur && cur.msgId === msgId) {
          if (cur.count !== count) {
            throw new FrameError(
              'count_mismatch',
              `fragment count changed mid-message: ${cur.count} -> ${count}`,
            )
          }
        } else {
          cur = { msgId, count, next: 0, chunks: [], length: 0 }
        }
        if (index !== cur.next) {
          throw new FrameError(
            'out_of_order',
            `fragment out of order: expected ${cur.next}, got ${index}`,
          )
        }
        if (cur.length + data.length > MAX_MESSAGE) {
          throw new FrameError('too_large', 'message too large')
        }
        cur.chunks.push(data.slice())
        cur.length += data.length
        if (index + 1 === count) {
          const out = new Uint8Array(cur.length)
          let at = 0
          for (const chunk of cur.chunks) {
            out.set(chunk, at)
            at += chunk.length
          }
          return out
        }
        cur.next = index + 1
        this.#current = cur
        return undefined
      }
    }
  }

  /** 丢弃进行中的重组（连接断开、传输重建时调用）。 */
  clear(): void {
    this.#current = undefined
  }
}

/** 便捷：直接喂 body。 */
export function pushBody(reassembler: Reassembler, body: Uint8Array): Uint8Array | undefined {
  return reassembler.push(decodeFrame(body))
}
