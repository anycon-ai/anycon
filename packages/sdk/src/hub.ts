/**
 * 与 hub 的一条 WebSocket：认证（第一帧 `Control::Auth`）、信封收发、断线重连。
 * 只管外层；Noise 和 JSON-RPC 在 link.ts。
 */
import {
  CLOSE_CODE,
  PROTOCOL_VERSION,
  UnknownControlError,
  controlBody,
  controlEnvelope,
  decodeEnvelope,
  encodeEnvelope,
  makeEnvelope,
  type AuthFailReason,
  type Control,
  type ClientInfo,
  type Envelope,
} from '@anycon/protocol'

import { Emitter } from './emitter.js'
import { ConnectionError, NodeRejected, PairingRequired, ProtocolMismatch } from './errors.js'
import {
  WS_OPEN,
  defaultWebSocket,
  toBytes,
  type WebSocketFactory,
  type WebSocketLike,
} from './ws.js'

export type AuthOk = Extract<Control, { type: 'auth_ok' }>

/**
 * `rate_limited { retry_after_ms }` 最多信到多久，毫秒（防对端给出荒谬的值把连接挂死）。
 * docs/protocol.md § 3.1：发送方可以把它封顶在 30 s。
 */
export const RATE_WAIT_MAX_MS = 30_000

export interface BackoffOptions {
  /** 首次重连等待，毫秒。默认 1000。 */
  initialMs?: number
  /** 上限，毫秒。默认 30000。 */
  maxMs?: number
}

export interface HubConnectionOptions {
  /** WebSocket 地址（`wss://…/v1/ws`）。 */
  url: string
  /** 设备 token；缺省 = 「只为配对而来」的临时连接。 */
  token?: string
  /** 已知的设备 id（auth 帧的 `from`）；临时连接留空。 */
  deviceId?: string
  client: ClientInfo
  webSocket?: WebSocketFactory
  /** WebSocket 升级请求附带的 HTTP 头（仅 Node / Bun），见 `WebSocketInit.headers`。 */
  headers?: Record<string, string>
  /** 掉线后自动重连。默认 false。 */
  reconnect?: boolean | BackoffOptions
  /** 等 `auth_ok` 的时间。默认 10 秒。 */
  authTimeoutMs?: number
}

export interface HubEvents {
  authed: [AuthOk]
  /** 除 auth / auth_ok / auth_fail 之外的控制消息。 */
  control: [Control]
  /** handshake / data / notify 信封。 */
  envelope: [Envelope]
  disconnected: [info: { code: number; reason: string; willReconnect: boolean }]
  /** 不会再重连的终止性错误（被顶掉、被吊销、token 无效……）。 */
  fatal: [Error]
}

type State = 'idle' | 'connecting' | 'authenticating' | 'ready' | 'backoff' | 'closed'

function authFailError(reason: AuthFailReason): Error {
  switch (reason) {
    case 'invalid':
      return new PairingRequired('the hub rejected this device token: pair this device again')
    case 'revoked':
      return new NodeRejected('this device has been revoked by the hub: pair it again')
    case 'version':
      return new ProtocolMismatch(
        PROTOCOL_VERSION,
        undefined,
        'the hub does not accept this protocol version',
      )
    case 'quota':
      return new ConnectionError('the hub refused the connection: quota exceeded')
    case 'protocol':
      return new ConnectionError('the hub refused the connection: protocol error')
  }
}

export class HubConnection extends Emitter<HubEvents> {
  readonly #opts: HubConnectionOptions
  readonly #factory: WebSocketFactory
  readonly #backoff: Required<BackoffOptions>
  readonly #authTimeoutMs: number
  #state: State = 'idle'
  #ws: WebSocketLike | undefined
  #deviceId: string
  #seq = 0
  #attempt = 0
  #closedByUs = false
  #first: { resolve: (ok: AuthOk) => void; reject: (e: unknown) => void } | undefined
  #authTimer: ReturnType<typeof setTimeout> | undefined
  #retryTimer: ReturnType<typeof setTimeout> | undefined
  #pendingFatal: Error | undefined
  #lastError: unknown
  /**
   * `rate_limited` 要求暂停到什么时候（绝对时间，毫秒）。速率预算是 hub 按**整条连接**记的，不分节点，
   * 所以它记在连接上，同一条连接上的所有 `NodeLink` 共用；新开一条 WebSocket 时清零。
   */
  #rateUntil = 0

