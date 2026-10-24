/**
 * dev server 预览（ADR-39，docs/protocol.md § 11）的 TS 侧：启动参数的编解码、label 校验、预览 origin。
 * 隧道消息 / 帧的编解码与 `crates/protocol/src/preview.rs` 的 `tunnel` 模块逐字节一致
 * （`tests/vectors/preview.json`）。
 */

import { decode, encode } from '@msgpack/msgpack'

import { ProtocolError } from './errors.js'
import type {
  PreviewGoaway,
  PreviewGoawayCode,
  PreviewLaunch,
  PreviewRejectReason,
  PreviewRequestHead,
  PreviewReset,
  PreviewResetCode,
  PreviewResponseHead,
} from './generated.js'

/** 预览 label：26 个字符的小写 base32（RFC 4648 字母表，无填充）。 */
export const PREVIEW_LABEL_LEN = 26
export const PREVIEW_LABEL_ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567'
/** `PreviewLaunch.v` 的当前版本。 */
export const PREVIEW_LAUNCH_VERSION = 1
/** 预览页的入口路径：hub 在这里回引导页，启动参数放在 fragment 里。 */
export const PREVIEW_ENTER_PATH = '/__anycon/enter'

const LABEL_RE = /^[a-z2-7]{26}$/

export function isValidPreviewLabel(label: string): boolean {
  return LABEL_RE.test(label)
}

/**
 * 按 hub 的模板（`/api/config.preview_origin_template`，如 `https://{label}.aiplace.app`）算出预览 origin。
 * label 不合法时抛错：它会成为主机名的一段。
 */
export function previewOrigin(template: string, label: string): string {
  if (!isValidPreviewLabel(label)) throw new ProtocolError(`invalid preview label: ${label}`)
  if (!template.includes('{label}'))
    throw new ProtocolError('preview origin template has no {label}')
  return template.replace('{label}', label)
}

function base64urlEncode(bytes: Uint8Array): string {
  let bin = ''
  for (const b of bytes) bin += String.fromCharCode(b)
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

function base64urlDecode(text: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]*$/.test(text)) throw new ProtocolError('invalid base64url')
  const std = text.replace(/-/g, '+').replace(/_/g, '/')
  const padded = std + '='.repeat((4 - (std.length % 4)) % 4)
  const bin = atob(padded)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** `PreviewLaunch` → URL fragment（不含 `#`）：JSON 的 UTF-8 字节做 base64url，无填充。 */
export function encodePreviewLaunch(launch: PreviewLaunch): string {
  return base64urlEncode(new TextEncoder().encode(JSON.stringify(launch)))
}

/** `path` 里不许出现的字符：反斜杠和 ASCII 控制字符（U+0000–U+001F、U+007F）。 */
function hasForbiddenPathChar(path: string): boolean {
  for (let i = 0; i < path.length; i++) {
    const c = path.charCodeAt(i)
    if (c === 0x5c || c <= 0x1f || c === 0x7f) return true
  }
  return false
}

const HEX32 = /^[0-9a-f]{32}$/
const HEX64 = /^[0-9a-f]{64}$/

/**
 * URL fragment（可以带或不带开头的 `#`）→ `PreviewLaunch`。格式不对一律抛 `ProtocolError`：
 * 版本、label、id 和密钥的格式、端口范围、`path` 以 `/` 开头且不是 `//`（协议相对 URL）、不含 `\` 和控制字符。
 */
