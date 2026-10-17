/**
 * 到一个节点（daemon）的端到端通道：IK 握手（initiator）、Noise Transport、JSON-RPC。
 *
 * 状态机（对应 docs/spec/web.md § 6.4）：
 *   hub_disconnected → peer_offline → handshaking → ready
 *                                         └→ noise_failed（daemon 不认识我们的公钥：被吊销或 daemon 重装）
 * 每次 `peer_online`（或 `auth_ok.peers_online` 含该节点）都重新握手；`peer_offline` / hub 断线丢弃 Transport。
 *
 * 一条 JSON-RPC 消息 = 一次 `Transport.encrypt` = 一个 `msg_id`；密文按 `frame`（ADR-30）切成一个 `WHOLE` 或
 * 若干 `FRAGMENT`，每片一个 `data` 信封（hub 对单个信封有上限，默认 256 KiB）。Noise 的 nonce 是顺序的，
 * 所以任何一片丢失或乱序都意味着传输失步。hub 对已配对的 `(from, to)` 只会「转发」或「退回 `backpressure`」，
 * 从不静默丢弃，并且「粘住」：一个 `seq` 被退回后，后续信封一律退回，直到被退回的那些 `seq` 按序重发被转发
 * （docs/protocol.md § 3.1）。发送方要配合：
 * - 保留最近发出的信封**原文**（按字节封顶，`RESEND_BUFFER_BYTES`）。凡是可能还在路上、hub 还没来得及回执的
 *   信封都不能丢：发出不满 `RESEND_RETAIN_MS`（10 s）的、以及被退回等着重发的，永不逐出。保留量超过上限时
 *   **暂停发新的**（把上限当作在途窗口），等最老的那批过了 10 s 再继续，而不是逐出；
 * - 收到 `backpressure { to, seq }`：暂停向该节点发新东西，攒齐被退回的 `seq`，等 `retry_after_ms`（最近一条
 *   `rate_limited` 给的，封顶 30 s；没有就 100–300 ms + 抖动，连续被退回时翻倍，封顶 ×8，最长约 2.4 s），
 *   然后把所有被退回的 `seq` **按升序原样重发**（不重新加密），再发排队的新东西。重发期间又被退回就再来一轮；
 * - 被退回的 `seq` 不在保留缓冲里（hub 报了我们没发过的 `seq`）→ 无法保证顺序，丢弃 Transport、重新 IK 握手；
 * - `handshake` 信封被退回（目标缓冲满或超速）：这一封节点没收到。按 `retry_after_ms` 延后再重新发起握手
 *   （没有就 300–1000 ms + 抖动，连续被退回时翻倍，封顶 30 s），不立即重发、也不重置，也不占用握手的重试次数；
 * - `rate_limited { retry_after_ms }` 是**整条 hub 连接**（所有节点、含重发）暂停多久的提示，不对应某一封信
 *   （伴随 `backpressure` 出现，不再意味着有信封被丢）：记在 `HubConnection.rateUntil`，各节点共用；到点之前新的
 *   信封排队，不失败请求、不重新握手；
 * - `dropped { seq, reason }` 每连接每秒至多一条，是「发错东西」的信号（`too_large` = 分片后仍超限，`not_paired` = 配对
 *   关系不对），**不据此重发**：作为 `error` 事件报出，进行中的请求失败，并重新握手；
 * - 接收方解密失败 / 分片乱序 / 收到 `RESET` 时，丢弃 Transport、直接重新 IK 握手。
 */
import {
  FrameError,
  MAX_MESSAGE,
  Reassembler,
  RpcPeer,
  bytesOfHex,
  decodeFrame,
  encodeFrames,
  type Control,
  type Crypto,
  type DropReason,
  type Envelope,
  type Handshake,
  type Keypair,
  type Transport,
} from '@anycon/protocol'

