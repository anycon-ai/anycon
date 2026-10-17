// 出站信封的发送方规则（docs/protocol.md § 3.1「hub 的顺序保证」）。
//
// hub 对已配对 `(from, to)` 的 `data` 信封只有两种处理：转发，或退回 `backpressure { to, seq }`——从不静默丢弃。
// 一个 seq 被退回后，hub 把之后同一 `(from, to)` 的所有信封都退回，直到那个 seq 的重发被转发。Noise 传输用隐式递增的 nonce，
// 接收方一旦看到乱序就只能重置，所以发送方必须配合：
//
// 1. 留着最近发出的 `data` 信封**原文**（按字节封顶，不是按条数），不能重新加密。
//    保留缓冲是**在途窗口**，不是 LRU：最近 `SENT_RETAIN_MS`（10 s）内发出的、以及被退回正等着重发的信封**永远不逐出**
//    （它们可能还在路上、hub 还没处理，被退回时得有原文可重发）；放不下新的就**暂停发送**（新信封留在 outbox 里），
//    等旧的过了保护期再逐出、腾出位置。传输作废后它名下的信封原文立刻释放，只留一个 10 s 的墓碑（seq → 对端）
//    用来认出晚到的退回，不让它们去重置已经重新握手的新传输；
// 2. 收到 `backpressure` 就**暂停该对端**（新的 `data` 信封先在 outbox 里排队，不写出去），等 `retry_after_ms`
//    （最近一条 `rate_limited` 给的；没有就 100–300 ms + 抖动），然后把**所有**被退回的 seq **按升序原样重发**，再放行排队的新信封；
//    重发期间又被退回就再等一轮；
// 3. 被退回的 seq 已经不在保留缓冲里 → 该对端的传输只能作废，重新握手（`resetPeer`）；
// 4. `handshake` 信封被退回（限速 / 对端缓冲满）→ 它根本没送到，既不重置也不立即重发：按最近 `rate_limited` 的
//    `retry_after_ms`（没有就 300–1000 ms + 抖动，连续几轮翻倍，封顶 30 s）延后，再叫调用方把同一条握手消息重新发一次。
//    立即重发会在超限期间形成「握手 → 退回 → 再握手」的循环，持续 10 s 就被 hub 以 1008 断开；
// 5. `rate_limited` 是整条 hub 连接的暂停提示（速率预算是按连接算的），不对应某一封信；`dropped` 不在这里处理（不重发）。
//
// 这个类不碰 WebSocket：调用方给 `write`，并在收到对应控制消息时调 `onBackpressure` 等。

import type { Envelope } from './wire'

/** 留作重发用的已发信封的总字节上限，同时也是在途窗口的大小。要容得下一条最大的请求（8 MiB 明文 + 分片 / 信封头）再留余量，所以取 16 MiB。 */
export const SENT_LOG_MAX_BYTES = 16 * 1024 * 1024
/** 发出后这么久之内的信封永远不逐出（hub 的退回通常在几百毫秒内到，这里留足余量）。 */
export const SENT_RETAIN_MS = 10_000
/** 没有 `rate_limited` 提示时，重发 data 前等多久：100–300 ms（含抖动）。 */
const RETRY_MIN_MS = 100
const RETRY_JITTER_MS = 200
/** `rate_limited` 最多信到多久（防止 hub 让我们停一小时）。 */
const MAX_RETRY_AFTER_MS = 30_000
/** 一个对端连续被退回超过这么久还没通：放弃这条传输，重新握手。 */
export const STUCK_RESET_MS = 60_000
/** 这么久没有新的退回，就算上一轮结束了（重置 STUCK 计时）。 */
const QUIET_MS = 5_000
/** 握手被退回、又没有 `rate_limited` 提示时，第一轮等 300–1000 ms（含抖动），之后每轮翻倍。 */
export const HANDSHAKE_RETRY_MIN_MS = 300
export const HANDSHAKE_RETRY_MAX_MS = 1_000
/** 握手重试间隔的封顶。 */
export const HANDSHAKE_BACKOFF_CAP_MS = 30_000
/** 这么久没有握手被退回，就算上一段结束了（翻倍从头算）。比封顶还长，免得刚好封顶的间隔被当成「安静」。 */
const HANDSHAKE_QUIET_MS = 60_000
/** 记住最近多少个握手信封的 seq（用来认出它们的退回）。 */
const HANDSHAKE_SEQS_KEEP = 64
/** 墓碑数量的硬上限（正常情况下 10 s 就过期，这是防御）。 */
const RETIRED_MAX = 65_536