export function decodePreviewLaunch(fragment: string): PreviewLaunch {
  const text = fragment.startsWith('#') ? fragment.slice(1) : fragment
  let value: unknown
  try {
    value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(base64urlDecode(text)))
  } catch (err) {
    if (err instanceof ProtocolError) throw err
    throw new ProtocolError('invalid preview launch')
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new ProtocolError('invalid preview launch')
  }
  const v = value as Record<string, unknown>
  const str = (k: string): string => {
    const x = v[k]
    if (typeof x !== 'string') throw new ProtocolError(`preview launch: ${k} must be a string`)
    return x
  }
  const int = (k: string, min: number, max: number): number => {
    const x = v[k]
    if (typeof x !== 'number' || !Number.isInteger(x) || x < min || x > max) {
      throw new ProtocolError(`preview launch: ${k} out of range`)
    }
    return x
  }
  const launch: PreviewLaunch = {
    v: int('v', 0, Number.MAX_SAFE_INTEGER),
    node: str('node'),
    node_public: str('node_public'),
    label: str('label'),
    port: int('port', 1, 65535),
    ticket_id: str('ticket_id'),
    psk: str('psk'),
    token: str('token'),
    expires_at_ms: int('expires_at_ms', 0, Number.MAX_SAFE_INTEGER),
    path: str('path'),
  }
  if (launch.v !== PREVIEW_LAUNCH_VERSION)
    throw new ProtocolError(`unsupported preview launch version ${launch.v}`)
  if (!isValidPreviewLabel(launch.label)) throw new ProtocolError('preview launch: invalid label')
  if (!HEX32.test(launch.node) || !HEX32.test(launch.ticket_id))
    throw new ProtocolError('preview launch: invalid id')
  if (!HEX64.test(launch.node_public) || !HEX64.test(launch.psk))
    throw new ProtocolError('preview launch: invalid key')
  if (launch.token.length === 0) throw new ProtocolError('preview launch: empty token')
  if (!launch.path.startsWith('/') || launch.path.startsWith('//')) {
    throw new ProtocolError('preview launch: path must start with a single /')
  }
  // 浏览器把 `\` 当 `/`、并且会去掉 tab / 换行：`/\evil.example` 会变成协议相对的 URL
  if (hasForbiddenPathChar(launch.path)) {
    throw new ProtocolError(
      'preview launch: path must not contain a backslash or control characters',
    )
  }
  return launch
}

// ---------------------------------------------------------------------------
// 隧道（docs/protocol.md § 11）：二进制格式和常量。与 `crates/protocol/src/preview.rs` 的 `tunnel` 模块
// 逐字节一致，`tests/vectors/preview.json`（Rust 生成）守着。
// ---------------------------------------------------------------------------

/** 票据 id 的字节数。 */
export const TUNNEL_TICKET_ID_LEN = 16
/** 票据密钥（Noise PSK）的字节数。 */
export const TUNNEL_PSK_LEN = 32

/** 握手第一条消息 body 的版本字节。 */
export const TUNNEL_HANDSHAKE_VERSION = 1
/** 握手回复 body 的第一个字节：接受（后面是 Noise 第二条消息）。 */
export const TUNNEL_HANDSHAKE_ACCEPT = 0x00
/** 握手回复 body 的第一个字节：拒绝（后面是 UTF-8 原因）。 */
export const TUNNEL_HANDSHAKE_REJECT = 0x01
/** Noise prologue 的前缀。完整 prologue = 前缀 ‖ 0x00 ‖ label ‖ 0x00 ‖ ticket_id（16 字节）。 */
export const TUNNEL_PROLOGUE_PREFIX = 'anycon-preview-v1'