import { Emitter } from './emitter.js'
import { AnyconError, ConnectionError, NodeOffline, NodeRejected } from './errors.js'
import { RATE_WAIT_MAX_MS, type HubConnection } from './hub.js'
import type { StoredNode } from './storage.js'

export type LinkState =
  | 'hub_disconnected'
  | 'peer_offline'
  | 'handshaking'
  | 'ready'
  | 'noise_failed'
  | 'closed'

export interface LinkEvents {
  state: [state: LinkState, previous: LinkState]
  /** 握手完成（含每次重连后的重新握手）。 */
  ready: []
  /** 从 ready 掉线（peer_offline / hub 断线 / 重新握手开始）。 */
  offline: []
  /**
   * 非致命的协议问题：hub 丢弃了我们的信封（`dropped`：`too_large` / `not_paired`）。进行中的请求已经失败，
   * 通道会重新握手；这里让调用方有机会记录。限速（`rate_limited`）和背压（`backpressure`）不走这里：
   * hub 不丢信封，SDK 暂停、等、按序重发，请求照常完成。
   */
  error: [error: Error]
}

export interface LinkOptions {
  requestTimeoutMs?: number
  /** 等握手回复的时间，毫秒。默认 5000。 */
  handshakeTimeoutMs?: number
  /** 握手回复超时后的重试次数（含首次）。默认 3。 */
  handshakeAttempts?: number
  /**
   * 保留最近发出的信封原文的字节上限（被退回时按原样重发）。默认 16 MiB；协议要求不低于 8 MiB。
   * 超过上限时**暂停发送新的信封**（当作在途窗口），直到最老的一批过了 `resendRetainMs`，而不是逐出。
   * 调小只会让大消息发得更慢。主要给测试用。
   */
  resendBufferBytes?: number
  /**
   * 发出不满这么久的信封永不逐出（它们可能还在路上，hub 的退回回执可能还没到），毫秒。默认 10000。
   * 主要给测试用。
   */
  resendRetainMs?: number
}

/** 默认的重发缓冲：16 MiB（= `MAX_MESSAGE`，一整条最大的分片消息）。协议下限 8 MiB。 */
export const RESEND_BUFFER_BYTES = MAX_MESSAGE
/** 发出不满这么久的信封永不逐出：它们可能还在路上，hub 的退回回执可能还没到（docs/protocol.md § 3.1）。 */
export const RESEND_RETAIN_MS = 10_000
/** 被退回之后重发前默认等多久：100–300 ms 随机（抖动），连续被退回时翻倍，最多 ×8（约 2.4 s）。 */
const RESEND_DELAY_MIN_MS = 100
const RESEND_DELAY_SPREAD_MS = 200
const RESEND_BACKOFF_MAX = 8
/** 距上一轮重发不到这么久又被退回，算「连续被退回」。 */
const RESEND_BACKOFF_WINDOW_MS = 5000
/** `handshake` 被退回、又没有 `rate_limited` 给时间时，重新握手前等多久：300–1000 ms（含抖动），连续被退回时翻倍。 */
const HS_RETRY_MIN_MS = 300
const HS_RETRY_SPREAD_MS = 700
/** 握手重试等待的上限（与 `rate_limited` 的封顶一致）。 */
const HS_RETRY_MAX_MS = RATE_WAIT_MAX_MS

