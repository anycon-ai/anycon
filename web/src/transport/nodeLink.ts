// 与单个 daemon 的会话：IK 握手（web 是 initiator）、加解密、JSON-RPC。
// 一条 JSON-RPC = 一段 Noise 密文 = 一个或多个 `data` 信封（ADR-30：body 带 1 字节框架头，超过 200 KiB 分片）；
// `handshake` 信封承载 Noise 握手消息。
import { DUPLICATE_PEER_ONLINE_MS, HANDSHAKE_TIMEOUT_MS, RPC_TIMEOUT_MS } from '@/lib/constants'
import { bytesEqual } from '@/lib/bytes'
import { initialNode, nodeReduce, type NodeEvent, type NodeMachine } from './connection'
import type { Crypto, Handshake, Keypair, Transport } from './crypto'
import { RpcClient } from './rpcClient'
import { decodeFrame, encodeFrames, Reassembler } from './wire'

export interface LinkContext {
  crypto: Crypto
  keypair: Keypair
  /** hub 当前能不能发（已认证、ws 开着）。加密会推进 nonce，所以必须先问再加密。 */
  canSend(): boolean
  /** 发一个 `data` 信封（body 已经带好框架头）。返回信封 seq，发不出去返回 null。 */
  sendData(to: string, body: Uint8Array): number | null
  sendHandshake(to: string, message: Uint8Array): number | null
  /** 这个节点的传输作废了：丢掉它名下排队没发的、被 hub 退回等重发的信封（密文属于旧传输，发过去只会让对端再次失步）。 */
  discardPending(nodeId: string): void
  /**
   * 这个节点的握手信封眼下是不是被发送方扣着：被 hub 退回（限速 / 缓冲满）后延后重发中，或在 outbox 里等 `rate_limited` 过去。
   * 它根本没送到对端，所以握手超时计时不该走——否则限速几十秒就会被误判成「daemon 不认识我们的公钥」（`noise_failed`）。
   */
  handshakeHeld(nodeId: string): boolean
  onChange(nodeId: string): void
  onReady(nodeId: string): void
  onNotification(nodeId: string, method: string, params: unknown): void
}

export class NodeLink {
  machine: NodeMachine = initialNode
  readonly rpc: RpcClient
  private hs: Handshake | null = null
  /** 在途握手的第一条消息：被 hub 退回（没送到）后原样再发一次。 */
  private hsMessage: Uint8Array | null = null
  private transport: Transport | null = null
  private hsTimer: ReturnType<typeof setTimeout> | null = null
  /** 在途的这次 IK 握手是什么时候发出的（用来认出「紧随其后」的重复 peer_online）。 */
  private hsStartedAt = 0
  /** 握手回复还在路上时又来了一个（不是紧随其后的）peer_online：这一轮有结果后丢掉它，按新的 peer_online 重来。 */
  private restartAfterHandshake = false
  /** 入站分片重组（每个对端一个）。 */
  private readonly reassembler = new Reassembler()
  /** 出站消息 id（u32，每个节点自己递增，分片靠它区分消息）。 */
  private msgId = 0

  constructor(
    readonly nodeId: string,
    public publicKey: Uint8Array,
    private readonly ctx: LinkContext,
    rpcTimeoutMs: number = RPC_TIMEOUT_MS,
  ) {
    this.rpc = new RpcClient((plain) => this.sendPlain(plain), rpcTimeoutMs)
    this.rpc.onNotification = (method, params) => this.ctx.onNotification(this.nodeId, method, params)
  }

  get ready(): boolean {
    return this.machine.noise === 'ready' && this.transport !== null
  }

  private dispatch(e: NodeEvent): void {
    const next = nodeReduce(this.machine, e)
    if (next === this.machine) return
    this.machine = next
    this.ctx.onChange(this.nodeId)
  }

  private sendPlain(plain: Uint8Array): number[] | null {
    if (!this.transport || !this.ctx.canSend()) return null
    const bodies = encodeFrames(this.transport.encrypt(plain), this.msgId)
    this.msgId = (this.msgId + 1) >>> 0
    const seqs: number[] = []
    for (const body of bodies) {
      const seq = this.ctx.sendData(this.nodeId, body)
      if (seq === null) {
        // canSend() 刚刚还是 true，走到这里说明连接在中途断了：已加密的部分不可能再补发，传输只能作废。
        this.resetTransport('send failed mid-message')
        return null
      }
      seqs.push(seq)
    }
    return seqs
  }