/** 一条隧道消息的明文上限（加密后仍小于一个 `WHOLE` 数据帧，所以预览隧道从不分片）。 */
export const TUNNEL_MAX_MESSAGE = 128 * 1024
/** 单个 `DATA` / `WS_MESSAGE` 帧的数据上限。 */
export const TUNNEL_MAX_DATA_FRAME = 64 * 1024
/** `REQUEST` / `RESPONSE` 头部（msgpack 编码后）上限。 */
export const TUNNEL_MAX_HEAD = 64 * 1024
/** 一条 WebSocket 消息（拼起来之后）的上限。 */
export const TUNNEL_MAX_WS_MESSAGE = 16 * 1024 * 1024
/** 每个 stream 每个方向初始的流量窗口（`DATA` 和 `WS_MESSAGE` 的数据字节）。 */
export const TUNNEL_STREAM_WINDOW = 256 * 1024
/** 一条隧道上同时打开的 stream 上限。 */
export const TUNNEL_MAX_STREAMS = 256
/** 发送方未被回执的密文上限：daemon → 预览页。 */
export const TUNNEL_DAEMON_INFLIGHT = 8 * 1024 * 1024
/** 发送方未被回执的密文上限：预览页 → daemon。 */
export const TUNNEL_CLIENT_INFLIGHT = 2 * 1024 * 1024
/** 收到这么多字节（或 `TUNNEL_ACK_EVERY_MESSAGES` 条）还没回执过、手里又没有别的要发时，立即发一条纯回执消息。 */
export const TUNNEL_ACK_EVERY_BYTES = 256 * 1024
export const TUNNEL_ACK_EVERY_MESSAGES = 32
/** 收到消息后最多等这么久就发纯回执。 */
export const TUNNEL_ACK_DELAY_MS = 50
/** WebSocket 关闭原因的字节上限（RFC 6455）。 */
export const TUNNEL_WS_CLOSE_REASON_MAX = 123

/** `WS_MESSAGE` 标志位：二进制消息（否则是文本）。 */
export const TUNNEL_WS_BINARY = 0x01
/** `WS_MESSAGE` 标志位：这是这条 WebSocket 消息的最后一段。 */
export const TUNNEL_WS_FIN = 0x02

/** 帧头：类型 1 字节 + stream 4 字节 + 长度 4 字节。 */
export const TUNNEL_FRAME_HEADER = 1 + 4 + 4
const MESSAGE_HEADER = 8

/** 帧类型（§ 11.3）。 */
export const TunnelFrameType = {
  /** P→D，msgpack `PreviewRequestHead`，打开一个 stream。 */
  Request: 0x01,
  /** D→P，msgpack `PreviewResponseHead`。 */
  Response: 0x02,
  /** 双向，HTTP 消息体的一段。占流量窗口。 */
  Data: 0x03,
  /** 双向，无载荷：这个方向的消息体结束。 */
  End: 0x04,
  /** 双向，msgpack `PreviewReset`：终止 stream。 */
  Reset: 0x05,
  /** 双向，4 字节大端：给对端这个 stream 增加的窗口。 */
  Window: 0x06,
  /** 双向，1 字节标志 + 数据：WebSocket 消息的一段。占流量窗口。 */
  WsMessage: 0x07,
  /** 双向，2 字节大端关闭码 + UTF-8 原因。 */
  WsClose: 0x08,
  /** D→P，stream 0，msgpack `PreviewGoaway`。 */
  Goaway: 0x09,
} as const
export type TunnelFrameType = (typeof TunnelFrameType)[keyof typeof TunnelFrameType]

const FRAME_TYPES: ReadonlySet<number> = new Set(Object.values(TunnelFrameType))

export interface TunnelFrame {
  type: TunnelFrameType
  stream: number
  payload: Uint8Array
}

/** 解密后的一条隧道消息。`ack` 是 u64，用 bigint 保证与 Rust 逐字节一致。 */
export interface TunnelMessage {
  /** 发送方此刻已经收到对端多少条传输消息（这条传输建立以来，累计，不含握手）。 */
  ack: bigint
  frames: TunnelFrame[]
}

export type TunnelErrorCode = 'truncated' | 'too_large' | 'version' | 'malformed'

/** 隧道字节不合法。`code` 与 Rust 的 `TunnelError` 变体对应（snake_case）。 */
export class TunnelError extends ProtocolError {
  override readonly name = 'TunnelError'
  constructor(
    readonly code: TunnelErrorCode,
    message: string = code,
  ) {
    super(message)
  }
}

export function newTunnelFrame(
  type: TunnelFrameType,
  stream: number,
  payload: Uint8Array = new Uint8Array(0),
): TunnelFrame {
  return { type, stream, payload }
}

/** 一帧编码后的字节数。 */
export function tunnelFrameLen(frame: TunnelFrame): number {
  return TUNNEL_FRAME_HEADER + frame.payload.length
}