export interface SenderDeps {
  /** 把一个已编码的信封写进 WebSocket。连接不可写返回 false。 */
  write(bytes: Uint8Array): boolean
  /** 该对端的传输要作废：丢掉 Noise 会话、重新握手。实现里会回调 `discardPeer(to)`。 */
  resetPeer(to: string, reason: string): void
  /** 握手信封被退回、延后时间到了：叫该对端的链路把（还在进行中的）那条握手消息再发一次。 */
  retryHandshake(to: string): void
  /** [0, 1) 的随机数，用于抖动。 */
  rng?: () => number
  /** 重发缓冲的字节上限。 */
  maxLogBytes?: number
}

interface Pending {
  seq: number
  to: string
  kind: Envelope['kind']
  bytes: Uint8Array
}

interface LogEntry {
  bytes: Uint8Array
  to: string
  /** 最近一次写出去的时间（重发也算）。保护期从这里算起。 */
  sentAt: number
}

/** 所属传输已经作废的 seq：原文已释放，只记住它是谁的，晚到的退回 / 回执据此忽略。 */
interface Tombstone {
  to: string
  at: number
}

interface PeerState {
  /** 被退回、等着重发的 seq。 */
  returned: Set<number>
  /** 暂停中：该对端新的 `data` 信封先不写出去。 */
  blocked: boolean
  timer: ReturnType<typeof setTimeout> | null
  /** 连续被退回的这一段从什么时候开始。 */
  since: number
  lastBounceAt: number
}

interface HandshakeState {
  /** 该对端最近一次发出（或排队）的握手信封。更早的握手已经放弃，它们晚到的退回不理。 */
  seq: number
  /** 连续被退回的轮数；决定下一次延后多久。 */
  rounds: number
  lastBounceAt: number
  /** 延后重发的定时器；null = 没有在等。 */
  timer: ReturnType<typeof setTimeout> | null
}

export class EnvelopeSender {
  private outbox: Pending[] = []
  /** 按最近一次写出的先后排列（重发会挪到末尾）：逐出时从头往后看，遇到还在保护期内的就可以停。 */
  private readonly sentLog = new Map<number, LogEntry>()
  private readonly retired = new Map<number, Tombstone>()
  private logBytes = 0
  private readonly peers = new Map<string, PeerState>()
  private readonly handshakes = new Map<string, HandshakeState>()
  private readonly handshakeSeqs = new Map<number, string>()
  /** `rate_limited` 要求暂停到什么时候（整条连接）。 */
  private rateUntil = 0
  private drainTimer: ReturnType<typeof setTimeout> | null = null
  private drainAt = 0
  /** 上一轮 drain 因为在途窗口满了而停下（有 data 信封在 outbox 里等位置）。 */
  private windowHeld = false
  private readonly maxLogBytes: number

  constructor(private readonly deps: SenderDeps) {
    this.maxLogBytes = deps.maxLogBytes ?? SENT_LOG_MAX_BYTES
  }

  // ---- 发 ----

  /** 登记并（尽量马上）发出一个信封。`seq` 必须单调递增。 */
  send(seq: number, to: string, kind: Envelope['kind'], bytes: Uint8Array): void {
    if (kind === 'handshake') this.noteHandshake(seq, to)
    this.outbox.push({ seq, to, kind, bytes })
    this.drain()
  }

  private drain(): void {
    const now = Date.now()
    this.pruneRetired(now)
    if (this.outbox.length === 0) {
      this.windowHeld = false
      return
    }
    if (now < this.rateUntil) {
      this.armDrain(this.rateUntil - now)
      return
    }
    this.outbox.sort((a, b) => a.seq - b.seq)
    const held: Pending[] = []
    let failed = false
    // 在途窗口满了之后，后面所有 data 都排在它后面等（按 seq 的先后，不让小的插队饿死大的）
    let windowFull = false
    for (const item of this.outbox) {
      if (failed || (item.kind === 'data' && (windowFull || this.peers.get(item.to)?.blocked))) {
        held.push(item)
        continue
      }
      if (item.kind === 'data' && !this.hasRoom(item.bytes.length, now)) {
        windowFull = true
        held.push(item)
        continue
      }
      if (!this.deps.write(item.bytes)) {
        failed = true
        held.push(item)
        continue
      }
      if (item.kind === 'data') this.record(item, now)
    }
    this.outbox = held
    this.windowHeld = windowFull
    if (windowFull) this.armDrain(this.nextRoomIn(now))
  }

