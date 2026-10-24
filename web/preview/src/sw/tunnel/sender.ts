// 隧道出站信封的发送方规则（docs/protocol.md § 3.1「hub 的顺序保证」、§ 11.5）。
//
// 和会话传输的发送方（Web UI 的 `EnvelopeSender`）规则一样，区别只有一条：保留原文不按「最近 10 秒」，
// 而是按**端到端回执**——对端的累计 `ack` 超过这条消息的编号才释放。已经被回执的消息一定被 hub 转发过，
// 所以它之后不会再被退回；晚到的退回认得出来（编号不大于最近一次被回执释放的 seq）直接忽略。
//
// - hub 对 `data` 信封只有转发和退回 `backpressure { to, seq }` 两种处理，从不静默丢弃。一个 seq 被退回后，之后的信封
//   都会被退回，直到它的重发被转发。所以收到退回就**暂停**发新信封（排队），等 `retry_after_ms`
//   （`rate_limited` 给的；没有就 100–300 ms 抖动），把被退回的 seq **按升序原样重发**（密文不能重新加密），再放行排队的；
// - `rate_limited` 暂停发往对端的信封（预览只有 daemon 一个对端；`to` 是别人时忽略）；
// - 被退回的 seq 原文已经不在（也没被回执）→ 传输失步，交给调用方重置；
// - `handshake` 信封被退回 → 它没送到：按 `retry_after_ms` 延后、连续几轮翻倍地原样重发同一条，既不立刻重发也不重置传输；
// - 不在传输编号里的信封（RESET 占位）不保留：被退回也不管，下一条握手转发成功时 hub 会清掉粘住状态。
//
// 这个类不碰 WebSocket：调用方给 `write`，并在收到对应控制消息时调用 `onBackpressure` 等。

/** 没有 `rate_limited` 提示时，重发 data 前等多久：100–300 ms（含抖动）。 */
const RETRY_MIN_MS = 100
const RETRY_JITTER_MS = 200
/** `rate_limited` 最多信到多久（防止 hub 让我们停一小时）。 */
const MAX_RETRY_AFTER_MS = 30_000
/** 握手被退回、又没有 `rate_limited` 提示时，第一轮等 300–1000 ms（含抖动），之后每轮翻倍，封顶 30 s。 */
const HANDSHAKE_RETRY_MIN_MS = 300
const HANDSHAKE_RETRY_JITTER_MS = 700
const HANDSHAKE_BACKOFF_CAP_MS = 30_000

export interface SenderDeps {
  /** 把一个已编码的信封写进 WebSocket。连接不可写返回 false。 */
  write(bytes: Uint8Array): boolean
  /** 对端（daemon）的设备 id：`rate_limited.to` 指向别人时与我们无关。 */
  peer: string
  /** 被退回的 seq 原文已经不在：这条传输只能作废、重新握手。 */
  needReset(reason: string): void
  /** [0, 1) 的随机数，用于抖动。 */
  rng?: () => number
}

interface Held {
  seq: number
  /** 在传输里的消息编号（从 0 起，不含握手）。 */
  index: number
  bytes: Uint8Array
}

interface HandshakeState {
  seq: number
  bytes: Uint8Array
  rounds: number
  timer: ReturnType<typeof setTimeout> | null
}

export class TunnelSender {
  /** 已写出、还没被回执的 data 信封，按 seq 升序（Map 保持插入序，重发不改变位置）。 */
  private readonly retained = new Map<number, Held>()
  /** 还没写出去的（暂停中 / 被限速 / 连接暂时不可写）。 */
  private outbox: Held[] = []
  private inflight = 0
  private ackedCount = 0
  /** 最近一次被回执释放的最大 seq：不大于它的退回都是晚到的，忽略。 */
  private lastReleasedSeq = -1
  /** 小于它的 seq 属于更早的传输（或握手）。 */
  private floorSeq = 0
  /** 当前传输发出过的最大 data seq：大于它的退回不是我们的 data 信封。 */
  private maxDataSeq = -1
  private readonly returned = new Set<number>()
  private blocked = false
  private timer: ReturnType<typeof setTimeout> | null = null
  private rateUntil = 0
  private handshake: HandshakeState | null = null

