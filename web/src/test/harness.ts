// 传输层测试的脚手架：假 WebSocket + 假 hub + 假 daemon（用假 crypto 做 responder）。
import type { ClientInfo, Control, EnrollFailReason, ErrorCode, NotifyBody } from '@/protocol'
import { AnyconClient, type ClientStorage, type HubSnapshot, type NodeSnapshot, type WebSocketLike } from '@/transport/client'
import type { FullCrypto, Handshake, Keypair, Transport } from '@/transport/crypto'
import { FakeEnrollDaemon } from './enrollDaemon'
import { decodeControl, decodeEnvelope, decodeFrame, decodeRpc, encodeControl, encodeEnrollFrame, encodeEnvelope, encodeFrames, encodeRpc, makeEnvelope, makeNotification, Reassembler, resetBody, type Envelope, type RpcMessage } from '@/transport/wire'
import type { IdentityRecord, NodeRecord } from '@/db'
import { createFakeCrypto } from './fakeCrypto'

export const DAEMON_ID = 'd'.repeat(32)
export const CLIENT_ID = 'c'.repeat(32)
export const TEMP_ID = 't'.repeat(32)

export class FakeSocket implements WebSocketLike {
  static instances: FakeSocket[] = []
  binaryType = 'blob'
  readyState = 0
  onopen: ((ev: unknown) => void) | null = null
  onmessage: ((ev: { data: unknown }) => void) | null = null
  onclose: ((ev: { code: number; reason?: string }) => void) | null = null
  onerror: ((ev: unknown) => void) | null = null
  sent: Envelope[] = []
  closedWith: number | null = null

  constructor(
    readonly url: string,
    private readonly hub: FakeHub,
  ) {
    FakeSocket.instances.push(this)
    queueMicrotask(() => {
      this.readyState = 1
      this.onopen?.({})
    })
  }
  send(data: Uint8Array): void {
    const env = decodeEnvelope(data)
    this.sent.push(env)
    this.hub.fromClient(this, env)
  }
  close(code = 1000): void {
    this.closedWith = code
    this.readyState = 3
    this.onclose?.({ code })
  }
  /** hub → 客户端 */
  deliver(env: Envelope): void {
    const bytes = encodeEnvelope(env)
    // 异步投递，和真实 WebSocket 一致（响应不会在 send() 返回之前到达）
    queueMicrotask(() => this.onmessage?.({ data: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) }))
  }
  control(msg: Control): void {
    this.deliver(makeEnvelope('', CLIENT_ID, 'control', 0, encodeControl(msg)))
  }
  serverClose(code: number): void {
    this.readyState = 3
    this.onclose?.({ code })
  }
}

/** 假 daemon 的 RPC 处理函数抛这个，就会回一条带稳定错误码的错误响应。 */
export class FakeRpcError extends Error {
  constructor(
    readonly code: ErrorCode,
    readonly field?: string,
  ) {
    super(code)
  }
}

export interface FakeDaemonOpts {
  /** 不回应握手（模拟 daemon 不认识我们的公钥）。 */
  ignoreHandshake?: boolean
  rpc?: Record<string, (params: unknown) => unknown>
}

/**
 * 配对完成后 daemon 发 `pair.ack` 的方式：
 * - good：正确的 ack；none：不发（hub 伪造 PairDone 的情形）；forged：密文被篡改（解不开）；
 * - wrong_peer / wrong_node / wrong_fingerprint：能解开但内容和 PairDone / 握手对不上；not_ack：解得开但不是 pair.ack。
 */
export type AckMode = 'good' | 'none' | 'forged' | 'wrong_peer' | 'wrong_node' | 'wrong_fingerprint' | 'not_ack'