/** 消息明文的字节数（编码前就能算，用来决定还能不能再塞一帧）。 */
export function messageLen(frames: readonly TunnelFrame[]): number {
  let n = MESSAGE_HEADER
  for (const f of frames) n += tunnelFrameLen(f)
  return n
}

/** 编码一条隧道消息。超过 `TUNNEL_MAX_MESSAGE` 抛 `TunnelError('too_large')`（调用方负责把帧分到多条消息里）。 */
export function encodeMessage(ack: bigint | number, frames: readonly TunnelFrame[]): Uint8Array {
  const len = messageLen(frames)
  if (len > TUNNEL_MAX_MESSAGE) throw new TunnelError('too_large', 'tunnel message too large')
  const big = typeof ack === 'bigint' ? ack : BigInt(ack)
  if (big < 0n || big > 0xffff_ffff_ffff_ffffn)
    throw new TunnelError('malformed', 'ack out of range')
  const out = new Uint8Array(len)
  const view = new DataView(out.buffer)
  view.setBigUint64(0, big, false)
  let at = MESSAGE_HEADER
  for (const f of frames) {
    if (!Number.isInteger(f.stream) || f.stream < 0 || f.stream > 0xffff_ffff) {
      throw new TunnelError('malformed', 'stream id out of range')
    }
    out[at] = f.type
    view.setUint32(at + 1, f.stream, false)
    view.setUint32(at + 5, f.payload.length, false)
    out.set(f.payload, at + TUNNEL_FRAME_HEADER)
    at += TUNNEL_FRAME_HEADER + f.payload.length
  }
  return out
}

/** 解码一条隧道消息。未知类型的帧跳过（前向兼容）；长度越界抛 `truncated`，超过上限抛 `too_large`。载荷是拷贝。 */
export function decodeMessage(buf: Uint8Array): TunnelMessage {
  if (buf.length > TUNNEL_MAX_MESSAGE)
    throw new TunnelError('too_large', 'tunnel message too large')
  if (buf.length < MESSAGE_HEADER) throw new TunnelError('truncated', 'truncated tunnel message')
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  const ack = view.getBigUint64(0, false)
  const frames: TunnelFrame[] = []
  let at = MESSAGE_HEADER
  while (at < buf.length) {
    if (buf.length - at < TUNNEL_FRAME_HEADER)
      throw new TunnelError('truncated', 'truncated frame header')
    const type = buf[at]!
    const stream = view.getUint32(at + 1, false)
    const len = view.getUint32(at + 5, false)
    if (buf.length - at - TUNNEL_FRAME_HEADER < len) {
      throw new TunnelError('truncated', 'truncated frame payload')
    }
    const start = at + TUNNEL_FRAME_HEADER
    if (FRAME_TYPES.has(type)) {
      frames.push({
        type: type as TunnelFrameType,
        stream,
        payload: buf.slice(start, start + len),
      })
    }
    at = start + len
  }
  return { ack, frames }
}

// ---------- 头部：msgpack，字段名为键，键序与 Rust 结构体的声明顺序一致 ----------

const REQUEST_HEAD_KEYS = ['method', 'path', 'headers', 'websocket'] as const
const RESPONSE_HEAD_KEYS = ['status', 'headers', 'encoding'] as const

const RESET_CODES: ReadonlySet<string> = new Set<PreviewResetCode>([
  'refused',
  'timeout',
  'too_large',
  'cancelled',
  'flow_control',
  'protocol',
  'limit',
  'internal',
])
const GOAWAY_CODES: ReadonlySet<string> = new Set<PreviewGoawayCode>([
  'expired',
  'closed',
  'revoked',
  'disabled',
  'port_not_allowed',
  'shutdown',
])

function packHead(value: Record<string, unknown>): Uint8Array {
  const bytes = encode(value, { ignoreUndefined: true })
  if (bytes.length > TUNNEL_MAX_HEAD) throw new TunnelError('too_large', 'tunnel head too large')
  return bytes
}