  constructor(options: HubConnectionOptions) {
    super()
    this.#opts = options
    this.#factory = options.webSocket ?? defaultWebSocket
    const r = typeof options.reconnect === 'object' ? options.reconnect : {}
    this.#backoff = { initialMs: r.initialMs ?? 1000, maxMs: r.maxMs ?? 30_000 }
    this.#authTimeoutMs = options.authTimeoutMs ?? 10_000
    this.#deviceId = options.deviceId ?? ''
  }

  /**
   * 配对完成后同一条连接就是正式设备：之后 handshake / data 信封的 `from` 要用 `PairDone.device_id`
   * （旧的临时 id 不再有效）。
   */
  adoptDeviceId(id: string): void {
    this.#deviceId = id
  }

  /** hub 分配 / 确认的设备 id（`auth_ok` 之后可用）。 */
  get deviceId(): string {
    return this.#deviceId
  }

  /** 到目前为止发出的最大 seq（整条 hub 连接上唯一递增，control 帧也占号）。 */
  get lastSeq(): number {
    return this.#seq
  }

  /**
   * hub 通过 `rate_limited` 要求这条连接（所有节点、含重发）暂停到什么时候，绝对时间（毫秒）；
   * 没有限速或已过期是 0 或一个过去的时刻。封顶 {@link RATE_WAIT_MAX_MS}，重连后清零。
   */
  get rateUntil(): number {
    return this.#rateUntil
  }

  get ready(): boolean {
    return this.#state === 'ready'
  }

  get closed(): boolean {
    return this.#state === 'closed'
  }