export class FakeHub {
  daemonOnline = true
  daemonSeq = 0
  authFrames: Extract<Control, { type: 'auth' }>[] = []
  /** 下一次 auth 之后给的正式身份；有 token 时 device_id=CLIENT_ID。 */
  autoAuth = true
  authFail: 'invalid' | 'revoked' | 'version' | 'quota' | 'protocol' | null = null
  // 配对脚本
  /** daemon 本地生成的短码（hub 永远看不到它，这里只是脚本）。 */
  pairCodeOk: string | null = null
  /** 假装是不知道短码的 hub 在冒充 daemon：握手用一个错的 PSK。 */
  impersonate = false
  ackMode: AckMode = 'good'
  /** 下一个 data 信封被 hub 丢弃并回 dropped{reason}（不是 backpressure：Dropped 不是让你重发）。 */
  dropNextWith: 'too_large' | 'not_paired' | 'rate_limited' | null = null
  /** 假 hub 的信封上限（和真 hub 的 ANYCON_MAX_ENVELOPE_BYTES 一致）；超了回 dropped{too_large}。 */
  maxEnvelopeBytes = 262_144
  oversize = 0
  /** daemon 收到的、重组并解密后的 RPC 请求（按到达顺序）。 */
  received: RpcMessage[] = []
  /** pair_join 里收到的 match_id（断言 hub 只看到 match_id）。 */
  joinMatchIds: string[] = []
  pairFailReason: 'not_found' | 'timeout' | null = null
  pairIssueToken = 'tok-new'
  /** PairDone 之后是否紧接着发 PeerOnline（新版 hub 会发）。 */
  peerOnlineAfterPair = true
  /** PairMatched 里给 joiner 的 peer：默认就是 daemon 正式 id；设成别的值模拟「daemon 还没有正式身份，用临时 id 配对」。 */
  matchedPeer = DAEMON_ID
  /** 配对握手消息 2 的延迟（毫秒）；0 = 马上。拉长「撮合之后、握手完成之前」这一段，看这期间有没有别的 handshake 帧混进来。 */
  pairReplyDelayMs = 0
  /** 配对握手里读不懂的 handshake 帧数（真 daemon 遇到就作废这次配对，这里也一样）。 */
  pairHandshakeFailures = 0
  private daemonTransport: Transport | null = null
  private daemonHs: Handshake | null = null
  private pairHs: Handshake | null = null
  private pairTransport: Transport | null = null
  private daemonReassembler = new Reassembler()
  private daemonMsgId = 0
  /** 对 data 信封回 backpressure 的次数。 */
  backpressureOnce = 0
  dataFrames: Envelope[] = []
  /** 被 dropNextWith 丢掉的信封。 */
  droppedEnvelopes: Envelope[] = []
  /** 被 backpressure 丢掉的信封（断言重发的是同一批字节）。 */
  backpressured: Envelope[] = []
  /**
   * 真 hub 的粘住规则（docs/protocol.md § 3.1）：`capacity` = 目标连接的发送缓冲能放几封 data 信封（超了退回 backpressure），
   * 某个 seq 被退回后，同一 (from, to) 的其它信封一律退回，直到它的重发被转发。`stickyRead()` 模拟目标读走了一些、腾出缓冲。
   * null = 不启用（只有 `backpressureOnce` 那种一次性退回）。
   */
  sticky: { capacity: number; rateLimitedMs?: number } | null = null
  stickyQueued = 0
  private stickyReturned: number[] = []
  /** 目标（daemon）实际收到的 data 信封 seq，按到达顺序——它看到的就是这个顺序。 */
  forwardedSeqs: number[] = []
  /** 所有到达 hub 的 data 信封 seq（含被退回的），按到达顺序。 */
  arrivedSeqs: number[] = []
  /** daemon 解密失败的次数（= 传输失步，真 daemon 会为此发 RESET）。 */
  decryptFailures = 0
  /**
   * 限速模拟（docs/protocol.md § 3.1）：前 `remaining` 个 IK `handshake` 信封被退回 `backpressure`（超速和缓冲满对握手也一样退回，不粘住），
   * `retryAfterMs` 非 0 时每次再跟一条 `rate_limited`。用完之后放行。
   */
  bounceHandshakes: { remaining: number; retryAfterMs?: number } | null = null
  /** 每个到达 hub 的 IK 握手信封（含被退回的）的到达时间（`Date.now()`）。 */
  handshakeArrivals: number[] = []
  /** 被退回的 IK 握手信封数。 */
  handshakeBounces = 0
  /** 按真 hub 的规则判定：一直被限速退回、期间没有任何信封被转发、相邻两次退回间隔 ≤ 1.5 s，满 10 s 就以 1008 断开。 */
  closed1008 = false
  private limitedSince = 0
  private lastLimitedAt = 0
  /** IK 握手回复的延迟（毫秒）；0 = 马上（微任务）。 */
  ikDelayMs = 0
  /** 收到的 IK 握手次数（断言失步后重新握手）。 */
  ikHandshakes = 0
  readonly crypto: FullCrypto
  readonly daemonKp: Keypair
  readonly daemonName = 'test-node'
  /** 配对握手消息 3 里 joiner 带来的 ClientInfo（daemon 靠它知道设备显示名）。 */
  joinerInfo: unknown = null