  private teardownNoise(): void {
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = null
    this.restartAfterHandshake = false
    this.reassembler.clear()
    this.hs?.free()
    this.hs = null
    this.hsMessage = null
    this.transport?.free()
    this.transport = null
    this.ctx.discardPending(this.nodeId)
  }

  // ---- 来自 hub / 客户端的事件 ----

  hubDown(): void {
    this.teardownNoise()
    this.rpc.rejectAll('closed', 'hub connection lost')
    this.dispatch({ type: 'hub_down' })
  }

  /**
   * 对端（重新）上线：丢掉现有传输，重新做 IK 握手。
   *
   * 但**不能在上一条握手的回复还没到时并行再发一条**：IK 回复只能配给发出它的那次握手，第二次握手建好之后，
   * 第一次的回复到了会被喂给第二次的握手 → 解密失败 → `noise_failed`。重新配对后就会这样：`attachNode`
   * 和 hub 在 `PairDone` 之后发的 `PeerOnline` 各要求握手一次，相隔几毫秒。所以握手在途时：
   * - 紧随其后（`DUPLICATE_PEER_ONLINE_MS` 内）的重复通知直接忽略——在途的这条握手就是为它发的；
   * - 更晚的（真的换了连接）不并行：等这一轮有结果（回复 / 失败 / 超时）后丢弃它，再重新握手一次。
   */
  peerOnline(): void {
    if (this.hs && this.machine.noise === 'handshaking') {
      // 握手被 hub 退回、还没送到过谁：它最终会发给现在在线的这个连接，不用等有结果后再重来
      const held = this.ctx.handshakeHeld(this.nodeId)
      if (!held && Date.now() - this.hsStartedAt >= DUPLICATE_PEER_ONLINE_MS) this.restartAfterHandshake = true
      return
    }
    this.restartHandshake()
  }

  /** 丢掉当前这一轮（传输和在途握手），当作一次新的 peer_online 重新握手。 */
  private restartHandshake(): void {
    this.teardownNoise()
    this.rpc.rejectAll('closed', 'peer reconnected')
    this.machine = nodeReduce(this.machine, { type: 'peer_online' })
    this.ctx.onChange(this.nodeId)
    this.startHandshake()
  }

  peerOffline(): void {
    this.teardownNoise()
    this.rpc.rejectAll('closed', 'peer offline')
    this.dispatch({ type: 'peer_offline' })
  }

  /** 更新 daemon 公钥（重新配对后），并从头握手。 */
  rekey(publicKey: Uint8Array): void {
    const same = bytesEqual(publicKey, this.publicKey)
    this.publicKey = publicKey
    if (!this.machine.peerOnline) return
    // 在途的握手用的就是这把钥匙（重新配对同一个节点时 hub 的 PeerOnline 已经让握手开始了）：不用重来
    if (same && this.hs && this.machine.noise === 'handshaking') return
    this.peerOnline()
  }

  startHandshake(): void {
    if (!this.machine.peerOnline || this.machine.noise === 'handshaking' || this.machine.noise === 'ready') return
    if (!this.ctx.canSend()) return
    this.teardownNoise()
    let msg: Uint8Array
    try {
      this.hs = this.ctx.crypto.reconnect('initiator', this.ctx.keypair, this.publicKey)
      msg = this.hs.write()
    } catch (e) {
      console.warn('anycon: cannot start the IK handshake', e)
      this.teardownNoise()
      this.dispatch({ type: 'handshake_failed' })
      return
    }
    // 先进入 handshaking 再发：回复可能在 send 返回前就到了（同步传输 / 测试）。
    this.dispatch({ type: 'handshake_started' })
    this.hsMessage = msg
    this.sendHandshakeMessage(msg)
  }

  /** 发（或重发）在途握手的第一条消息，并从现在起计握手超时。发不出去（hub 不可写）就放弃这一轮。 */
  private sendHandshakeMessage(msg: Uint8Array): void {
    this.hsStartedAt = Date.now()
    if (this.hsTimer) clearTimeout(this.hsTimer)
    this.hsTimer = setTimeout(() => this.onHandshakeTimeout(), HANDSHAKE_TIMEOUT_MS)
    if (this.ctx.sendHandshake(this.nodeId, msg) === null) {
      this.teardownNoise()
      this.dispatch({ type: 'reset' })
    }
  }