function unpackHead(payload: Uint8Array): Record<string, unknown> {
  if (payload.length > TUNNEL_MAX_HEAD) throw new TunnelError('too_large', 'tunnel head too large')
  let value: unknown
  try {
    value = decode(payload)
  } catch {
    throw new TunnelError('malformed', 'head decode')
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new TunnelError('malformed', 'head decode')
  }
  return value as Record<string, unknown>
}

function headerPairs(value: unknown): [string, string][] {
  if (!Array.isArray(value)) throw new TunnelError('malformed', 'head headers')
  return value.map((pair): [string, string] => {
    if (
      !Array.isArray(pair) ||
      pair.length !== 2 ||
      typeof pair[0] !== 'string' ||
      typeof pair[1] !== 'string'
    ) {
      throw new TunnelError('malformed', 'head headers')
    }
    return [pair[0], pair[1]]
  })
}

function optionalMessage(value: unknown): string | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== 'string') throw new TunnelError('malformed', 'head message')
  return value
}

/** `REQUEST` 载荷。键序 `method, path, headers, websocket`（`websocket` 总是写出）。 */
export function encodeRequestHead(head: PreviewRequestHead): Uint8Array {
  const src: Record<string, unknown> = { ...head, websocket: head.websocket ?? false }
  return packHead(Object.fromEntries(REQUEST_HEAD_KEYS.map((k) => [k, src[k]])))
}

export function decodeRequestHead(payload: Uint8Array): PreviewRequestHead {
  const v = unpackHead(payload)
  if (typeof v.method !== 'string' || typeof v.path !== 'string') {
    throw new TunnelError('malformed', 'request head')
  }
  if (v.websocket !== undefined && typeof v.websocket !== 'boolean') {
    throw new TunnelError('malformed', 'request head')
  }
  return {
    method: v.method,
    path: v.path,
    headers: headerPairs(v.headers),
    websocket: v.websocket === true,
  }
}

/** `RESPONSE` 载荷。键序 `status, headers, encoding`（`encoding` 总是写出）。 */
export function encodeResponseHead(head: PreviewResponseHead): Uint8Array {
  const src: Record<string, unknown> = { ...head, encoding: head.encoding ?? 'identity' }
  return packHead(Object.fromEntries(RESPONSE_HEAD_KEYS.map((k) => [k, src[k]])))
}

export function decodeResponseHead(payload: Uint8Array): PreviewResponseHead {
  const v = unpackHead(payload)
  if (
    typeof v.status !== 'number' ||
    !Number.isInteger(v.status) ||
    v.status < 0 ||
    v.status > 0xffff
  ) {
    throw new TunnelError('malformed', 'response head')
  }
  const encoding = v.encoding ?? 'identity'
  if (encoding !== 'identity' && encoding !== 'gzip')
    throw new TunnelError('malformed', 'response head')
  return { status: v.status, headers: headerPairs(v.headers), encoding }
}

/** `RESET` 载荷。键序 `code, message?`（`message` 为空时省略键）。 */
export function encodeReset(reset: PreviewReset): Uint8Array {
  return packHead(
    reset.message === undefined || reset.message === null
      ? { code: reset.code }
      : { code: reset.code, message: reset.message },
  )
}

export function decodeReset(payload: Uint8Array): PreviewReset {
  const v = unpackHead(payload)
  if (typeof v.code !== 'string' || !RESET_CODES.has(v.code))
    throw new TunnelError('malformed', 'reset')
  const message = optionalMessage(v.message)
  return message === undefined
    ? { code: v.code as PreviewResetCode }
    : { code: v.code as PreviewResetCode, message }
}

/** `GOAWAY` 载荷（只有 daemon 发）。键序 `code, message?`。 */
export function encodeGoaway(goaway: PreviewGoaway): Uint8Array {
  return packHead(
    goaway.message === undefined || goaway.message === null
      ? { code: goaway.code }
      : { code: goaway.code, message: goaway.message },
  )
}