  // ---- 账号内接入（ADR-36）----
  /** 假 daemon 的接入引擎（`enableEnroll()` 创建）。没有它，`enroll_begin` 得到 `not_found`。 */
  enroll: FakeEnrollDaemon | null = null
  /** hub 直接拒绝 `enroll_begin` 的原因（forbidden / quota / rate_limited / offline / busy…）。 */
  enrollBeginFail: EnrollFailReason | null = null
  /** hub 对 `enroll_begin` 什么都不回（限频的回执被「每秒一条」的预算吞掉的样子）。 */
  enrollBeginSilent = false
  /** 收到的 `enroll_begin` 的 node。 */
  enrollBegins: string[] = []
  /** 收到的 `enroll_cancel` 的 enroll_id。 */
  enrollCancels: string[] = []
  /** 当前（进行中）的接入会话 id；没有为 null。 */
  enrollId: string | null = null
  private enrollCount = 0
  private enrollNConfirmed = false
  private enrollDConfirmed = false
  private enrollSock: FakeSocket | null = null

  /** `crypto` 默认是内存假货；wasm 冒烟测试传真实实现。 */
  constructor(
    public opts: FakeDaemonOpts = {},
    crypto: FullCrypto = createFakeCrypto(),
  ) {
    this.crypto = crypto
    this.daemonKp = crypto.generateKeypair()
  }

  factory = (url: string): WebSocketLike => new FakeSocket(url, this)

  get socket(): FakeSocket {
    return FakeSocket.instances[FakeSocket.instances.length - 1]!
  }

  private reply(sock: FakeSocket, msg: Control): void {
    sock.control(msg)
  }