  /** 在 `ms` 之后 drain 一次；已经有更早的定时器就不动，更晚的换成这个。 */
  private armDrain(ms: number): void {
    const at = Date.now() + ms
    if (this.drainTimer && this.drainAt <= at) return
    if (this.drainTimer) clearTimeout(this.drainTimer)
    this.drainAt = at
    this.drainTimer = setTimeout(() => {
      this.drainTimer = null
      this.drain()
    }, ms)
  }

  /** 腾出了位置（释放 / 作废）：在窗口上等着的 data 马上再试一次。 */
  private wake(): void {
    if (this.windowHeld) this.armDrain(0)
  }

  // ---- 在途窗口 ----

  /** 写出去之后才进重发缓冲（排队没发的不占位）。 */
  private record(item: Pending, now: number): void {
    this.sentLog.set(item.seq, { bytes: item.bytes, to: item.to, sentAt: now })
    this.logBytes += item.bytes.length
  }

  /**
   * 再放一封 `len` 字节的 data 信封，窗口还放得下吗？放不下先试着逐出**过了保护期**且没在等重发的旧项；还是放不下就是不行（暂停）。
   * 窗口是空的时候总是放行：单封信封比整个窗口还大也得能发出去，否则永远发不出去。
   */
  private hasRoom(len: number, now: number): boolean {
    if (this.logBytes === 0 || this.logBytes + len <= this.maxLogBytes) return true
    this.evict(now, len)
    return this.logBytes === 0 || this.logBytes + len <= this.maxLogBytes
  }

  private evict(now: number, need: number): void {
    let pinned: Set<number> | undefined
    for (const [seq, e] of this.sentLog) {
      if (this.logBytes + need <= this.maxLogBytes) return
      if (now - e.sentAt < SENT_RETAIN_MS) return // 按写出先后排列：后面的都还在保护期内
      pinned ??= this.returnedSeqs()
      if (!pinned.has(seq)) this.unlog(seq)
    }
  }

  /** 被退回、正等着重发的 seq：重发要用原文，不能逐出。 */
  private returnedSeqs(): Set<number> {
    const all = new Set<number>()
    for (const p of this.peers.values()) for (const seq of p.returned) all.add(seq)
    return all
  }

  /** 窗口满了，最早什么时候有可能腾出位置（只是个提示：腾出 / 作废会 `wake()`，这里封顶 1 s 兜底）。 */
  private nextRoomIn(now: number): number {
    const pinned = this.returnedSeqs()
    for (const [seq, e] of this.sentLog) {
      if (pinned.has(seq)) continue
      return Math.min(Math.max(e.sentAt + SENT_RETAIN_MS - now, 50), 1000)
    }
    return 1000
  }

  private unlog(seq: number): void {
    const e = this.sentLog.get(seq)
    if (!e) return
    this.sentLog.delete(seq)
    this.logBytes -= e.bytes.length
  }

  /** 这条信封所属的传输作废了：释放原文，留个墓碑。 */
  private retire(seq: number, e: LogEntry, now: number): void {
    this.unlog(seq)
    this.retired.set(seq, { to: e.to, at: now })
  }

  private pruneRetired(now: number): void {
    for (const [seq, t] of this.retired) {
      if (now - t.at < SENT_RETAIN_MS && this.retired.size <= RETIRED_MAX) break
      this.retired.delete(seq)
    }
  }

  // ---- 来自 hub 的回执 ----

  /** `backpressure { to, seq }`：那一封没有转发。data 的话该对端进入暂停；handshake 的话延后再握手。 */
  onBackpressure(to: string, seq: number): void {
    const hsPeer = this.handshakeSeqs.get(seq)
    if (hsPeer !== undefined) return this.onHandshakeBounced(hsPeer, seq)
    if (this.retired.has(seq)) return // 所属传输已经作废并重新握手了，这封信不该再出现
    const entry = this.sentLog.get(seq)
    const peer = entry?.to ?? to
    if (!entry) {
      this.deps.resetPeer(peer, `hub returned envelope ${seq}, which is no longer in the resend buffer`)
      return
    }
    const now = Date.now()
    let p = this.peers.get(peer)
    if (!p) {
      p = { returned: new Set(), blocked: false, timer: null, since: now, lastBounceAt: now }
      this.peers.set(peer, p)
    }
    if (now - p.lastBounceAt > QUIET_MS) p.since = now
    p.lastBounceAt = now
    p.blocked = true
    p.returned.add(seq)
    if (p.timer === null) this.arm(peer, p, this.retryDelay())
  }