export class NodeLink extends Emitter<LinkEvents> {
  readonly rpc: RpcPeer
  readonly node: StoredNode
  readonly #hub: HubConnection
  readonly #crypto: Crypto
  readonly #keypair: Keypair
  readonly #nodePublic: Uint8Array
  readonly #hsTimeoutMs: number
  readonly #hsAttempts: number
  #state: LinkState = 'hub_disconnected'
  #peerOnline = false
  #transport: Transport | undefined
  #handshake: Handshake | undefined
  #hsTimer: ReturnType<typeof setTimeout> | undefined
  #hsTry = 0
  /** 当前这一次握手信封的 seq（被退回时据此认出来）；没有在握手时是 0。 */
  #hsSeq = 0
  /** 握手被 hub 退回后，重新握手的定时器。 */
  #hsRetryTimer: ReturnType<typeof setTimeout> | undefined
  /** 重新握手最早在什么时候做（绝对时间，毫秒）；`rate_limited` 更晚就等它。 */
  #hsRetryAt = 0
  /** 连续被退回的握手次数（退避用，握手有回复或重新开始时清零）。 */
  #hsBounces = 0
  /**
   * 最近发出的 data 信封的原文（seq → 整帧 + 发出时间，按发送顺序），被退回时按原样重发。
   * 按字节封顶，但**不逐出发出不满 `#retainMs` 的**：超过上限就暂停发新的（`#flushQueued`）。
   */
  readonly #sent = new Map<number, { frame: Uint8Array; at: number }>()
  #sentBytes = 0
  readonly #sentCap: number
  readonly #retainMs: number
  /** hub 退回、还没重发的 seq（`backpressure`）。 */
  readonly #rejected = new Set<number>()
  /** 暂停向该节点发新信封：有被退回的 seq 在等重发，`rate_limited` 的等待还没到点，或在途窗口（保留缓冲）满了。 */
  #paused = false
  /** 这一轮重发最早在什么时候做（绝对时间，毫秒）。 */
  #resendAt = 0
  /** 在途窗口满了的话，最早什么时候有信封过了保留期、腾得出位置（绝对时间，毫秒）。 */
  #windowUntil = 0
  /** 唤醒定时器：到点重发被退回的 seq、再发排队的新信封。 */
  #wakeTimer: ReturnType<typeof setTimeout> | undefined
  /** 连续被退回时的退避倍数（1, 2, 4, 8）。 */
  #backoff = 1
  #lastRoundAt = 0
  /** 这个传输会话开始前（握手期间）发出的 seq 都已过期：迟到的 `backpressure` 不理。 */
  #staleBelow = 0
  /** 暂停期间排队的 data body（已加密、已成帧），恢复后按序发。 */
  #queued: Uint8Array[] = []
  /** 下一条消息的 `msg_id`（u32，本地递增）。 */
  #msgId = 0
  #reassembler = new Reassembler()
  #failure: Error | undefined

  constructor(
    hub: HubConnection,
    crypto: Crypto,
    keypair: Keypair,
    node: StoredNode,
    options: LinkOptions = {},
  ) {
    super()
    this.#hub = hub
    this.#crypto = crypto
    this.#keypair = keypair
    this.node = node
    this.#nodePublic = bytesOfHex(node.publicKey)
    this.#sentCap = options.resendBufferBytes ?? RESEND_BUFFER_BYTES
    this.#retainMs = options.resendRetainMs ?? RESEND_RETAIN_MS
    this.#hsTimeoutMs = options.handshakeTimeoutMs ?? 5000
    this.#hsAttempts = options.handshakeAttempts ?? 3
    this.rpc = new RpcPeer({
      send: (bytes) => this.#sendData(bytes),
      ...(options.requestTimeoutMs !== undefined ? { timeoutMs: options.requestTimeoutMs } : {}),
    })
  }

  get state(): LinkState {
    return this.#state
  }

  get ready(): boolean {
    return this.#state === 'ready'
  }

  /** 握手失败的原因（`noise_failed` 时）。 */
  get failure(): Error | undefined {
    return this.#failure
  }

  // ---------- 来自 Client 的事件 ----------