  fromClient(sock: FakeSocket, env: Envelope): void {
    if (env.kind === 'control') {
      const msg = decodeControl(env.body)
      switch (msg.type) {
        case 'auth': {
          this.authFrames.push(msg)
          if (this.authFail) return this.reply(sock, { type: 'auth_fail', reason: this.authFail })
          if (!this.autoAuth) return
          const authed = !!msg.device_token
          this.reply(sock, {
            type: 'auth_ok',
            device_id: authed ? CLIENT_ID : TEMP_ID,
            server_time_ms: Date.now(),
            peers_online: authed && this.daemonOnline ? [DAEMON_ID] : [],
          })
          return
        }
        case 'pair_join': {
          this.joinMatchIds.push(msg.match_id)
          const code = this.pairCodeOk
          // daemon 那一侧：和 joiner 一样，一次派生同时得到 match_id 和 PSK
          const secrets = code ? this.crypto.derivePairSecrets(code) : null
          if (this.pairFailReason || !code || !secrets || msg.match_id !== secrets.matchId) return this.reply(sock, { type: 'pair_failed', reason: this.pairFailReason ?? 'not_found' })
          this.reply(sock, { type: 'pair_matched', peer: this.matchedPeer, role: 'joiner' })
          this.pairHs = this.crypto.pairing('responder', this.daemonKp, (this.impersonate ? this.crypto.derivePairSecrets('WXYZ6789') : secrets).psk)
          this.pairTransport = null
          return
        }
        case 'enroll_begin':
          return this.onEnrollBegin(sock, msg.node)
        case 'enroll_cancel': {
          this.enrollCancels.push(msg.enroll_id)
          if (this.enrollId === msg.enroll_id) return this.endEnroll()
          // 和真 hub 一样：没有这个会话（已结束 / 不是你的）→ 回执 `not_found`，让客户端不用再等
          return this.reply(sock, { type: 'enroll_failed', enroll_id: msg.enroll_id, reason: 'not_found' })
        }
        case 'pair_confirm': {
          if (this.enrollId) {
            this.enrollNConfirmed = true
            return this.maybeFinishEnroll()
          }
          this.reply(sock, { type: 'pair_done', peer: DAEMON_ID, device_id: CLIENT_ID, device_token: this.pairIssueToken })
          if (this.peerOnlineAfterPair) this.reply(sock, { type: 'peer_online', device: DAEMON_ID })
          this.sendPairAck(sock)
          return
        }
        default:
          return
      }
    }
    if (env.to !== DAEMON_ID && env.to !== this.matchedPeer) return
    if (env.kind === 'handshake') {
      if (this.enrollId && this.enroll) return this.enroll.onEnvelope(env)
      if (this.pairHs) return this.daemonPairStep(sock, env)
      this.handshakeArrivals.push(Date.now())
      const bounce = this.bounceHandshakes
      if (bounce && bounce.remaining > 0) {
        bounce.remaining -= 1
        this.handshakeBounces += 1
        this.noteRateLimited(sock)
        this.reply(sock, { type: 'backpressure', to: DAEMON_ID, seq: env.seq })
        if (bounce.retryAfterMs) this.reply(sock, { type: 'rate_limited', retry_after_ms: bounce.retryAfterMs })
        return
      }
      this.limitedSince = 0 // 有信封被转发：限速的这一段结束了
      if (this.opts.ignoreHandshake) return
      this.daemonHs = this.crypto.reconnect('responder', this.daemonKp)
      this.ikHandshakes += 1
      // 新的传输从头开始：粘住状态随之清掉（docs/spec/hub.md § 3.3：成功转发的 handshake 清掉粘住状态；否则被丢掉的那个 seq 永远等不到重发）
      this.stickyReturned = []
      this.stickyQueued = 0
      try {
        this.daemonHs.read(env.body)
        const out = this.daemonHs.write()
        this.daemonTransport = this.daemonHs.intoTransport()
        this.daemonReassembler.clear()
        const reply = makeEnvelope(DAEMON_ID, CLIENT_ID, 'handshake', ++this.daemonSeq, out)
        // 真实 daemon 的回复要隔一会儿才到；延迟让「回复还在路上时又发起了第二次握手」这种交错能被测出来
        if (this.ikDelayMs > 0) setTimeout(() => sock.deliver(reply), this.ikDelayMs)
        else sock.deliver(reply)
      } catch {
        /* 不认识：沉默 */
      }
      return
    }
    if (env.kind === 'data') {
      if (!this.daemonOnline) return this.reply(sock, { type: 'undeliverable', to: DAEMON_ID, seq: env.seq })
      if (this.sticky && !this.stickyAccept(sock, env)) return
      if (this.backpressureOnce > 0) {
        this.backpressureOnce -= 1
        this.backpressured.push(env)
        return this.reply(sock, { type: 'backpressure', to: DAEMON_ID, seq: env.seq })
      }
      if (this.dropNextWith) {
        const reason = this.dropNextWith
        this.dropNextWith = null
        this.droppedEnvelopes.push(env)
        return this.reply(sock, { type: 'dropped', seq: env.seq, reason })
      }
      if (encodeEnvelope(env).length > this.maxEnvelopeBytes) {
        this.oversize += 1
        return this.reply(sock, { type: 'dropped', seq: env.seq, reason: 'too_large' })
      }
      this.dataFrames.push(env)
      if (!this.daemonTransport) return
      const frame = decodeFrame(env.body)
      if (frame.kind === 'reset') {
        this.daemonTransport = null
        return
      }
      const ciphertext = this.daemonReassembler.push(frame)
      if (!ciphertext) return
      let plain: RpcMessage
      try {
        plain = decodeRpc(this.daemonTransport.decrypt(ciphertext))
      } catch (e) {
        // 乱序 / 丢片：真 daemon 会丢弃传输并发 RESET
        this.decryptFailures += 1
        throw e
      }
      this.received.push(plain)
      this.onDaemonRpc(sock, plain)
    }
  }

