// 预览隧道的连接层（docs/protocol.md § 11.1–11.2、spec/web.md § 13.2「隧道」）：
// 连 hub 的预览 WebSocket、用预览令牌认证、等 daemon 在线、做 NKpsk0 握手，然后把 `TunnelSession` 交给上层。
// 每个 service worker 实例一条。
//
// 状态：idle → connecting → waiting_peer → handshaking → ready；断线按 0.5 秒起翻倍封顶 10 秒重连（backoff）。
// 终态（之后的请求直接返回错误页、不再重连）：`auth_fail { invalid }`（以及 version / protocol / revoked）、关闭码 4004、
// `GOAWAY`、握手被拒、握手失败（对端不是钉住的 daemon）、票据过期。
import {
  CLOSE_CODE,
  PROTOCOL_VERSION,
  TUNNEL_TICKET_ID_LEN,
  bytesOfHex,
  controlEnvelope,
  decodeControl,
  decodeEnvelope,
  decodeFrame,
  encodeEnvelope,
  encodeFrames,
  handshakeInit,
  makeEnvelope,
  parseHandshakeReply,
  previewPrologue,
  resetBody,
  type Control,
  type Handshake,
  type PreviewCrypto,
  type PreviewGoaway,
  type PreviewHello,
  type PreviewLaunch,
  type PreviewRejectReason,
  UnknownControlError,
} from '@anycon/protocol'
import { TunnelSender } from './sender'
import { TunnelSession } from './session'

/** 页面能看懂的「隧道不可用」原因。终态的原因之后的请求都一样，非终态的会重试。 */
export type UnavailableKind =
  | 'no_launch'
  | 'expired'
  | 'unauthorized'
  | 'revoked'
  | 'version'
  | 'closed'
  | 'disabled'
  | 'port_not_allowed'
  | 'shutdown'
  | 'rejected'
  | 'handshake_failed'
  | 'crypto_unavailable'
  | 'node_offline'
  | 'connecting'

const TERMINAL_KINDS: ReadonlySet<UnavailableKind> = new Set([
  'no_launch',
  'expired',
  'unauthorized',
  'revoked',
  'version',
  'closed',
  'disabled',
  'port_not_allowed',
  'shutdown',
  'rejected',
  'handshake_failed',
  'crypto_unavailable',
])

export class TunnelUnavailable extends Error {
  override readonly name = 'TunnelUnavailable'
  constructor(
    readonly kind: UnavailableKind,
    readonly detail?: string,
  ) {
    super(detail ? `${kind}: ${detail}` : kind)
  }
  get terminal(): boolean {
    return TERMINAL_KINDS.has(this.kind)
  }
}

export type TunnelState =
  | { phase: 'idle' }
  | { phase: 'connecting' }
  | { phase: 'waiting_peer' }
  | { phase: 'handshaking' }
  | { phase: 'ready' }
  | { phase: 'backoff'; retryInMs: number; reason: string }
  | { phase: 'terminal'; kind: UnavailableKind; detail?: string }

export interface SocketLike {
  binaryType: string
  readonly readyState: number
  onopen: ((ev: unknown) => void) | null
  onmessage: ((ev: { data: unknown }) => void) | null
  onclose: ((ev: { code: number; reason?: string }) => void) | null
  onerror: ((ev: unknown) => void) | null
  send(data: Uint8Array): void
  close(code?: number, reason?: string): void
}

export interface LinkDeps {
  launch: PreviewLaunch
  /** `wss://<自己的主机>/v1/ws`（本机开发是 `ws://`）。 */
  wsUrl: string
  createSocket(url: string): SocketLike
  crypto(): Promise<PreviewCrypto>
  /** 自报的客户端信息。 */
  clientVersion?: string
  origin?: string
  rng?: () => number
}