export function decodeGoaway(payload: Uint8Array): PreviewGoaway {
  const v = unpackHead(payload)
  if (typeof v.code !== 'string' || !GOAWAY_CODES.has(v.code))
    throw new TunnelError('malformed', 'goaway')
  const message = optionalMessage(v.message)
  return message === undefined
    ? { code: v.code as PreviewGoawayCode }
    : { code: v.code as PreviewGoawayCode, message }
}

// ---------- 小载荷 ----------

/** `WINDOW` 载荷：u32 大端，给对端这个 stream 增加的窗口字节。 */
export function windowPayload(increment: number): Uint8Array {
  if (!Number.isInteger(increment) || increment < 0 || increment > 0xffff_ffff) {
    throw new TunnelError('malformed', 'window increment out of range')
  }
  const out = new Uint8Array(4)
  new DataView(out.buffer).setUint32(0, increment, false)
  return out
}

export function parseWindow(payload: Uint8Array): number {
  if (payload.length !== 4) throw new TunnelError('malformed', 'window')
  return new DataView(payload.buffer, payload.byteOffset, 4).getUint32(0, false)
}

/** `WS_MESSAGE` 载荷：标志（`TUNNEL_WS_BINARY` / `TUNNEL_WS_FIN`）‖ 数据。 */
export function wsMessage(flags: number, data: Uint8Array): Uint8Array {
  const out = new Uint8Array(1 + data.length)
  out[0] = flags
  out.set(data, 1)
  return out
}

/** 返回 `{ flags, data }`；`data` 是 `payload` 的视图。 */
export function parseWsMessage(payload: Uint8Array): { flags: number; data: Uint8Array } {
  if (payload.length === 0) throw new TunnelError('malformed', 'ws message')
  return { flags: payload[0]!, data: payload.subarray(1) }
}

/** `WS_CLOSE` 载荷：关闭码（u16 大端）‖ 原因。原因超过 `TUNNEL_WS_CLOSE_REASON_MAX` 字节时在 UTF-8 字符边界处截断。 */
export function wsClose(code: number, reason: string): Uint8Array {
  if (!Number.isInteger(code) || code < 0 || code > 0xffff) {
    throw new TunnelError('malformed', 'ws close code out of range')
  }
  const bytes = new TextEncoder().encode(reason)
  let end = Math.min(bytes.length, TUNNEL_WS_CLOSE_REASON_MAX)
  // 下标 `end` 处是 UTF-8 续字节（10xxxxxx）说明切在了字符中间，往回退到字符起点
  while (end > 0 && end < bytes.length && (bytes[end]! & 0xc0) === 0x80) end--
  const out = new Uint8Array(2 + end)
  new DataView(out.buffer).setUint16(0, code, false)
  out.set(bytes.subarray(0, end), 2)
  return out
}

export function parseWsClose(payload: Uint8Array): { code: number; reason: string } {
  if (payload.length < 2) throw new TunnelError('malformed', 'ws close')
  const code = new DataView(payload.buffer, payload.byteOffset, 2).getUint16(0, false)
  let reason: string
  try {
    reason = new TextDecoder('utf-8', { fatal: true }).decode(payload.subarray(2))
  } catch {
    throw new TunnelError('malformed', 'ws close reason')
  }
  return { code, reason }
}

// ---------- 握手 ----------

/** 握手第一条（P → D）的信封 body：版本 ‖ ticket_id ‖ Noise 消息。 */
export function handshakeInit(ticketId: Uint8Array, noise: Uint8Array): Uint8Array {
  if (ticketId.length !== TUNNEL_TICKET_ID_LEN)
    throw new TunnelError('malformed', 'ticket id length')
  const out = new Uint8Array(1 + TUNNEL_TICKET_ID_LEN + noise.length)
  out[0] = TUNNEL_HANDSHAKE_VERSION
  out.set(ticketId, 1)
  out.set(noise, 1 + TUNNEL_TICKET_ID_LEN)
  return out
}