  constructor(private readonly deps: SenderDeps) {}

  /** 未被回执的密文字节数（含排队没写出去的）：调用方据此决定还能不能再发。 */
  get inflightBytes(): number {
    return this.inflight
  }

  /** 登记并（尽量马上）写出一个 data 信封。`seq` 单调递增，`index` 是传输里的消息编号。 */
  sendData(seq: number, index: number, bytes: Uint8Array): void {
    this.inflight += bytes.length
    this.maxDataSeq = Math.max(this.maxDataSeq, seq)
    const item: Held = { seq, index, bytes }
    if (this.blocked || this.outbox.length > 0 || Date.now() < this.rateUntil) {
      this.outbox.push(item)
      if (!this.blocked) this.armDrain()
      return
    }
    this.writeOrQueue(item)
  }

  private writeOrQueue(item: Held): void {
    if (this.deps.write(item.bytes)) this.retained.set(item.seq, item)
    else {
      this.outbox.push(item)
      this.armDrain(1_000)
    }
  }

  /** 不保留、被退回也不重发的信封（RESET 占位）。 */
  sendRaw(bytes: Uint8Array): void {
    this.deps.write(bytes)
  }

  // ---- 回执 ----

  /** 对端的累计回执：编号小于 `count` 的消息都已经被对端收到，原文可以释放。 */
  onAck(count: number): void {
    if (count <= this.ackedCount) return
    this.ackedCount = count
    for (const [seq, held] of this.retained) {
      if (held.index >= count) break
      this.retained.delete(seq)
      this.inflight -= held.bytes.length
      this.returned.delete(seq)
      if (seq > this.lastReleasedSeq) this.lastReleasedSeq = seq
    }
  }

  // ---- 来自 hub 的控制消息 ----

  onBackpressure(seq: number): void {
    if (this.handshake && this.handshake.seq === seq) return this.onHandshakeBounced(this.handshake)
    if (seq < this.floorSeq || seq <= this.lastReleasedSeq) return
    if (!this.retained.has(seq)) {
      if (this.outbox.some((o) => o.seq === seq)) return
      // 不是我们发的 data 信封（RESET 占位等）也会走到这里，但那些不进 `retained`：只有 seq 落在当前传输里才算丢了原文
      if (this.knownTransportSeq(seq)) this.deps.needReset(`hub returned envelope ${seq}, which is no longer in the resend buffer`)
      return
    }
    const wasBlocked = this.blocked
    this.returned.add(seq)
    this.blocked = true
    if (!wasBlocked || this.timer === null) this.arm(this.retryDelay())
  }

  private knownTransportSeq(seq: number): boolean {
    return seq >= this.floorSeq && seq <= this.maxDataSeq
  }

  onRateLimited(retryAfterMs: number, to?: string | null): void {
    if (to && to !== this.deps.peer) return
    const wait = Math.min(Math.max(retryAfterMs, 0), MAX_RETRY_AFTER_MS)
    this.rateUntil = Math.max(this.rateUntil, Date.now() + wait)
    if (!this.blocked && this.outbox.length > 0) this.armDrain()
  }

  /** `undeliverable { seq }`：对端不在线，那一封不会再有机会重发。 */
  forget(seq: number): void {
    const held = this.retained.get(seq)
    if (held) {
      this.retained.delete(seq)
      this.inflight -= held.bytes.length
    }
    this.returned.delete(seq)
  }

  // ---- 重发 ----

  private retryDelay(): number {
    return RETRY_MIN_MS + (this.deps.rng ?? Math.random)() * RETRY_JITTER_MS
  }

  private arm(baseDelay: number): void {
    const wait = Math.max(baseDelay, this.rateUntil - Date.now(), 0)
    if (this.timer) clearTimeout(this.timer)
    this.timer = setTimeout(() => this.fire(), wait)
  }