  /** 真 hub 的 1008 判定（`crates/hub/src/ws/conn.rs` 的 `note_limited`）。 */
  private noteRateLimited(sock: FakeSocket): void {
    const now = Date.now()
    if (this.limitedSince === 0 || now - this.lastLimitedAt > 1500) this.limitedSince = now
    this.lastLimitedAt = now
    if (now - this.limitedSince >= 10_000 && !this.closed1008) {
      this.closed1008 = true
      sock.serverClose(1008)
    }
  }

  /** 粘住规则。返回 true = 转发给 daemon，false = 已退回。 */
  private stickyAccept(sock: FakeSocket, env: Envelope): boolean {
    const cap = this.sticky!.capacity
    this.arrivedSeqs.push(env.seq)
    const bounce = (): false => {
      this.backpressured.push(env)
      this.reply(sock, { type: 'backpressure', to: DAEMON_ID, seq: env.seq })
      const ms = this.sticky?.rateLimitedMs
      if (ms) this.reply(sock, { type: 'rate_limited', retry_after_ms: ms })
      return false
    }
    const expected = this.stickyReturned[0]
    if (expected !== undefined) {
      if (env.seq !== expected) {
        if (!this.stickyReturned.includes(env.seq)) this.stickyReturned.push(env.seq)
        return bounce()
      }
      if (this.stickyQueued >= cap) return bounce()
      this.stickyReturned.shift()
    } else if (this.stickyQueued >= cap) {
      this.stickyReturned.push(env.seq)
      return bounce()
    }
    this.stickyQueued += 1
    this.forwardedSeqs.push(env.seq)
    return true
  }

  /** 目标连接读走了 n 封 data 信封，腾出发送缓冲。 */
  stickyRead(n = Infinity): void {
    this.stickyQueued = Math.max(0, this.stickyQueued - n)
  }

  /** 创建接入引擎（假 daemon）。`password` 等在返回值上设。 */
  enableEnroll(): FakeEnrollDaemon {
    this.enroll = new FakeEnrollDaemon({
      crypto: this.crypto,
      keypair: this.daemonKp,
      nodeId: DAEMON_ID,
      send: (session, frame) => session.sock.deliver(makeEnvelope(DAEMON_ID, session.peer, 'handshake', ++this.daemonSeq, encodeEnrollFrame(frame))),
      confirmed: (_session, transport, info: ClientInfo) => {
        this.pairTransport = transport
        this.joinerInfo = info
        this.enrollDConfirmed = true
        this.maybeFinishEnroll()
      },
    })
    return this.enroll
  }

  private onEnrollBegin(sock: FakeSocket, node: string): void {
    this.enrollBegins.push(node)
    if (this.enrollBeginSilent) return
    if (this.enrollBeginFail) return this.reply(sock, { type: 'enroll_failed', reason: this.enrollBeginFail })
    if (!this.enroll || node !== DAEMON_ID) return this.reply(sock, { type: 'enroll_failed', reason: 'not_found' })
    if (this.enrollId) return this.reply(sock, { type: 'enroll_failed', reason: 'busy' }) // 每条连接同时最多 1 个
    const id = (++this.enrollCount).toString(16).padStart(32, '0')
    this.enrollId = id
    this.enrollSock = sock
    this.enrollNConfirmed = false
    this.enrollDConfirmed = false
    const authed = !!this.authFrames.at(-1)?.device_token
    this.enroll.begin(id, authed ? CLIENT_ID : TEMP_ID, sock)
    this.reply(sock, { type: 'enroll_started', enroll_id: id, node: DAEMON_ID, expires_at_ms: Date.now() + 300_000 })
  }