  /** hub 认证成功（含重连后）。`peersOnline` 是此刻在线的已配对设备。 */
  onHubAuthed(peersOnline: readonly string[]): void {
    if (this.#state === 'closed') return
    this.#peerOnline = peersOnline.includes(this.node.id)
    // 节点在线就直接进入 handshaking，不经过 peer_offline（否则等待方会把中间态当成「已落定」）。
    if (this.#peerOnline) this.#startHandshake()
    else this.#setState('peer_offline')
  }

  onHubDown(): void {
    if (this.#state === 'closed') return
    this.#peerOnline = false
    this.#dropSession(
      new NodeOffline(this.node.name ?? this.node.id, 'the hub connection was lost'),
    )
    this.#setState('hub_disconnected')
  }

  onControl(ctrl: Control): void {
    switch (ctrl.type) {
      case 'peer_online':
        if (ctrl.device !== this.node.id) return
        this.#peerOnline = true
        // 每次 online 都重新握手：对端可能重启过，旧 Transport 的计数器已经对不上。
        this.#dropSession(new NodeOffline(this.node.name ?? this.node.id, 'the node reconnected'))
        this.#startHandshake()
        return
      case 'peer_offline':
        if (ctrl.device !== this.node.id) return
        this.#peerOnline = false
        this.#dropSession(new NodeOffline(this.node.name ?? this.node.id))
        this.#setState('peer_offline')
        return
      case 'undeliverable':
        if (ctrl.to !== this.node.id) return
        // 对端已无活跃连接，那一封（以及之后的）都没送到。peer_offline 通常随后到达。
        this.rpc.failAll(
          new NodeOffline(this.node.name ?? this.node.id, 'the node is not reachable'),
        )
        return
      case 'backpressure':
        if (ctrl.to !== this.node.id) return
        this.#onBackpressure(ctrl.seq)
        return
      case 'dropped':
        this.#onDropped(ctrl.seq, ctrl.reason)
        return
      case 'rate_limited':
        // 限速是整条 hub 连接的，不分节点；只是「暂停多久」的提示。时间点已经由 HubConnection 记在 `rateUntil` 上。
        this.#onRateLimited()
        return
      default:
        return
    }
  }

  onEnvelope(env: Envelope): void {
    if (env.from !== this.node.id) return
    if (env.kind === 'handshake') this.#onHandshake(env.body)
    else if (env.kind === 'data') this.#onData(env.body)
  }

  /** 释放资源，之后所有请求立刻失败。 */
  close(): void {
    this.#dropSession(new NodeOffline(this.node.name ?? this.node.id, 'the client was closed'))
    this.#setState('closed')
  }

  // ---------- 握手 ----------

  #startHandshake(): void {
    this.#clearHsTimer()
    this.#freeHandshake()
    this.#failure = undefined
    this.#hsTry = 0
    this.#hsBounces = 0
    this.#attemptHandshake()
  }

  #attemptHandshake(): void {
    this.#hsTry += 1
    this.#freeHandshake()
    try {
      const hs = this.#crypto.reconnect('initiator', this.#keypair, this.#nodePublic)
      this.#handshake = hs
      this.#setState('handshaking')
      this.#hsSeq = this.#hub.sendEnvelope(this.node.id, 'handshake', hs.write())
    } catch (cause) {
      this.#freeHandshake()
      if (cause instanceof ConnectionError) {
        // hub 连接不可用：等 onHubAuthed 再来，这不是节点的问题。
        this.#setState('hub_disconnected')
        return
      }
      this.#fail(new NodeRejected(`cannot start the Noise handshake: ${errorMessage(cause)}`))
      return
    }
    this.#hsTimer = setTimeout(() => {
      this.#hsTimer = undefined
      if (this.#state !== 'handshaking') return
      if (this.#hsTry < this.#hsAttempts) this.#attemptHandshake()
      else {
        this.#fail(
          new NodeRejected(
            'the node did not answer the Noise handshake: this device may have been revoked or the node reinstalled; re-pair it',
          ),
        )
      }
    }, this.#hsTimeoutMs)
  }