const WS_OPEN = 1
/** 重连退避：0.5 秒起，翻倍，封顶 10 秒。 */
const BACKOFF_START_MS = 500
const BACKOFF_CAP_MS = 10_000
/** 连上并稳定这么久之后，退避清零。 */
const STABLE_AFTER_MS = 10_000
/** 一次握手等多久没回复就算超时；最多试这么多次，再不行就断开重连。 */
const HANDSHAKE_TIMEOUT_MS = 8_000
const HANDSHAKE_ATTEMPTS = 3
/** 请求最多等多久隧道就绪（连 hub、认证、握手）。 */
const READY_TIMEOUT_MS = 20_000
/** 节点不在线时，请求最多等多久它上线。 */
const PEER_GRACE_MS = 3_000
/** 传输连续失步这么多次（30 秒内）就换一条 hub 连接。 */
const MAX_RESETS = 6
const RESET_WINDOW_MS = 30_000

interface Waiter {
  resolve(session: TunnelSession): void
  reject(err: TunnelUnavailable): void
  timer: ReturnType<typeof setTimeout>
}

interface PendingHandshake {
  hs: Handshake
  gen: number
}

export class PreviewTunnel {
  private current: TunnelState = { phase: 'idle' }
  private readonly listeners = new Set<(s: TunnelState) => void>()
  private readonly waiters = new Set<Waiter>()
  private sock: SocketLike | null = null
  /** 每个 WebSocket 一个代号：旧连接晚到的事件一律忽略。 */
  private sockGen = 0
  private seq = 0
  private selfId = ''
  private authed = false
  private peerOnline = false
  private sender: TunnelSender | null = null
  private activeSession: TunnelSession | null = null
  private pending: PendingHandshake | null = null
  private hsGen = 0
  private hsAttempts = 0
  private hsFailures = 0
  private hsTimer: ReturnType<typeof setTimeout> | null = null
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private stableTimer: ReturnType<typeof setTimeout> | null = null
  private peerTimer: ReturnType<typeof setTimeout> | null = null
  private expiryTimer: ReturnType<typeof setTimeout> | null = null
  private attempt = 0
  private resets: number[] = []
  private started = false
  private expiresAt: number

  constructor(private readonly deps: LinkDeps) {
    this.expiresAt = deps.launch.expires_at_ms
  }

  get state(): TunnelState {
    return this.current
  }

  get session(): TunnelSession | null {
    return this.activeSession
  }

  get launch(): PreviewLaunch {
    return this.deps.launch
  }