  private endEnroll(): void {
    this.enroll?.end()
    this.enrollId = null
    this.enrollSock = null
  }

  /** 两边都 `pair_confirm` 了才 `pair_done`（和真 hub 一样）；发起方是临时连接时签发设备 id 和 token。 */
  private maybeFinishEnroll(): void {
    const sock = this.enrollSock
    if (!sock || !this.enrollNConfirmed || !this.enrollDConfirmed) return
    const authed = !!this.authFrames.at(-1)?.device_token
    this.reply(sock, authed ? { type: 'pair_done', peer: DAEMON_ID } : { type: 'pair_done', peer: DAEMON_ID, device_id: CLIENT_ID, device_token: this.pairIssueToken })
    if (this.peerOnlineAfterPair) this.reply(sock, { type: 'peer_online', device: DAEMON_ID })
    this.sendPairAck(sock)
    this.endEnroll()
  }

  /** hub 自己结束接入会话（超时 / 对端断线 / …），通知发起方。 */
  failEnroll(reason: EnrollFailReason): void {
    const sock = this.enrollSock
    const id = this.enrollId
    if (!sock || !id) return
    this.endEnroll()
    this.reply(sock, { type: 'enroll_failed', enroll_id: id, reason })
  }

  /** 配对结束后 daemon 经 PSK 绑定的传输发 `pair.ack`。 */
  private sendPairAck(sock: FakeSocket): void {
    const t = this.pairTransport
    if (!t || this.ackMode === 'none') return
    const ack = {
      node_id: this.ackMode === 'wrong_node' ? 'e'.repeat(32) : DAEMON_ID,
      peer_id: this.ackMode === 'wrong_peer' ? 'f'.repeat(32) : CLIENT_ID,
      name: this.daemonName,
      version: '9.9.9',
      fingerprint: this.ackMode === 'wrong_fingerprint' ? '00:00:00:00:00:00:00:00' : this.crypto.fingerprint(this.daemonKp.public),
    }
    const msg = this.ackMode === 'not_ack' ? makeNotification('session.changed', {}) : makeNotification('pair.ack', ack)
    const ct = t.encrypt(encodeRpc(msg))
    if (this.ackMode === 'forged') ct[ct.length - 1]! ^= 0xff
    for (const body of encodeFrames(ct, this.daemonMsgId++)) sock.deliver(makeEnvelope(DAEMON_ID, CLIENT_ID, 'data', ++this.daemonSeq, body))
  }

  private daemonPairStep(sock: FakeSocket, env: Envelope): void {
    const hs = this.pairHs!
    try {
      const got = hs.read(env.body)
      if (hs.isMyTurn()) {
        // 消息 2 的 payload：daemon 的自我介绍
        const hello = new TextEncoder().encode(JSON.stringify({ name: this.daemonName, version: '9.9.9' }))
        const reply = makeEnvelope(this.matchedPeer, TEMP_ID, 'handshake', ++this.daemonSeq, hs.write(hello))
        if (this.pairReplyDelayMs > 0) setTimeout(() => sock.deliver(reply), this.pairReplyDelayMs)
        else sock.deliver(reply)
      }
      if (hs.isFinished()) {
        if (got.length > 0) this.joinerInfo = JSON.parse(new TextDecoder().decode(got))
        this.pairTransport = hs.intoTransport()
        this.pairHs = null
      }
    } catch {
      this.pairHandshakeFailures += 1
      this.pairHs = null
    }
  }

  private onDaemonRpc(sock: FakeSocket, msg: RpcMessage): void {
    if (!('method' in msg) || !('id' in msg)) return
    const handler = this.opts.rpc?.[msg.method]
    if (!handler) return // 不回应：用于超时测试
    try {
      const result = handler(msg.params)
      this.daemonSend(sock, { jsonrpc: '2.0', id: msg.id, result })
    } catch (e) {
      if (!(e instanceof FakeRpcError)) throw e
      this.daemonSend(sock, {
        jsonrpc: '2.0',
        id: msg.id,
        error: { code: -32001, message: e.code, data: { code: e.code, field: e.field } },
      })
    }
  }