  /** 连接并认证。首次失败直接拒绝；成功之后按 `reconnect` 选项自动重连。 */
  start(): Promise<AuthOk> {
    if (this.#state !== 'idle') return Promise.reject(new Error('HubConnection already started'))
    return new Promise<AuthOk>((resolve, reject) => {
      this.#first = { resolve, reject }
      void this.#open()
    })
  }

  sendControl(ctrl: Control): number {
    const seq = this.#nextSeq()
    // 发给 hub 的 control 帧 from / to 都填空串（hub 不看 from）。
    this.#write(encodeEnvelope(controlEnvelope('', seq, ctrl)))
    return seq
  }

  /** 发 handshake / data / notify 信封，返回 seq。 */
  sendEnvelope(to: string, kind: 'handshake' | 'data' | 'notify', body: Uint8Array): number {
    return this.sendEncoded(to, kind, body).seq
  }

  /** 同 `sendEnvelope`，另外返回编码后的整帧（背压重发要原样再发一遍）。 */
  sendEncoded(
    to: string,
    kind: 'handshake' | 'data' | 'notify',
    body: Uint8Array,
  ): { seq: number; frame: Uint8Array } {
    const seq = this.#nextSeq()
    const frame = encodeEnvelope(makeEnvelope(this.#deviceId, to, kind, seq, body))
    this.#write(frame)
    return { seq, frame }
  }

  /** 重发一帧已编码的信封（背压重试用）。 */
  sendRaw(bytes: Uint8Array): void {
    this.#write(bytes)
  }

  close(code = 1000, reason = 'client closing'): void {
    this.#closedByUs = true
    this.#state = 'closed'
    this.#rateUntil = 0
    this.#clearTimers()
    const ws = this.#ws
    this.#ws = undefined
    if (ws) {
      detach(ws)
      try {
        ws.close(code, reason)
      } catch {
        // 已经关了
      }
    }
    this.#first?.reject(new ConnectionError('connection closed before authentication finished'))
    this.#first = undefined
  }

  #nextSeq(): number {
    this.#seq += 1
    return this.#seq
  }

  #write(bytes: Uint8Array): void {
    const ws = this.#ws
    if (!ws || ws.readyState !== WS_OPEN || this.#state !== 'ready') {
      throw new ConnectionError('not connected to the hub')
    }
    ws.send(bytes)
  }

  async #open(): Promise<void> {
    this.#state = 'connecting'
    this.#pendingFatal = undefined
    this.#lastError = undefined
    this.#rateUntil = 0 // 速率预算是按 hub 连接算的，新连接重新开始
    let ws: WebSocketLike
    try {
      ws = await this.#factory(
        this.#opts.url,
        this.#opts.headers ? { headers: this.#opts.headers } : undefined,
      )
    } catch (cause) {
      this.#failAttempt(
        new ConnectionError(`cannot create WebSocket: ${String(cause)}`, { cause }),
        1006,
        '',
      )
      return
    }
    if (this.#closedByUs) {
      detach(ws)
      ws.close(1000, 'closed')
      return
    }
    this.#ws = ws
    ws.binaryType = 'arraybuffer'
    ws.onopen = () => {
      if (this.#ws !== ws) return
      this.#state = 'authenticating'
      this.#authTimer = setTimeout(
        () => ws.close(CLOSE_CODE.AUTH_TIMEOUT, 'auth timeout'),
        this.#authTimeoutMs,
      )
      const auth: Control = {
        type: 'auth',
        ...(this.#opts.token ? { device_token: this.#opts.token } : {}),
        protocol_version: PROTOCOL_VERSION,
        client: this.#opts.client,
      }
      try {
        const frame = encodeEnvelope(controlEnvelope('', this.#nextSeq(), auth))
        ws.send(frame)
      } catch (cause) {
        this.#pendingFatal = new ConnectionError('failed to send auth frame', { cause })
        ws.close(1011, 'auth send failed')
      }
    }
    ws.onmessage = (event) => {
      if (this.#ws !== ws) return
      const bytes = toBytes(event.data)
      if (bytes) this.#onFrame(ws, bytes)
    }
    ws.onerror = (event) => {
      this.#lastError = event
    }
    ws.onclose = (event) => {
      if (this.#ws !== ws) return
      this.#onClose(event.code, event.reason)
    }
  }

  #onFrame(ws: WebSocketLike, bytes: Uint8Array): void {
    let env: Envelope
    try {
      env = decodeEnvelope(bytes)
    } catch {
      return // 坏帧丢弃，不断连接
    }
    if (env.kind !== 'control') {
      this.emit('envelope', env)
      return
    }
    let ctrl: Control
    try {
      ctrl = controlBody(env)
    } catch (err) {
      if (err instanceof UnknownControlError) return // 版本错位时 hub 新增的控制消息：忽略
      return
    }
    switch (ctrl.type) {
      case 'auth_ok':
        this.#clearAuthTimer()
        this.#state = 'ready'
        this.#attempt = 0
        this.#deviceId = ctrl.device_id
        if (this.#first) {
          this.#first.resolve(ctrl)
          this.#first = undefined
        }
        this.emit('authed', ctrl)
        return
      case 'auth_fail':
        this.#pendingFatal = authFailError(ctrl.reason)
        this.#clearAuthTimer()
        // hub 随后会关连接；主动关一下，避免半开。
        try {
          ws.close(1000, 'auth failed')
        } catch {
          // ignore
        }
        return
      case 'rate_limited': {
        // 先记在连接上、再广播：各 NodeLink 的 onControl 读到的就是更新后的值。
        // 取更晚的那个：迟到的、较短的提示不能把已有的更长的暂停缩短。
        const wait = Math.min(Math.max(ctrl.retry_after_ms, 0), RATE_WAIT_MAX_MS)
        this.#rateUntil = Math.max(this.#rateUntil, Date.now() + wait + Math.random() * 50)
        this.emit('control', ctrl)
        return
      }
      default:
        this.emit('control', ctrl)
    }
  }

  #onClose(code: number, reason: string): void {
    this.#clearTimers()
    this.#ws = undefined
    if (this.#closedByUs || this.#state === 'closed') return
    const wasReady = this.#state === 'ready'
    const fatal = this.#pendingFatal ?? this.#fatalForCode(code)
    if (fatal) {
      this.#terminate(fatal, wasReady, code, reason)
      return
    }
    if (!wasReady) {
      // 认证完成前断开：首次尝试 → 直接失败；重连中 → 继续退避。
      const detail = describeError(this.#lastError)
      this.#failAttempt(
        new ConnectionError(
          `hub connection closed before auth_ok (code ${code}${reason ? `: ${reason}` : ''})${detail ? `: ${detail}` : ''}`,
          { closeCode: code },
        ),
        code,
        reason,
      )
      return
    }
    const willReconnect = this.#opts.reconnect !== undefined && this.#opts.reconnect !== false
    this.emit('disconnected', { code, reason, willReconnect })
    if (willReconnect) this.#scheduleReconnect(code)
    else this.#state = 'closed'
  }

  #fatalForCode(code: number): Error | undefined {
    if (code === CLOSE_CODE.SUPERSEDED) {
      return new ConnectionError('another connection of this device took over (close code 4001)', {
        closeCode: code,
      })
    }
    if (code === CLOSE_CODE.REVOKED) {
      return new NodeRejected(
        'this device has been revoked by the hub (close code 4004): pair it again',
      )
    }
    return undefined
  }

  /** 一次连接尝试在认证前失败。 */
  #failAttempt(error: Error, code: number, reason: string): void {
    if (this.#first) {
      this.#state = 'closed'
      const first = this.#first
      this.#first = undefined
      first.reject(error)
      return
    }
    const willReconnect = this.#opts.reconnect !== undefined && this.#opts.reconnect !== false
    if (willReconnect) {
      this.#scheduleReconnect(code)
    } else {
      this.#terminate(error, false, code, reason)
    }
  }

  #terminate(error: Error, wasReady: boolean, code: number, reason: string): void {
    this.#state = 'closed'
    this.#clearTimers()
    const first = this.#first
    this.#first = undefined
    if (first) {
      first.reject(error)
      return
    }
    if (wasReady) this.emit('disconnected', { code, reason, willReconnect: false })
    this.emit('fatal', error)
  }

  #scheduleReconnect(code: number): void {
    this.#state = 'backoff'
    const attempt = this.#attempt++
    // 4002 restarting：hub 在优雅关停，1–3 秒抖动重连，而不是指数退避。
    const delay =
      code === CLOSE_CODE.RESTARTING
        ? 1000 + Math.random() * 2000
        : Math.min(this.#backoff.maxMs, this.#backoff.initialMs * 2 ** attempt) *
          (0.5 + Math.random() * 0.5)
    this.#retryTimer = setTimeout(() => {
      this.#retryTimer = undefined
      if (!this.#closedByUs) void this.#open()
    }, delay)
  }

  #clearAuthTimer(): void {
    if (this.#authTimer !== undefined) clearTimeout(this.#authTimer)
    this.#authTimer = undefined
  }

  #clearTimers(): void {
    this.#clearAuthTimer()
    if (this.#retryTimer !== undefined) clearTimeout(this.#retryTimer)
    this.#retryTimer = undefined
  }
}

function describeError(event: unknown): string {
  if (typeof event !== 'object' || event === null) return ''
  const e = event as { message?: unknown; error?: { message?: unknown } }
  const message = e.message ?? e.error?.message
  return typeof message === 'string' ? message : ''
}

function detach(ws: WebSocketLike): void {
  ws.onopen = null
  ws.onmessage = null
  ws.onclose = null
  ws.onerror = null
}