  onState(listener: (s: TunnelState) => void): () => void {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  // ---- 生命周期 ----

  start(): void {
    if (this.started) return
    this.started = true
    this.armExpiry()
    if (Date.now() >= this.expiresAt) return this.terminal('expired')
    this.connect()
  }

  /** 安静地结束（换了新的启动参数、测试收尾）：关连接、清定时器，不留终态。 */
  stop(): void {
    this.started = false
    this.clearTimers()
    this.teardownSocket(1000, 'stop')
    this.dropSession('stopped')
    this.setState({ phase: 'idle' })
    this.failWaiters(new TunnelUnavailable('connecting', 'stopped'))
  }

  /** 等隧道就绪。不可用时抛 `TunnelUnavailable`：终态立刻抛，节点离线等 3 秒，其余最多等 20 秒。 */
  whenReady(): Promise<TunnelSession> {
    if (this.current.phase === 'ready' && this.activeSession) return Promise.resolve(this.activeSession)
    if (this.current.phase === 'terminal') return Promise.reject(new TunnelUnavailable(this.current.kind, this.current.detail))
    this.start()
    return new Promise<TunnelSession>((resolve, reject) => {
      const waiter: Waiter = {
        resolve,
        reject,
        timer: setTimeout(() => {
          this.waiters.delete(waiter)
          reject(this.transientError())
        }, READY_TIMEOUT_MS),
      }
      this.waiters.add(waiter)
      this.armPeerGrace()
    })
  }

  private transientError(): TunnelUnavailable {
    if (this.current.phase === 'waiting_peer') return new TunnelUnavailable('node_offline')
    const detail = this.current.phase === 'backoff' ? this.current.reason : undefined
    return new TunnelUnavailable('connecting', detail)
  }

  private setState(next: TunnelState): void {
    this.current = next
    if (next.phase !== 'waiting_peer' && this.peerTimer) {
      clearTimeout(this.peerTimer)
      this.peerTimer = null
    }
    this.armPeerGrace()
    for (const l of [...this.listeners]) l(next)
    if (next.phase === 'ready' && this.activeSession) {
      const session = this.activeSession
      for (const w of this.waiters) {
        clearTimeout(w.timer)
        w.resolve(session)
      }
      this.waiters.clear()
    } else if (next.phase === 'terminal') {
      this.failWaiters(new TunnelUnavailable(next.kind, next.detail))
    }
  }

  /** 节点不在线时，等它上线的请求最多等 3 秒。 */
  private armPeerGrace(): void {
    if (this.current.phase !== 'waiting_peer' || this.peerTimer || this.waiters.size === 0) return
    this.peerTimer = setTimeout(() => {
      this.peerTimer = null
      this.failWaiters(new TunnelUnavailable('node_offline'))
    }, PEER_GRACE_MS)
  }

  private failWaiters(err: TunnelUnavailable): void {
    for (const w of this.waiters) {
      clearTimeout(w.timer)
      w.reject(err)
    }
    this.waiters.clear()
  }

  private clearTimers(): void {
    for (const t of [this.hsTimer, this.reconnectTimer, this.stableTimer, this.peerTimer, this.expiryTimer]) if (t) clearTimeout(t)
    this.hsTimer = this.reconnectTimer = this.stableTimer = this.peerTimer = this.expiryTimer = null
  }

  private armExpiry(): void {
    if (this.expiryTimer) clearTimeout(this.expiryTimer)
    // setTimeout 的上限是 2^31 - 1 ms：更久的到点再重新算
    const wait = Math.min(Math.max(this.expiresAt - Date.now(), 0), 0x7fff_ffff)
    this.expiryTimer = setTimeout(() => {
      this.expiryTimer = null
      if (Date.now() >= this.expiresAt) this.terminal('expired')
      else this.armExpiry()
    }, wait)
  }

  /** 进入终态：关连接、失败全部 stream 和等待者，不再重连。 */
  private terminal(kind: UnavailableKind, detail?: string): void {
    if (this.current.phase === 'terminal') return
    this.clearTimers()
    this.teardownSocket(1000, kind)
    this.dropSession(kind)
    this.setState({ phase: 'terminal', kind, detail })
  }

  // ---- hub 连接 ----

  private connect(): void {
    if (!this.started || this.current.phase === 'terminal') return
    this.reconnectTimer = null
    this.setState({ phase: 'connecting' })
    const gen = ++this.sockGen
    this.seq = 0
    this.selfId = ''
    this.authed = false
    this.peerOnline = false
    let sock: SocketLike
    try {
      sock = this.deps.createSocket(this.deps.wsUrl)
    } catch (err) {
      this.scheduleReconnect(err instanceof Error ? err.message : 'websocket could not be created')
      return
    }
    sock.binaryType = 'arraybuffer'
    this.sock = sock
    this.sender = new TunnelSender({
      peer: this.deps.launch.node,
      rng: this.deps.rng,
      write: (bytes) => this.rawSend(bytes),
      needReset: (reason) => this.resetTransport(reason, true),
    })
    sock.onopen = () => {
      if (gen === this.sockGen) this.sendAuth()
    }
    sock.onmessage = (ev) => {
      if (gen === this.sockGen) this.onSocketData(ev.data)
    }
    sock.onerror = () => {
      // 接着一定会有 close
    }
    sock.onclose = (ev) => {
      if (gen === this.sockGen) this.onSocketClosed(ev.code)
    }
  }

  private rawSend(bytes: Uint8Array): boolean {
    const sock = this.sock
    if (!sock || sock.readyState !== WS_OPEN) return false
    try {
      sock.send(bytes)
      return true
    } catch {
      return false
    }
  }

  private sendAuth(): void {
    const { launch } = this.deps
    const auth: Control = {
      type: 'auth',
      protocol_version: PROTOCOL_VERSION,
      preview_token: launch.token,
      client: {
        name: 'Anycon Preview',
        kind: 'web',
        platform: 'browser',
        version: this.deps.clientVersion ?? 'dev',
        origin: this.deps.origin ?? null,
      },
    }
    this.rawSend(encodeEnvelope(controlEnvelope('', ++this.seq, auth)))
  }

  private teardownSocket(code: number, reason: string): void {
    const sock = this.sock
    this.sockGen++ // 之后这条连接的事件都当作过期
    this.sock = null
    this.authed = false
    this.peerOnline = false
    this.sender?.clear()
    this.sender = null
    if (sock) {
      sock.onopen = sock.onmessage = sock.onclose = sock.onerror = null
      try {
        sock.close(code, reason.slice(0, 100))
      } catch {
        // 已经关了
      }
    }
  }

  private onSocketClosed(code: number): void {
    this.sock = null
    this.authed = false
    this.peerOnline = false
    this.sender?.clear()
    this.sender = null
    this.dropSession('connection to the hub closed')
    if (code === CLOSE_CODE.REVOKED) return this.terminal('revoked', 'closed by the hub (4004)')
    // 4002：hub 在重启，1–3 秒后再来，不当作失败
    if (code === CLOSE_CODE.RESTARTING) return this.scheduleReconnect('hub restarting', 1_000 + this.rng() * 2_000, false)
    this.scheduleReconnect(`websocket closed (${code})`)
  }

  private rng(): number {
    return (this.deps.rng ?? Math.random)()
  }

  private scheduleReconnect(reason: string, delay?: number, countFailure = true): void {
    if (!this.started || this.current.phase === 'terminal') return
    if (this.stableTimer) clearTimeout(this.stableTimer)
    this.stableTimer = null
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = null
    const wait = delay ?? Math.min(BACKOFF_START_MS * 2 ** this.attempt, BACKOFF_CAP_MS) * (0.8 + this.rng() * 0.4)
    if (countFailure) this.attempt += 1
    this.setState({ phase: 'backoff', retryInMs: Math.round(wait), reason })
    this.reconnectTimer = setTimeout(() => this.connect(), wait)
  }

  /** 这条 hub 连接用不了了（握手一直没回、传输一直失步）：关掉重连。 */
  private failConnection(reason: string): void {
    this.teardownSocket(1000, reason)
    this.dropSession(reason)
    this.scheduleReconnect(reason)
  }

  // ---- hub 消息 ----

  private onSocketData(data: unknown): void {
    if (!(data instanceof ArrayBuffer)) return
    let env
    try {
      env = decodeEnvelope(new Uint8Array(data))
    } catch {
      return
    }
    switch (env.kind) {
      case 'control': {
        let ctrl: Control
        try {
          ctrl = decodeControl(env.body)
        } catch (err) {
          if (err instanceof UnknownControlError) return
          return
        }
        return this.onControl(ctrl)
      }
      case 'handshake':
        if (env.from === this.deps.launch.node) this.onHandshakeReply(env.body)
        return
      case 'data':
        if (env.from === this.deps.launch.node) this.onData(env.body)
        return
      default:
        return
    }
  }

  private onControl(ctrl: Control): void {
    const node = this.deps.launch.node
    switch (ctrl.type) {
      case 'auth_ok':
        this.selfId = ctrl.device_id
        this.authed = true
        this.stableTimer = setTimeout(() => {
          this.attempt = 0
        }, STABLE_AFTER_MS)
        if (ctrl.peers_online.includes(node)) {
          this.peerOnline = true
          void this.beginHandshake()
        } else this.setState({ phase: 'waiting_peer' })
        return
      case 'auth_fail':
        if (ctrl.reason === 'version' || ctrl.reason === 'protocol') return this.terminal('version', ctrl.reason)
        if (ctrl.reason === 'quota') return this.failConnection('too many previews are open (quota)')
        return this.terminal('unauthorized', ctrl.reason)
      case 'peer_online':
        if (ctrl.device !== node || !this.authed) return
        this.peerOnline = true
        void this.beginHandshake()
        return
      case 'peer_offline':
        if (ctrl.device !== node) return
        this.peerOnline = false
        this.dropSession('the machine went offline')
        this.setState({ phase: 'waiting_peer' })
        return
      case 'backpressure':
        this.sender?.onBackpressure(ctrl.seq)
        return
      case 'rate_limited':
        this.sender?.onRateLimited(ctrl.retry_after_ms, ctrl.to)
        return
      case 'undeliverable':
        this.sender?.forget(ctrl.seq)
        return
      case 'dropped':
        // 发给 daemon 的 data 信封被 hub 丢了（太大、不是对端、超速）：这条传输少了一条消息，重来
        if (this.activeSession) this.resetTransport(`hub dropped envelope ${ctrl.seq} (${ctrl.reason})`, true)
        return
      default:
        return
    }
  }

  // ---- 握手 ----

  private async beginHandshake(): Promise<void> {
    this.dropSession('handshake restarted')
    const gen = ++this.hsGen
    const sockGen = this.sockGen
    if (!this.authed || !this.peerOnline) return
    this.setState({ phase: 'handshaking' })
    let crypto: PreviewCrypto
    try {
      crypto = await this.deps.crypto()
    } catch (err) {
      if (gen === this.hsGen) this.terminal('crypto_unavailable', err instanceof Error ? err.message : String(err))
      return
    }
    if (gen !== this.hsGen || sockGen !== this.sockGen || !this.sender) return

    const { launch } = this.deps
    const ticketId = bytesOfHex(launch.ticket_id)
    const psk = bytesOfHex(launch.psk)
    let hs: Handshake
    let first: Uint8Array
    try {
      hs = crypto.previewInitiator(bytesOfHex(launch.node_public), psk, previewPrologue(launch.label, ticketId))
      first = hs.write()
    } catch (err) {
      psk.fill(0)
      this.terminal('handshake_failed', err instanceof Error ? err.message : String(err))
      return
    }
    psk.fill(0)
    if (ticketId.length !== TUNNEL_TICKET_ID_LEN) return this.terminal('handshake_failed', 'bad ticket id')
    this.pending = { hs, gen }
    const body = handshakeInit(ticketId, first)
    this.sender.sendHandshake(++this.seq, encodeEnvelope(makeEnvelope(this.selfId, launch.node, 'handshake', this.seq, body)))
    this.armHandshakeTimer(gen)
  }

  private armHandshakeTimer(gen: number): void {
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = setTimeout(() => {
      this.hsTimer = null
      if (gen !== this.hsGen || !this.pending) return
      // 握手被 hub 限速扣着的时候，对端根本没看到：不算超时
      if (this.sender?.handshakeHeld) return this.armHandshakeTimer(gen)
      this.hsAttempts += 1
      if (this.hsAttempts >= HANDSHAKE_ATTEMPTS) {
        this.hsAttempts = 0
        return this.failConnection('the machine did not answer the handshake')
      }
      void this.beginHandshake()
    }, HANDSHAKE_TIMEOUT_MS)
  }

  private onHandshakeReply(body: Uint8Array): void {
    const pending = this.pending
    if (!pending || pending.gen !== this.hsGen) return
    let reply
    try {
      reply = parseHandshakeReply(body)
    } catch {
      return
    }
    this.pending = null
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = null
    this.sender?.handshakeDone()
    if (!reply.accept) {
      pending.hs.free()
      return this.terminal(rejectKind(reply.reason), reply.reason)
    }
    let hello: PreviewHello
    let transport
    try {
      const payload = pending.hs.read(reply.noise)
      if (!pending.hs.isFinished()) throw new Error('handshake did not finish')
      transport = pending.hs.intoTransport()
      hello = parseHello(payload)
    } catch (err) {
      pending.hs.free()
      // 回复读不出来：多半是对端不是钉住的 daemon（连续几次就判定失败），偶尔也可能是上一次尝试的迟到回复
      this.hsFailures += 1
      if (this.hsFailures >= HANDSHAKE_ATTEMPTS) {
        return this.terminal('handshake_failed', err instanceof Error ? err.message : String(err))
      }
      void this.beginHandshake()
      return
    }
    if (hello.port !== this.deps.launch.port) {
      transport.free()
      return this.terminal('handshake_failed', 'the machine answered for a different port')
    }
    this.expiresAt = Math.min(this.expiresAt, hello.expires_at_ms)
    this.armExpiry()
    this.hsAttempts = 0
    this.hsFailures = 0
    const sender = this.sender!
    const node = this.deps.launch.node
    const session: TunnelSession = new TunnelSession({
      transport,
      send: (ciphertext, index) => {
        const bodies = encodeFrames(ciphertext, 0)
        const seq = ++this.seq
        sender.sendData(seq, index, encodeEnvelope(makeEnvelope(this.selfId, node, 'data', seq, bodies[0]!)))
      },
      inflightBytes: () => sender.inflightBytes,
      onPeerAck: (count) => {
        sender.onAck(count)
        session.pump()
      },
      onGoaway: (g) => this.onGoaway(g),
      onDesync: (reason) => this.resetTransport(reason, true),
    })
    this.activeSession = session
    this.setState({ phase: 'ready' })
  }

  private onGoaway(goaway: PreviewGoaway): void {
    this.terminal(goawayKind(goaway.code), goaway.message ?? undefined)
  }

  // ---- 数据 ----

  private onData(body: Uint8Array): void {
    const session = this.activeSession
    if (!session) return
    let frame
    try {
      frame = decodeFrame(body)
    } catch (err) {
      return this.resetTransport(err instanceof Error ? err.message : 'bad data frame', true)
    }
    switch (frame.kind) {
      case 'whole':
        session.recv(frame.data)
        return
      case 'reset':
        return this.resetTransport('the machine reset the transport', false)
      case 'fragment':
        // 预览隧道从不分片
        return this.resetTransport('unexpected fragment', true)
    }
  }

  /** 传输失步：丢掉它和上面所有 stream，需要的话通知对端，然后重新握手。 */
  private resetTransport(reason: string, notifyPeer: boolean): void {
    if (!this.sender || this.current.phase === 'terminal') return
    const now = Date.now()
    this.resets = this.resets.filter((t) => now - t < RESET_WINDOW_MS)
    this.resets.push(now)
    if (this.resets.length > MAX_RESETS) {
      this.resets = []
      return this.failConnection(`transport kept resetting: ${reason}`)
    }
    if (notifyPeer && this.selfId) {
      this.sender.sendRaw(encodeEnvelope(makeEnvelope(this.selfId, this.deps.launch.node, 'data', ++this.seq, resetBody())))
    }
    this.dropSession(reason)
    if (this.authed && this.peerOnline) void this.beginHandshake()
  }

  /** 丢掉当前传输（和还没完成的握手）。 */
  private dropSession(reason: string): void {
    this.hsGen++ // 让还在等 crypto 的握手作废
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = null
    this.pending?.hs.free()
    this.pending = null
    this.sender?.handshakeDone()
    const session = this.activeSession
    this.activeSession = null
    session?.destroy({ kind: 'transport', message: reason })
    this.sender?.discardTransport(this.seq + 1)
  }
}

function parseHello(payload: Uint8Array): PreviewHello {
  const value: unknown = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(payload))
  if (typeof value !== 'object' || value === null) throw new Error('bad hello')
  const v = value as Record<string, unknown>
  if (typeof v.port !== 'number' || typeof v.expires_at_ms !== 'number') throw new Error('bad hello')
  return { port: v.port, expires_at_ms: v.expires_at_ms }
}

function rejectKind(reason: PreviewRejectReason | string): UnavailableKind {
  switch (reason) {
    case 'expired':
      return 'expired'
    case 'port_not_allowed':
      return 'port_not_allowed'
    case 'disabled':
      return 'disabled'
    default:
      return 'rejected'
  }
}

function goawayKind(code: PreviewGoaway['code']): UnavailableKind {
  switch (code) {
    case 'expired':
      return 'expired'
    case 'closed':
      return 'closed'
    case 'revoked':
      return 'revoked'
    case 'disabled':
      return 'disabled'
    case 'port_not_allowed':
      return 'port_not_allowed'
    case 'shutdown':
      return 'shutdown'
  }
}