  /** daemon → 客户端：加密、分片（每片一个 data 信封）。 */
  daemonSend(sock: FakeSocket, msg: RpcMessage): void {
    const ct = this.daemonTransport!.encrypt(encodeRpc(msg))
    for (const body of encodeFrames(ct, this.daemonMsgId++)) sock.deliver(makeEnvelope(DAEMON_ID, CLIENT_ID, 'data', ++this.daemonSeq, body))
  }

  /** daemon → 客户端：RESET 帧（daemon 发现传输失步）。 */
  daemonSendReset(sock: FakeSocket = this.socket): void {
    sock.deliver(makeEnvelope(DAEMON_ID, CLIENT_ID, 'data', ++this.daemonSeq, resetBody()))
  }

  /** daemon 主动推通知。 */
  notify(method: string, params: unknown): void {
    this.daemonSend(this.socket, makeNotification(method, params))
  }
}

export function memoryStorage(initial: { identity?: IdentityRecord; nodes?: NodeRecord[] } = {}): ClientStorage & {
  identity?: IdentityRecord
  nodes: Map<string, NodeRecord>
  wiped: number
} {
  const s = {
    identity: initial.identity,
    nodes: new Map((initial.nodes ?? []).map((n) => [n.node_id, n])),
    wiped: 0,
    async loadIdentity() {
      return s.identity
    },
    async saveIdentity(rec: IdentityRecord) {
      s.identity = rec
    },
    async loadNodes() {
      return [...s.nodes.values()]
    },
    async saveNode(rec: NodeRecord) {
      s.nodes.set(rec.node_id, rec)
    },
    async deleteNode(id: string) {
      s.nodes.delete(id)
    },
    async wipe() {
      s.identity = undefined
      s.nodes.clear()
      s.wiped += 1
    },
  }
  return s
}

/** 一个接好假 hub / 假存储的 AnyconClient，记录它推给 UI 层的全部状态。 */
export function setupClient(opts: { paired?: boolean; hub?: FakeHub; knowsNode?: boolean } = {}) {
  FakeSocket.instances = []
  const hub = opts.hub ?? new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
  const kp = hub.crypto.generateKeypair()
  const identity: IdentityRecord = {
    id: 'self',
    static_keypair: { public: kp.public, secret: kp.secret },
    device_name: 'Chrome · macOS · localhost',
    account_id: 'acct1',
    ...(opts.paired === false ? {} : { device_id: CLIENT_ID, device_token: 'tok' }),
  }
  const nodes: NodeRecord[] =
    opts.knowsNode === false ? [] : [{ node_id: DAEMON_ID, name: 'n', public_key: hub.daemonKp.public, paired_at: 1, last_seen: 1 }]
  const storage = memoryStorage({ identity, nodes })
  const client = new AnyconClient({
    crypto: async () => hub.crypto,
    wsFactory: hub.factory,
    wsUrl: () => 'ws://localhost/v1/ws',
    storage,
    origin: () => 'http://localhost',
    rng: () => 0.5,
    yieldToPaint: async () => {}, // 测试里不等下一帧（有些测试用假定时器）
  })
  const hubStates: HubSnapshot[] = []
  const nodeStates: NodeSnapshot[] = []
  const ready: string[] = []
  const notes: Array<[string, string, unknown]> = []
  const drops: Array<[string, string | null]> = []
  const notifies: Array<[string, NotifyBody]> = []
  client.setSink({
    hub: (s) => hubStates.push(s),
    node: (s) => nodeStates.push(s),
    nodeReady: (id) => ready.push(id),
    notification: (id, m, p) => notes.push([id, m, p]),
    notify: (id, body) => notifies.push([id, body]),
    dropped: (reason, nodeId) => drops.push([reason, nodeId]),
  })
  return { hub, client, storage, hubStates, nodeStates, ready, notes, drops, notifies }
}