  private armDrain(minDelay = 0): void {
    if (this.timer !== null) return
    this.arm(minDelay)
  }

  private fire(): void {
    this.timer = null
    const now = Date.now()
    if (now < this.rateUntil) return this.arm(0)
    if (this.blocked) {
      const seqs = [...this.returned].sort((a, b) => a - b)
      this.returned.clear()
      for (const [i, seq] of seqs.entries()) {
        const held = this.retained.get(seq)
        if (!held) continue // 期间被回执了
        if (!this.deps.write(held.bytes)) {
          // 连接眼下写不进去：没发的放回去，过一会儿再试
          for (const rest of seqs.slice(i)) this.returned.add(rest)
          return this.arm(this.retryDelay())
        }
      }
      this.blocked = false
    }
    this.drain()
  }

  private drain(): void {
    while (this.outbox.length > 0 && !this.blocked) {
      if (Date.now() < this.rateUntil) return this.arm(0)
      const item = this.outbox[0]!
      if (!this.deps.write(item.bytes)) return this.arm(1_000)
      this.outbox.shift()
      this.retained.set(item.seq, item)
    }
  }

  // ---- 握手 ----

  /** 写出（或在被限速时延后写出）握手信封；被退回时会原样重发，直到 `handshakeDone()`。 */
  sendHandshake(seq: number, bytes: Uint8Array): void {
    this.cancelHandshake()
    const state: HandshakeState = { seq, bytes, rounds: 0, timer: null }
    this.handshake = state
    const wait = this.rateUntil - Date.now()
    if (wait > 0) state.timer = setTimeout(() => this.fireHandshake(state), wait + this.retryDelay())
    else this.deps.write(bytes)
  }

  private onHandshakeBounced(state: HandshakeState): void {
    state.rounds += 1
    if (state.timer) clearTimeout(state.timer)
    const rng = this.deps.rng ?? Math.random
    const backoff = (HANDSHAKE_RETRY_MIN_MS + rng() * HANDSHAKE_RETRY_JITTER_MS) * 2 ** (state.rounds - 1)
    const hint = this.rateUntil - Date.now()
    const wait = Math.min(Math.max(backoff, hint > 0 ? hint + this.retryDelay() : 0), HANDSHAKE_BACKOFF_CAP_MS)
    state.timer = setTimeout(() => this.fireHandshake(state), wait)
  }

  private fireHandshake(state: HandshakeState): void {
    if (this.handshake !== state) return
    state.timer = null
    const wait = this.rateUntil - Date.now()
    if (wait > 0) {
      state.timer = setTimeout(() => this.fireHandshake(state), wait + this.retryDelay())
      return
    }
    this.deps.write(state.bytes)
  }

  /** 握手完成（或放弃）：不再重发。 */
  handshakeDone(): void {
    this.cancelHandshake()
  }

  private cancelHandshake(): void {
    if (this.handshake?.timer) clearTimeout(this.handshake.timer)
    this.handshake = null
  }

  /** 握手信封是不是正被扣着（延后重发中）。 */
  get handshakeHeld(): boolean {
    return this.handshake?.timer != null
  }

  // ---- 清理 ----

  /**
   * 当前传输作废（握手重来、对端要求重置）：释放全部原文。`nextSeq` 是之后第一个信封的 seq，小于它的退回都是旧传输的，
   * 晚到时忽略。
   */
  discardTransport(nextSeq: number): void {
    this.retained.clear()
    this.outbox = []
    this.returned.clear()
    this.inflight = 0
    this.ackedCount = 0
    this.lastReleasedSeq = -1
    this.maxDataSeq = -1
    this.blocked = false
    this.floorSeq = nextSeq
    if (this.timer) clearTimeout(this.timer)
    this.timer = null
    this.cancelHandshake()
  }

  /** 连接断了：全部丢弃。 */
  clear(): void {
    this.discardTransport(0)
    this.rateUntil = 0
  }
}