  #onHandshake(body: Uint8Array): void {
    const hs = this.#handshake
    if (this.#state !== 'handshaking' || !hs) return
    try {
      hs.read(body)
      if (!hs.isFinished()) {
        throw new Error('handshake not finished after the reply')
      }
      const transport = hs.intoTransport()
      this.#handshake = undefined
      this.#clearHsTimer()
      if (!sameBytes(transport.remotePublic, this.#nodePublic)) {
        transport.free()
        throw new Error('the node presented a different static key than the one paired')
      }
      this.#transport = transport
      this.#reassembler.clear()
      this.#hsSeq = 0
      this.#hsBounces = 0
      // 握手期间（含更早）发出的 seq 到此为止；之后的 data 信封 seq 都更大。
      this.#staleBelow = this.#hub.lastSeq
    } catch (cause) {
      this.#clearHsTimer()
      // 可能是上一轮握手迟到的回复被新握手读到了：还有次数就重来，用完才判定被拒。
      if (this.#hsTry < this.#hsAttempts) {
        this.#attemptHandshake()
        return
      }
      this.#fail(new NodeRejected(`Noise handshake with the node failed: ${errorMessage(cause)}`))
      return
    }
    this.#setState('ready')
    this.emit('ready')
  }

  /**
   * hub 退回了我们的 `handshake`（目标缓冲满或这条连接超速）：这一封节点没收到，等回复没有意义。
   * 按 `retry_after_ms` 延后再重新握手；没有 `rate_limited` 给时间就 300–1000 ms + 抖动，连续被退回时翻倍，封顶 30 s。
   * 不立即重发（那只会再被退回，形成「握手 → 退回 → 再握手」的循环），也不重置、不占用握手的重试次数。
   */
  #onHandshakeBounced(): void {
    this.#clearHsTimer()
    this.#freeHandshake()
    this.#hsTry = Math.max(0, this.#hsTry - 1) // 没送到节点，不算一次失败的尝试
    const now = Date.now()
    const hinted = this.#hub.rateUntil > now
    const backoff = Math.min(
      HS_RETRY_MAX_MS,
      (HS_RETRY_MIN_MS + jitter(HS_RETRY_SPREAD_MS)) * 2 ** Math.min(this.#hsBounces, 16),
    )
    this.#hsBounces += 1
    this.#hsRetryAt = now + (hinted ? this.#hub.rateUntil - now : backoff)
    this.#armHandshakeRetry()
  }

  /** （重）装重新握手的定时器：到 `#hsRetryAt`，且不早于 `rate_limited` 给的时间点。 */
  #armHandshakeRetry(): void {
    if (this.#hsRetryTimer !== undefined) clearTimeout(this.#hsRetryTimer)
    const due = Math.max(this.#hsRetryAt, this.#hub.rateUntil)
    this.#hsRetryTimer = setTimeout(
      () => {
        this.#hsRetryTimer = undefined
        if (this.#state !== 'handshaking') return
        if (Math.max(this.#hsRetryAt, this.#hub.rateUntil) - Date.now() > 2) {
          this.#armHandshakeRetry() // 等待期间又收到了更晚的 rate_limited
          return
        }
        this.#attemptHandshake()
      },
      Math.max(0, due - Date.now()),
    )
  }

  #fail(error: Error): void {
    this.#freeHandshake()
    this.#failure = error
    this.rpc.failAll(error)
    this.#setState('noise_failed')
  }

  // ---------- 数据 ----------

  #onData(body: Uint8Array): void {
    const transport = this.#transport
    if (this.#state !== 'ready' || !transport) return
    let ciphertext: Uint8Array | undefined
    try {
      const frame = decodeFrame(body)
      if (frame.kind === 'reset') {
        // 对端发现传输失步，请求我们重新握手。
        this.#resync('the node asked for a new handshake')
        return
      }
      ciphertext = this.#reassembler.push(frame)
    } catch (err) {
      // 分片乱序 / 超限 / 格式坏：传输已经失步。
      this.#resync(err instanceof FrameError ? `bad data frame (${err.code})` : 'bad data frame')
      return
    }
    if (!ciphertext) return // 还在等后续分片
    let plain: Uint8Array
    try {
      plain = transport.decrypt(ciphertext)
    } catch {
      // 计数器对不上或被篡改：丢弃 Transport，重新握手最稳妥（docs/protocol.md § 3.1、crates/crypto/README.md）。
      this.#resync('decryption failed')
      return
    }
    try {
      this.rpc.handle(plain)
    } catch {
      // 对端发来不合法的 JSON-RPC：忽略这一条。
    }
  }

  /** 传输失步：丢掉 Transport（进行中的请求以 NodeOffline 失败），节点还在线就重新 IK 握手。 */
  #resync(why: string): void {
    this.#dropSession(new NodeOffline(this.node.name ?? this.node.id, `${why}; re-handshaking`))
    if (this.#peerOnline) this.#startHandshake()
  }

  #sendData(bytes: Uint8Array): void {
    if (this.#state === 'noise_failed') throw this.#failure ?? new NodeRejected()
    const transport = this.#transport
    if (this.#state !== 'ready' || !transport) {
      throw new NodeOffline(this.node.name ?? this.node.id)
    }
    const ciphertext = transport.encrypt(bytes)
    const msgId = this.#msgId
    this.#msgId = (this.#msgId + 1) >>> 0
    // 同一条消息的各片连续发出，不与别的消息交错（接收方的重组器只跟踪一条）。
    this.#queued.push(...encodeFrames(ciphertext, msgId))
    if (this.#paused) return // 暂停中（等重发 / 等 rate_limited 到点 / 在途窗口满）：恢复后按序发
    if (Date.now() < this.#hub.rateUntil) {
      // 之前的 rate_limited 还没到点（节点刚连上、或唤醒定时器还没装）：也走排队，到点再发。
      this.#paused = true
      this.#arm()
      return
    }
    this.#flushQueued()
  }

  #emitData(body: Uint8Array): void {
    const sent = this.#hub.sendEncoded(this.node.id, 'data', body)
    this.#sent.set(sent.seq, { frame: sent.frame, at: Date.now() })
    this.#sentBytes += sent.frame.length
  }

  /**
   * 在途窗口（保留缓冲）满了吗？满了先试着腾位置：只逐出发出已满 `#retainMs`、且不在待重发集合里的信封，
   * 从最老的开始，腾到上限以下为止。凡是更新的一律不动（它们可能还在路上，被退回时要原样重发）。
   * 腾不出来就是满，调用方暂停发新的；`#windowUntil` 记下最早什么时候有信封过期。
   */
  #windowFull(now: number): boolean {
    if (this.#sentBytes < this.#sentCap) return false
    for (const [seq, entry] of this.#sent) {
      if (this.#sentBytes < this.#sentCap || now - entry.at < this.#retainMs) break
      if (this.#rejected.has(seq)) continue // 等着重发的不能丢
      this.#sentBytes -= entry.frame.length
      this.#sent.delete(seq)
    }
    if (this.#sentBytes < this.#sentCap) return false
    let free = now + 50 // 全是等着重发的：重发之后它们才可能过期，稍后再看
    for (const [seq, entry] of this.#sent) {
      if (this.#rejected.has(seq)) continue
      free = entry.at + this.#retainMs
      break
    }
    this.#windowUntil = Math.max(free, now + 1)
    return true
  }

  /**
   * hub 退回了我们的一封 `data`（目标发送缓冲满，或我们超过速率预算），并从此刻起退回同一 `(from, to)` 的所有
   * 后续信封，直到被退回的这些重发成功。这里攒齐被退回的 `seq`（hub 会为每一封各回一条），暂停发新东西，
   * 过一会儿按升序原样重发。重复的一片对端解不开，所以每个 `seq` 一轮只发一次。
   * 被退回的若是我们的 `handshake`，走 `#onHandshakeBounced`：延后再握手。
   */
  #onBackpressure(seq: number): void {
    if (this.#state === 'handshaking' && seq !== 0 && seq === this.#hsSeq) {
      this.#onHandshakeBounced()
      return
    }
    if (this.#state !== 'ready' || !this.#transport) return // 没有传输，也没什么可重发
    if (seq <= this.#staleBelow) return // 上一个会话 / 握手期间的尾巴
    if (!this.#sent.has(seq)) {
      // hub 说了一封我们没留的信封（我们没发过，或不属于这个会话）：无法再保证顺序，丢弃传输重新握手。
      this.#resync('a rejected envelope is no longer buffered for resending')
      return
    }
    const firstOfRound = this.#rejected.size === 0
    this.#rejected.add(seq)
    if (!firstOfRound) return // 并入这一轮，重发时间已经定了
    const now = Date.now()
    const rateUntil = this.#hub.rateUntil
    this.#resendAt = now + (rateUntil > now ? rateUntil - now + jitter(50) : this.#nextDelay(now))
    this.#paused = true
    this.#arm()
  }

  /** 没有 `rate_limited` 给的时间时的重发等待：100–300 ms + 抖动；连续被退回就翻倍，安静 5 秒后重来。 */
  #nextDelay(now: number): number {
    this.#backoff =
      this.#lastRoundAt > 0 && now - this.#lastRoundAt < RESEND_BACKOFF_WINDOW_MS
        ? Math.min(this.#backoff * 2, RESEND_BACKOFF_MAX)
        : 1
    return (RESEND_DELAY_MIN_MS + jitter(RESEND_DELAY_SPREAD_MS)) * this.#backoff
  }

  /**
   * hub 对这条连接限速：只是「暂停多久」的提示（`retry_after_ms`），通常伴随 `backpressure`；时间点由
   * `HubConnection` 记在 `rateUntil` 上（整条连接共享，所有节点同时暂停）。
   * 暂停到点之前新的信封排队；被退回的 seq 也等到这个时间点之后再重发；握手被退回后等待重握手的，也顺延。
   * 不会失败进行中的请求，也不重新握手——hub 不再因为限速丢信封，退回的信封都会按序重发。
   */
  #onRateLimited(): void {
    if (this.#state === 'closed') return
    if (this.#hsRetryTimer !== undefined) this.#armHandshakeRetry()
    if (this.#state !== 'ready' || !this.#transport) return // 之后发的时候再按 `rateUntil` 排队
    this.#paused = true
    this.#arm()
  }

  /** 下一次该醒的时间点：有被退回的 seq 就是重发时间，否则是窗口腾出位置的时间；都不早于 `rate_limited` 给的。 */
  #nextWake(): number {
    const rate = this.#hub.rateUntil
    return this.#rejected.size > 0
      ? Math.max(rate, this.#resendAt)
      : Math.max(rate, this.#windowUntil)
  }

  /** （重）装唤醒定时器。 */
  #arm(): void {
    if (this.#wakeTimer !== undefined) clearTimeout(this.#wakeTimer)
    this.#wakeTimer = setTimeout(() => this.#wake(), Math.max(0, this.#nextWake() - Date.now()))
  }

  #wake(): void {
    this.#wakeTimer = undefined
    if (this.#state !== 'ready' || !this.#transport) {
      this.#paused = false
      return
    }
    if (this.#nextWake() - Date.now() > 2) {
      this.#arm() // 等待期间又收到了更晚的 rate_limited
      return
    }
    try {
      if (this.#rejected.size > 0 && !this.#resendRejected()) return
      this.#flushQueued()
    } catch {
      // hub 断线：state 会变，由 onHubDown 清理。
      this.#paused = false
    }
  }

  /** 把被退回的 seq 按升序原样重发。缓冲里找不到其中任何一个 → 重置传输，返回 false。 */
  #resendRejected(): boolean {
    const seqs = [...this.#rejected].toSorted((a, b) => a - b)
    this.#rejected.clear()
    const entries: Array<{ frame: Uint8Array; at: number }> = []
    for (const seq of seqs) {
      const entry = this.#sent.get(seq)
      if (!entry) {
        this.#resync('a rejected envelope is no longer buffered for resending')
        return false
      }
      entries.push(entry)
    }
    const now = Date.now()
    this.#lastRoundAt = now
    // 原样重发（seq 不变）：hub 靠 seq 判断这是不是它在等的那一封。重发之后它们又在路上了，保留期重新算。
    for (const entry of entries) {
      entry.at = now
      this.#hub.sendRaw(entry.frame)
    }
    return true
  }

  /** 把排队的 body 按序发出去；在途窗口满了就暂停，等腾出位置再继续。 */
  #flushQueued(): void {
    const queued = this.#queued
    let next = 0
    try {
      while (next < queued.length) {
        if (this.#windowFull(Date.now())) {
          this.#paused = true
          this.#arm()
          return
        }
        this.#emitData(queued[next++]!)
      }
      this.#windowUntil = 0
      this.#paused = false
    } finally {
      this.#queued = queued.slice(next)
    }
  }

  /**
   * hub 丢弃了我们发出的一封 `data`（不是因为对端离线）。hub 对 `dropped` 每连接每秒至多回一条，所以它不是
   * 「每个被丢的 seq 都有」，也不能据此逐条重发——它是「发错东西」的信号。只处理缓冲里有的 `seq`（seq 在整条
   * hub 连接上唯一，其他节点的丢弃与我们无关）：
   * - `not_paired`：hub 认为我们和节点没配对（被吊销）→ 通道失败（`NodeRejected`）；
   * - `too_large`：分片之后仍超限（hub 的 `ANYCON_MAX_ENVELOPE_BYTES` 比一片还小）；
   * - `rate_limited`：（协议里保留，hub 不再发——限速走 `backpressure`）有信封被限速丢了。
   * 后两种都意味着至少丢了一片，Noise 传输已经失步：报 `error` 事件，进行中的请求失败，重新握手。
   */
  #onDropped(seq: number, reason: DropReason): void {
    if (!this.#sent.has(seq)) return
    if (reason === 'not_paired') {
      const err = new NodeRejected('the hub says this device is no longer paired with the node')
      this.emit('error', err)
      this.#fail(err)
      return
    }
    const err =
      reason === 'too_large'
        ? new AnyconError(
            'envelope_too_large',
            'the hub dropped a data envelope as too large: its ANYCON_MAX_ENVELOPE_BYTES is below the fragment size',
          )
        : new AnyconError('envelope_dropped', `the hub dropped a data envelope (${reason})`)
    this.emit('error', err)
    this.#dropSession(err)
    if (this.#peerOnline) this.#startHandshake()
  }

  // ---------- 内部 ----------

  #dropSession(reason: Error): void {
    this.#clearHsTimer()
    this.#freeHandshake()
    this.#transport?.free()
    this.#transport = undefined
    this.#sent.clear()
    this.#sentBytes = 0
    this.#rejected.clear()
    this.#queued = []
    this.#paused = false
    this.#windowUntil = 0
    this.#backoff = 1
    this.#lastRoundAt = 0
    if (this.#wakeTimer !== undefined) clearTimeout(this.#wakeTimer)
    this.#wakeTimer = undefined
    this.#reassembler.clear()
    this.rpc.failAll(reason)
  }

  #freeHandshake(): void {
    try {
      this.#handshake?.free()
    } catch {
      // 已被消费
    }
    this.#handshake = undefined
    this.#hsSeq = 0
  }

  #clearHsTimer(): void {
    if (this.#hsTimer !== undefined) clearTimeout(this.#hsTimer)
    this.#hsTimer = undefined
    if (this.#hsRetryTimer !== undefined) clearTimeout(this.#hsRetryTimer)
    this.#hsRetryTimer = undefined
  }

  #setState(next: LinkState): void {
    const prev = this.#state
    if (prev === next) return
    this.#state = next
    this.emit('state', next, prev)
    if (prev === 'ready') this.emit('offline')
  }
}

/** `[0, max)` 的随机毫秒数（重发等待的抖动）。 */
function jitter(max: number): number {
  return Math.random() * max
}

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false
  return true
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}