export function parseHandshakeInit(body: Uint8Array): { ticketId: Uint8Array; noise: Uint8Array } {
  if (body.length === 0) throw new TunnelError('truncated', 'empty handshake')
  if (body[0] !== TUNNEL_HANDSHAKE_VERSION) {
    throw new TunnelError('version', `unsupported handshake version ${body[0]}`)
  }
  if (body.length < 1 + TUNNEL_TICKET_ID_LEN)
    throw new TunnelError('truncated', 'truncated handshake')
  return {
    ticketId: body.slice(1, 1 + TUNNEL_TICKET_ID_LEN),
    noise: body.slice(1 + TUNNEL_TICKET_ID_LEN),
  }
}

/** 握手回复（D → P）接受：`0x00` ‖ Noise 第二条。 */
export function handshakeAccept(noise: Uint8Array): Uint8Array {
  const out = new Uint8Array(1 + noise.length)
  out[0] = TUNNEL_HANDSHAKE_ACCEPT
  out.set(noise, 1)
  return out
}

/** 握手回复拒绝：`0x01` ‖ 原因（UTF-8）。 */
export function handshakeReject(reason: PreviewRejectReason | string): Uint8Array {
  const text = new TextEncoder().encode(reason)
  const out = new Uint8Array(1 + text.length)
  out[0] = TUNNEL_HANDSHAKE_REJECT
  out.set(text, 1)
  return out
}

export type TunnelHandshakeReply =
  | { accept: true; noise: Uint8Array }
  /** 原因保持为字符串：新版本 daemon 可能给出这一版不认识的原因。 */
  | { accept: false; reason: string }

export function parseHandshakeReply(body: Uint8Array): TunnelHandshakeReply {
  if (body.length === 0) throw new TunnelError('truncated', 'empty handshake reply')
  if (body[0] === TUNNEL_HANDSHAKE_ACCEPT) return { accept: true, noise: body.slice(1) }
  if (body[0] === TUNNEL_HANDSHAKE_REJECT) {
    try {
      return {
        accept: false,
        reason: new TextDecoder('utf-8', { fatal: true }).decode(body.subarray(1)),
      }
    } catch {
      throw new TunnelError('malformed', 'reject reason')
    }
  }
  throw new TunnelError('malformed', 'handshake reply')
}

/**
 * Noise prologue：`"anycon-preview-v1" ‖ 0x00 ‖ label ‖ 0x00 ‖ ticket_id`。两端各自算：预览页取自己的主机名，
 * daemon 取 `preview_peer.label`，hub 说谎只会让握手失败。
 */
export function previewPrologue(label: string, ticketId: Uint8Array): Uint8Array {
  if (ticketId.length !== TUNNEL_TICKET_ID_LEN)
    throw new TunnelError('malformed', 'ticket id length')
  const enc = new TextEncoder()
  const prefix = enc.encode(TUNNEL_PROLOGUE_PREFIX)
  const text = enc.encode(label)
  const out = new Uint8Array(prefix.length + 1 + text.length + 1 + TUNNEL_TICKET_ID_LEN)
  out.set(prefix, 0)
  let at = prefix.length
  out[at++] = 0
  out.set(text, at)
  at += text.length
  out[at++] = 0
  out.set(ticketId, at)
  return out
}

/** 把 16 字节随机数编码成 label（base32 小写，无填充：128 bit → 26 个字符）。 */
export function labelFromBytes(bytes: Uint8Array): string {
  if (bytes.length !== 16) throw new TunnelError('malformed', 'label needs 16 bytes')
  let out = ''
  let acc = 0
  let bits = 0
  for (const b of bytes) {
    acc = ((acc << 8) | b) >>> 0
    bits += 8
    while (bits >= 5) {
      bits -= 5
      out += PREVIEW_LABEL_ALPHABET[(acc >>> bits) & 0x1f]
    }
    acc &= (1 << bits) - 1
  }
  if (bits > 0) out += PREVIEW_LABEL_ALPHABET[(acc << (5 - bits)) & 0x1f]
  return out
}