  /** `rate_limited { retry_after_ms }`：整条连接暂停这么久（含重发）。 */
  onRateLimited(retryAfterMs: number): void {
    this.rateUntil = Math.max(this.rateUntil, Date.now() + Math.min(Math.max(retryAfterMs, 0), MAX_RETRY_AFTER_MS))
    // 已经排队的重发 / 新信封 / 延后的握手到点时会重新检查 rateUntil
  }

  /** `undeliverable { seq }`：对端不在线，那一封不会再有机会。 */
  onUndeliverable(seq: number): void {
    this.forget(seq)
  }

  /** 这个 seq 对应的信封发给谁（`dropped` 回执只有 seq）；不在缓冲里返回 undefined。传输已作废的 `dead` 为 true。 */
  lookup(seq: number): { to: string; dead: boolean } | undefined {
    const e = this.sentLog.get(seq)
    if (e) return { to: e.to, dead: false }
    const t = this.retired.get(seq)
    return t && { to: t.to, dead: true }
  }

  /** 不再重发这个 seq（`undeliverable` / `dropped`）。 */
  forget(seq: number): void {
    this.unlog(seq)
    this.retired.delete(seq)
    for (const p of this.peers.values()) p.returned.delete(seq)
    this.wake()
  }

  // ---- 重发 ----

  private retryDelay(): number {
    return RETRY_MIN_MS + (this.deps.rng ?? Math.random)() * RETRY_JITTER_MS
  }

  /** `baseDelay` 之后重发；`rate_limited` 要求的暂停更长就等它。 */
  private arm(peer: string, p: PeerState, baseDelay: number): void {
    const wait = Math.max(baseDelay, this.rateUntil - Date.now())
    p.timer = setTimeout(() => this.fire(peer), Math.max(0, wait))
  }

  /** 一轮结束：把所有被退回的 seq 按升序原样重发，然后放行排队的新信封。 */
  private fire(peer: string): void {
    const p = this.peers.get(peer)
    if (!p) return
    p.timer = null
    const now = Date.now()
    if (now < this.rateUntil) return this.arm(peer, p, 0) // 等待期间又收到了更长的 rate_limited
    if (now - p.since > STUCK_RESET_MS) {
      this.deps.resetPeer(peer, `hub kept returning envelopes for ${Math.round((now - p.since) / 1000)} s`)
      return
    }
    const seqs = [...p.returned].sort((a, b) => a - b)
    p.returned.clear()
    for (const [i, seq] of seqs.entries()) {
      const entry = this.sentLog.get(seq)
      if (!entry) {
        if (this.retired.has(seq)) continue // 所属传输作废了，不重发
        this.deps.resetPeer(peer, `envelope ${seq} was returned by the hub but is no longer in the resend buffer`)
        return
      }
      if (!this.deps.write(entry.bytes)) {
        // 连接眼下写不进去：没发的放回去，过一会儿再试（连接断了的话 clear() 会收拾）
        for (const rest of seqs.slice(i)) p.returned.add(rest)
        return this.arm(peer, p, this.retryDelay())
      }
      // 又发出去了一次：重新计保护期，并挪到末尾保持「按写出先后排列」
      this.sentLog.delete(seq)
      this.sentLog.set(seq, { ...entry, sentAt: now })
    }
    p.blocked = false
    this.drain()
  }

  // ---- 握手被退回 ----

  /** 登记一个握手信封（排队时就记，退回的回执可能比写出去还早到）。 */
  private noteHandshake(seq: number, to: string): void {
    let h = this.handshakes.get(to)
    if (!h) {
      h = { seq, rounds: 0, lastBounceAt: 0, timer: null }
      this.handshakes.set(to, h)
    } else {
      h.seq = seq
      // 新发的握手取代了还在等的重发
      if (h.timer) clearTimeout(h.timer)
      h.timer = null
    }
    this.handshakeSeqs.set(seq, to)
    if (this.handshakeSeqs.size > HANDSHAKE_SEQS_KEEP) this.handshakeSeqs.delete(this.handshakeSeqs.keys().next().value!)
  }