  /**
   * hub 退回了握手信封（限速 / 对端缓冲满），发送方延后到点了：把同一条握手消息再发一次。
   * 它从来没送到过对端，重发是安全的；不重置也不新起握手（新起的握手会换一把临时密钥，没有必要）。
   * 握手已经结束 / 放弃了（回复到了、超时、传输重置）就什么也不做。
   */
  resendHandshake(): void {
    const msg = this.hsMessage
    if (!msg || !this.hs || this.machine.noise !== 'handshaking' || !this.ctx.canSend()) return
    this.sendHandshakeMessage(msg)
  }

  private onHandshakeTimeout(): void {
    this.hsTimer = null
    if (this.restartAfterHandshake) return this.restartHandshake()
    if (this.ctx.handshakeHeld(this.nodeId)) {
      // 握手被扣着（限速），根本没送出去：不算对端没回，接着等
      this.hsTimer = setTimeout(() => this.onHandshakeTimeout(), HANDSHAKE_TIMEOUT_MS)
      return
    }
    this.teardownNoise()
    this.dispatch({ type: 'handshake_timeout' })
    if (this.machine.noise === 'none') this.startHandshake()
  }

  onHandshake(body: Uint8Array): void {
    const hs = this.hs
    if (!hs) return
    try {
      hs.read(body)
      if (!hs.isFinished() && hs.isMyTurn()) {
        if (this.ctx.sendHandshake(this.nodeId, hs.write()) === null) throw new Error('hub unavailable')
      }
      if (!hs.isFinished()) return
      const remote = hs.remotePublic()
      const transport = hs.intoTransport()
      if (remote && !bytesEqual(remote, this.publicKey)) {
        transport.free()
        throw new Error('remote static key mismatch')
      }
      if (this.restartAfterHandshake) {
        // 这一轮是在更新的 peer_online 之前发的：对端可能已经换了连接，这个传输不可信，丢掉重来
        transport.free()
        return this.restartHandshake()
      }
      if (this.hsTimer) clearTimeout(this.hsTimer)
      this.hsTimer = null
      this.hs = null
      this.transport = transport
    } catch (e) {
      if (this.restartAfterHandshake) return this.restartHandshake() // 本来就要按新的 peer_online 重来
      console.warn(`anycon: IK handshake with node ${this.nodeId.slice(0, 8)} failed`, e)
      this.teardownNoise()
      this.dispatch({ type: 'handshake_failed' })
      return
    }
    this.dispatch({ type: 'handshake_ok' })
    this.ctx.onReady(this.nodeId)
  }

  onData(body: Uint8Array): void {
    const transport = this.transport
    if (!transport) return
    let ciphertext: Uint8Array | undefined
    try {
      const frame = decodeFrame(body)
      if (frame.kind === 'reset') return this.resetTransport('peer sent RESET')
      ciphertext = this.reassembler.push(frame)
    } catch (e) {
      return this.resetTransport(`bad frame: ${(e as Error).message}`)
    }
    if (!ciphertext) return // 分片还没收齐
    let plain: Uint8Array
    try {
      plain = transport.decrypt(ciphertext)
    } catch (e) {
      // 密文对不上（丢包 / 乱序 / 篡改）：这条会话不可信了，重新握手。
      return this.resetTransport(`decrypt failed: ${(e as Error).message}`)
    }
    this.rpc.handle(plain)
  }

  /** 传输层失步 / 作废：丢掉 Transport，在途请求以 `closed` 失败，然后重新做 IK 握手。 */
  resetTransport(reason: string): void {
    console.warn(`anycon: noise transport with node ${this.nodeId.slice(0, 8)} reset (${reason}); re-handshaking`)
    this.teardownNoise()
    this.rpc.rejectAll('closed', 'noise session reset')
    this.dispatch({ type: 'reset' })
    this.startHandshake()
  }

  /** 发起请求。链路未就绪时立即以 `offline` 失败（调用方据此标记「未发送」）。 */
  request<R = unknown>(method: string, params: unknown = {}, timeoutMs?: number): Promise<R> {
    return this.rpc.request<R>(method, params, timeoutMs)
  }

  undeliverable(seq: number): boolean {
    return this.rpc.undeliverable(seq)
  }

  dispose(): void {
    this.teardownNoise()
    this.rpc.rejectAll('closed', 'link disposed')
  }
}