  private onHandshakeBounced(peer: string, seq: number): void {
    const h = this.handshakes.get(peer)
    if (!h || h.seq !== seq) return // 已经放弃的那次握手，晚到的退回
    const now = Date.now()
    if (now - h.lastBounceAt > HANDSHAKE_QUIET_MS) h.rounds = 0
    h.lastBounceAt = now
    if (h.timer === null) h.rounds += 1
    else clearTimeout(h.timer) // 已经在等了：只刷新计时，不算新的一轮
    h.timer = setTimeout(() => this.fireHandshake(peer), this.handshakeDelay(h.rounds, now))
  }

  /**
   * 延后多久再握手：300–1000 ms（含抖动）起步，每连续一轮翻倍，封顶 30 s；`rate_limited` 还在生效的话至少等到它过去（再加一点抖动）。
   * 两条消息谁先到都一样：`rate_limited` 晚到的话，到点时 `fireHandshake` 发现还在窗口里会继续等。
   */
  private handshakeDelay(rounds: number, now: number): number {
    const rng = this.deps.rng ?? Math.random
    const backoff = (HANDSHAKE_RETRY_MIN_MS + rng() * (HANDSHAKE_RETRY_MAX_MS - HANDSHAKE_RETRY_MIN_MS)) * 2 ** Math.max(0, rounds - 1)
    const hint = this.rateUntil - now
    return Math.min(Math.max(backoff, hint > 0 ? hint + this.retryDelay() : 0), HANDSHAKE_BACKOFF_CAP_MS)
  }

  private fireHandshake(peer: string): void {
    const h = this.handshakes.get(peer)
    if (!h) return
    h.timer = null
    const now = Date.now()
    if (now < this.rateUntil) {
      // 等待期间才收到（或收到了更长的）rate_limited：等它过去
      h.timer = setTimeout(() => this.fireHandshake(peer), this.rateUntil - now + this.retryDelay())
      return
    }
    this.deps.retryHandshake(peer)
  }

  /** 这个对端的握手眼下是不是被我们扣着（延后重发中，或还在 outbox 里等 `rate_limited` 过去）？此时握手超时计时不该走。 */
  isHandshakeHeld(peer: string): boolean {
    return (this.handshakes.get(peer)?.timer ?? null) !== null || this.outbox.some((i) => i.kind === 'handshake' && i.to === peer)
  }

  /** 握手完成：退避清零。 */
  handshakeDone(peer: string): void {
    const h = this.handshakes.get(peer)
    if (h?.timer) clearTimeout(h.timer)
    this.handshakes.delete(peer)
  }

  // ---- 清理 ----

  /**
   * 该对端的传输作废了：它名下没发出去的（含握手）、已发出去但被退回的信封都不能再用（密文属于旧传输）。
   * 已发出去的释放原文、留墓碑（晚到的退回据此忽略，不会去重置新传输）；延后中的握手重发也取消（握手已经放弃）。
   */
  discardPeer(to: string): void {
    const p = this.peers.get(to)
    if (p?.timer) clearTimeout(p.timer)
    this.peers.delete(to)
    const now = Date.now()
    for (const [seq, e] of this.sentLog) if (e.to === to) this.retire(seq, e, now)
    this.outbox = this.outbox.filter((i) => i.to !== to)
    const h = this.handshakes.get(to)
    if (h) {
      if (h.timer) clearTimeout(h.timer)
      h.timer = null
      h.seq = -1 // 退避的轮数留着（刚被限速就重新握手，不该从头算）；但在途的握手已经放弃
    }
    this.wake()
  }

  /** hub 连接断了 / 客户端停了：全部丢弃。 */
  clear(): void {
    for (const p of this.peers.values()) if (p.timer) clearTimeout(p.timer)
    for (const h of this.handshakes.values()) if (h.timer) clearTimeout(h.timer)
    this.peers.clear()
    this.handshakes.clear()
    this.handshakeSeqs.clear()
    if (this.drainTimer) clearTimeout(this.drainTimer)
    this.drainTimer = null
    this.windowHeld = false
    this.outbox = []
    this.sentLog.clear()
    this.retired.clear()
    this.logBytes = 0
    this.rateUntil = 0
  }

  // ---- 测试 / 诊断 ----

  get queued(): number {
    return this.outbox.length
  }
  get loggedBytes(): number {
    return this.logBytes
  }
  isBlocked(to: string): boolean {
    return this.peers.get(to)?.blocked ?? false
  }
}
