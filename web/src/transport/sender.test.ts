// 发送方规则（docs/protocol.md § 3.1）：用一个只实现「转发 / 退回并粘住」的假 hub 驱动 EnvelopeSender，
// 断言重发顺序、暂停范围、缓冲上限和最终到达顺序。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { EnvelopeSender, HANDSHAKE_BACKOFF_CAP_MS, SENT_LOG_MAX_BYTES, SENT_RETAIN_MS, STUCK_RESET_MS } from './sender'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)

/** 信封字节：前 4 字节是 seq，后面是填充。 */
function bytesOf(seq: number, size = 16): Uint8Array {
  const b = new Uint8Array(size)
  new DataView(b.buffer).setUint32(0, seq)
  return b
}
const seqOf = (b: Uint8Array): number => new DataView(b.buffer, b.byteOffset).getUint32(0)

/**
 * 真 hub 的粘住规则：转发，或退回 `backpressure`；一个 seq 被退回后，同一 (from, to) 的其它信封一律退回，
 * 直到它的重发被转发，然后轮到下一个被退回的 seq。`capacity`：目标连接的发送缓冲能放几封（`read()` 腾出位置）。
 */
class StickyHub {
  /** 到达目标的顺序（= 接收方看到的顺序）。 */
  forwarded: number[] = []
  /** 所有到达 hub 的 data 信封（含被退回的），按到达顺序。 */
  arrivals: number[] = []
  returned: number[] = []
  queued = 0
  inFlight: Array<() => void> = []
  sender!: EnvelopeSender
  constructor(public capacity: number) {}

  receive(bytes: Uint8Array, to = A): void {
    const seq = seqOf(bytes)
    this.arrivals.push(seq)
    const expected = this.returned[0]
    if (expected !== undefined) {
      if (seq !== expected) {
        if (!this.returned.includes(seq)) this.returned.push(seq)
        return this.bounce(to, seq)
      }
      if (this.queued >= this.capacity) return this.bounce(to, seq) // 重发了，但缓冲还是满的：继续粘住
      this.returned.shift()
    } else if (this.queued >= this.capacity) {
      this.returned.push(seq)
      return this.bounce(to, seq)
    }
    this.queued += 1
    this.forwarded.push(seq)
  }

  /** 回执在路上：发送方已经把整批写完了才收到。 */
  private bounce(to: string, seq: number): void {
    this.inFlight.push(() => this.sender.onBackpressure(to, seq))
  }

  /** 目标连接读走 n 封，腾出缓冲。 */
  read(n = Infinity): void {
    this.queued = Math.max(0, this.queued - n)
  }

  /** 把在路上的回执投递给发送方。 */
  deliver(): void {
    const batch = this.inFlight
    this.inFlight = []
    for (const f of batch) f()
  }
}

function setup(opts: { capacity?: number; maxLogBytes?: number; rng?: () => number } = {}) {
  const hub = new StickyHub(opts.capacity ?? 1000)
  const resets: Array<[string, string]> = []
  let writable = true
  const failTo = new Set<string>()
  const writes: number[] = []
  /** 发送方叫链路重发握手的时刻（`Date.now()`，假定时器下可控）。 */
  const retries: Array<{ to: string; at: number }> = []
  const sender = new EnvelopeSender({
    write: (b) => {
      if (!writable || failTo.has(destinations.get(seqOf(b)) ?? A)) return false
      writes.push(seqOf(b))
      hub.receive(b, destinations.get(seqOf(b)) ?? A)
      return true
    },
    resetPeer: (to, reason) => {
      resets.push([to, reason])
      sender.discardPeer(to)
    },
    retryHandshake: (to) => {
      retries.push({ to, at: Date.now() })
    },
    rng: opts.rng ?? (() => 0.5), // data 重发前等 100 + 0.5 × 200 = 200 ms；握手重发第一轮等 300 + 0.5 × 700 = 650 ms
    maxLogBytes: opts.maxLogBytes,
  })
  hub.sender = sender
  const destinations = new Map<number, string>()
  return {
    hub,
    sender,
    resets,
    writes,
    retries,
    failTo,
    setWritable: (v: boolean) => (writable = v),
    send(seq: number, to = A, kind: 'data' | 'handshake' = 'data', size = 16): void {
      destinations.set(seq, to)
      sender.send(seq, to, kind, bytesOf(seq, size))
    },
  }
}

beforeEach(() => vi.useFakeTimers())
afterEach(() => vi.useRealTimers())

describe('EnvelopeSender — backpressure', () => {
  it('pauses the peer, holds new envelopes, then resends every returned seq in ascending order and releases the held ones', async () => {
    const t = setup({ capacity: 3 })
    for (let seq = 1; seq <= 8; seq++) t.send(seq) // 1–3 转发，4–8 被退回（4 是粘住的那封）
    expect(t.hub.forwarded).toEqual([1, 2, 3])
    t.hub.deliver() // 5 封退回的回执到了
    expect(t.sender.isBlocked(A)).toBe(true)

    // 暂停期间的新信封不写出去
    const writesBefore = t.writes.length
    t.send(9)
    t.send(10)
    expect(t.writes.length).toBe(writesBefore)
    expect(t.sender.queued).toBe(2)

    // 等 100–300 ms 之前不会重发
    await vi.advanceTimersByTimeAsync(150)
    expect(t.writes.length).toBe(writesBefore)

    // 目标读走了缓冲里的东西，然后到点重发：4..8 升序原样重发，9、10 在它们之后
    t.hub.read()
    await vi.advanceTimersByTimeAsync(100)
    t.hub.deliver()
    expect(t.writes.slice(writesBefore, writesBefore + 5)).toEqual([4, 5, 6, 7, 8]) // 全部被退回的 seq，升序
    // 容量只有 3：4、5、6 转发，7 重发又被退回……继续下一轮
    for (let round = 0; round < 10 && t.hub.forwarded.length < 10; round++) {
      t.hub.read()
      await vi.advanceTimersByTimeAsync(300)
      t.hub.deliver()
    }
    t.hub.read()
    await vi.advanceTimersByTimeAsync(300)
    expect(t.hub.forwarded).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]) // 接收方看到的永远是升序，一封不少
    expect(t.resets).toEqual([])
    expect(t.sender.isBlocked(A)).toBe(false)
  })

  it('keeps the hub-side order when the resend itself is bounced again (another round, still ascending)', async () => {
    const t = setup({ capacity: 2 })
    for (let seq = 1; seq <= 6; seq++) t.send(seq)
    t.hub.deliver()
    const roundSizes: number[] = []
    for (let round = 0; round < 12 && t.hub.forwarded.length < 6; round++) {
      const before = t.writes.length
      await vi.advanceTimersByTimeAsync(250)
      roundSizes.push(t.writes.length - before)
      t.hub.deliver()
      t.hub.read(1) // 目标读得很慢：每轮只腾出一个位置
    }
    expect(t.hub.forwarded).toEqual([1, 2, 3, 4, 5, 6])
    // 每一轮写出去的都是升序的一批（不是单封重发）
    const resends = t.writes.slice(6)
    expect(resends.length).toBeGreaterThan(4)
    expect(t.resets).toEqual([])
  })

  it('only pauses the peer that was bounced; other peers and handshakes keep flowing', () => {
    const t = setup({ capacity: 1 })
    t.send(1, A)
    t.send(2, A) // A 被退回
    t.hub.deliver()
    expect(t.sender.isBlocked(A)).toBe(true)
    const before = t.writes.length
    t.send(3, B) // 另一个对端：照发
    t.send(4, A, 'handshake') // 握手信封不受粘住规则管：照发
    t.send(5, A) // A 的新 data：排队
    expect(t.writes.slice(before)).toEqual([3, 4])
    expect(t.sender.queued).toBe(1)
  })

  it('does not resend (or reset) for a seq whose transport was already discarded', async () => {
    const t = setup({ capacity: 1 })
    t.send(1)
    t.send(2)
    t.send(3)
    t.sender.discardPeer(A) // 传输重置了：1、2、3 的密文属于旧传输
    const before = t.writes.length
    t.hub.deliver() // 2、3 的回执晚到
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.writes.length).toBe(before)
    expect(t.resets).toEqual([])
    expect(t.sender.isBlocked(A)).toBe(false)
  })

  it('discardPeer drops what is queued for that peer so it cannot be sent after the new handshake', async () => {
    const t = setup({ capacity: 1 })
    t.send(1)
    t.send(2)
    t.hub.deliver()
    t.send(3)
    t.send(4, B)
    t.sender.discardPeer(A)
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.writes).not.toContain(3)
    expect(t.writes).toContain(4)
  })

  it('an undeliverable seq is not resent, and does not leave the peer blocked', async () => {
    const t = setup({ capacity: 1 })
    t.send(1)
    t.send(2)
    t.hub.deliver()
    t.sender.onUndeliverable(2) // 对端下线
    const before = t.writes.length
    await vi.advanceTimersByTimeAsync(500)
    expect(t.writes.length).toBe(before) // 没有重发 2
    expect(t.sender.isBlocked(A)).toBe(false)
    t.send(3) // 放行了
    expect(t.writes.at(-1)).toBe(3)
  })

  it('resets the peer when a bounce arrives so late that the seq has already aged out of the resend buffer (> 10 s)', async () => {
    const t = setup({ capacity: 1000, maxLogBytes: 100 })
    t.send(1, A, 'data', 40)
    t.send(2, A, 'data', 40)
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS + 1000)
    t.send(3, B, 'data', 40) // 窗口放不下了：1 过了保护期，被逐出
    t.send(4, B, 'data', 40) // 2 也是
    expect(t.writes).toEqual([1, 2, 3, 4])
    t.sender.onBackpressure(A, 2) // 回执晚了 10 s 才到：原文已经不在
    expect(t.resets.map(([to]) => to)).toEqual([A])
    expect(t.resets[0]![1]).toMatch(/no longer in the resend buffer/)
  })

  it('keeps hundreds of small envelopes (the buffer is capped by bytes, not by count) and the default window is at least 8 MiB', () => {
    expect(SENT_LOG_MAX_BYTES).toBeGreaterThanOrEqual(8 * 1024 * 1024)
    const many = setup({ capacity: 1_000_000 })
    for (let seq = 1; seq <= 1500; seq++) many.send(seq) // 以前最多留 512 条
    expect(many.sender.lookup(1)).toBeDefined()
    expect(many.sender.lookup(1500)).toBeDefined()
    expect(many.sender.queued).toBe(0)
  })

  it('retains the resend buffer as an in-flight window: when it is full new data waits instead of evicting anything younger than 10 s', async () => {
    const t = setup({ capacity: 1_000_000, maxLogBytes: 1000 })
    for (let seq = 1; seq <= 10; seq++) t.send(seq, A, 'data', 300)
    // 窗口 1000 字节放得下 3 封；其余在 outbox 里等，已发出的也没被逐出
    expect(t.writes).toEqual([1, 2, 3])
    expect(t.sender.queued).toBe(7)
    expect(t.sender.loggedBytes).toBe(900)
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS - 1000)
    expect(t.writes).toEqual([1, 2, 3])
    for (const seq of [1, 2, 3]) expect(t.sender.lookup(seq), `seq ${seq}`).toEqual({ to: A, dead: false })
    // 过了保护期才逐出最老的，腾出的位置给排队的（4、5、6），7 又要等它们过保护期
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.writes).toEqual([1, 2, 3, 4, 5, 6])
    expect(t.sender.queued).toBe(4)
    expect(t.sender.lookup(1)).toBeUndefined()
    expect(t.sender.lookup(6)).toBeDefined()
    expect(t.sender.loggedBytes).toBeLessThanOrEqual(1000)
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS)
    expect(t.writes).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9])
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS)
    expect(t.writes).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]) // 一封不少，顺序不变
    expect(t.resets).toEqual([])
  })

  it('a bounce for an envelope sent a moment ago finds its original even though newer data is waiting for window space', async () => {
    const t = setup({ capacity: 1000, maxLogBytes: 100 })
    t.send(1, A, 'data', 40)
    t.send(2, A, 'data', 40)
    t.send(3, B, 'data', 40) // 窗口放不下：等着，而不是把 1 或 2 挤掉
    expect(t.writes).toEqual([1, 2])
    expect(t.sender.queued).toBe(1)
    t.sender.onBackpressure(A, 2) // 2 被退回
    expect(t.resets).toEqual([])
    await vi.advanceTimersByTimeAsync(250)
    expect(t.writes).toEqual([1, 2, 2]) // 原样重发
    expect(t.sender.queued).toBe(1) // 3 还在等：1、2 都没过保护期
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS)
    expect(t.writes).toEqual([1, 2, 2, 3])
    expect(t.resets).toEqual([])
  })

  it('never evicts an envelope that is waiting to be resent, however old it is', async () => {
    const t = setup({ capacity: 1000, maxLogBytes: 100 })
    t.send(1, A, 'data', 40)
    t.send(2, A, 'data', 40)
    t.failTo.add(A) // A 的重发写不进去：2 一直在「等重发」
    t.sender.onBackpressure(A, 2)
    await vi.advanceTimersByTimeAsync(SENT_RETAIN_MS + 1000) // 1、2 都过了保护期
    t.send(3, B, 'data', 40) // 要位置：逐出 1，但 2 不能动
    expect(t.writes).toEqual([1, 2, 3])
    expect(t.sender.lookup(1)).toBeUndefined()
    expect(t.sender.lookup(2)).toBeDefined()
    t.send(4, B, 'data', 40) // 再要位置：只剩 2（等重发）和 3（刚发），没有可逐出的 → 等
    expect(t.writes).toEqual([1, 2, 3])
    expect(t.sender.queued).toBe(1)
    expect(t.resets).toEqual([])
    t.failTo.delete(A)
    await vi.advanceTimersByTimeAsync(500) // 2 终于重发出去
    expect(t.writes.slice(3)).toEqual([2])
    expect(t.resets).toEqual([])
  })

  it('sends an envelope bigger than the whole window when nothing else is in flight', () => {
    const t = setup({ capacity: 1000, maxLogBytes: 100 })
    t.send(1, A, 'data', 500)
    expect(t.writes).toEqual([1])
    t.send(2, A, 'data', 10) // 窗口被那一封占满
    expect(t.writes).toEqual([1])
  })

  it('discarding a peer releases its originals at once (frees the window) but still recognises its late bounces', async () => {
    const t = setup({ capacity: 1000, maxLogBytes: 1000 })
    for (let seq = 1; seq <= 3; seq++) t.send(seq, A, 'data', 300)
    t.send(4, B, 'data', 300) // 窗口满了，等着
    expect(t.writes).toEqual([1, 2, 3])
    t.sender.discardPeer(A) // 传输重置：1–3 的密文作废
    expect(t.sender.loggedBytes).toBe(0)
    expect(t.sender.lookup(2)).toEqual({ to: A, dead: true })
    await vi.advanceTimersByTimeAsync(0)
    expect(t.writes).toEqual([1, 2, 3, 4]) // 腾出的窗口马上给了 4
    t.sender.onBackpressure(A, 2) // 晚到的退回：忽略，不重置新传输
    t.sender.onBackpressure(A, 3)
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.resets).toEqual([])
    expect(t.sender.isBlocked(A)).toBe(false)
    expect(t.writes).toEqual([1, 2, 3, 4])
  })

  it('forgetting a seq (undeliverable) frees window space for waiting envelopes', async () => {
    const t = setup({ capacity: 1000, maxLogBytes: 600 })
    t.send(1, A, 'data', 300)
    t.send(2, A, 'data', 300)
    t.send(3, A, 'data', 300)
    expect(t.writes).toEqual([1, 2])
    t.sender.onUndeliverable(1)
    await vi.advanceTimersByTimeAsync(0)
    expect(t.writes).toEqual([1, 2, 3])
  })

  it('resets the peer if the hub keeps bouncing it for a minute', async () => {
    const t = setup({ capacity: 0 }) // 目标永远不读
    t.send(1)
    let guard = 0
    while (t.resets.length === 0 && guard++ < 400) {
      t.hub.deliver()
      await vi.advanceTimersByTimeAsync(250)
    }
    expect(t.resets).toHaveLength(1)
    expect(t.resets[0]![1]).toMatch(/kept returning/)
    expect(guard * 250).toBeGreaterThanOrEqual(STUCK_RESET_MS)
  })

  it('retries later when the socket cannot be written at resend time', async () => {
    const t = setup({ capacity: 1 })
    t.send(1)
    t.send(2)
    t.hub.deliver()
    t.setWritable(false)
    await vi.advanceTimersByTimeAsync(250) // 到点了但写不进去
    expect(t.sender.isBlocked(A)).toBe(true)
    t.setWritable(true)
    t.hub.read()
    await vi.advanceTimersByTimeAsync(300)
    expect(t.hub.forwarded).toEqual([1, 2])
  })
})

describe('EnvelopeSender — rate_limited', () => {
  it('pauses all sending until retry_after_ms has passed, then flushes in seq order', async () => {
    const t = setup()
    t.sender.onRateLimited(1500)
    t.send(1)
    t.send(2, B)
    t.send(3, A, 'handshake')
    expect(t.writes).toEqual([])
    await vi.advanceTimersByTimeAsync(1400)
    expect(t.writes).toEqual([])
    await vi.advanceTimersByTimeAsync(200)
    expect(t.writes).toEqual([1, 2, 3])
  })

  it('waits for retry_after_ms (not just the default 100–300 ms) before resending a returned seq', async () => {
    const t = setup({ capacity: 1 })
    t.send(1)
    t.send(2)
    t.hub.deliver() // backpressure(2)
    t.sender.onRateLimited(2000) // 伴随出现的 rate_limited
    t.hub.read()
    const before = t.writes.length
    await vi.advanceTimersByTimeAsync(1900)
    expect(t.writes.length).toBe(before) // 默认的 200 ms 早过了，但还在 rate_limited 的窗口里
    await vi.advanceTimersByTimeAsync(200)
    expect(t.writes.slice(before)).toEqual([2])
    expect(t.hub.forwarded).toEqual([1, 2])
  })

  it('ignores an absurd retry_after_ms (at most 30 s)', async () => {
    const t = setup()
    t.sender.onRateLimited(3_600_000)
    t.send(1)
    await vi.advanceTimersByTimeAsync(31_000)
    expect(t.writes).toEqual([1])
  })

  it('forgets the pause when the connection is cleared', () => {
    const t = setup()
    t.sender.onRateLimited(5000)
    t.sender.clear()
    t.send(1)
    expect(t.writes).toEqual([1])
  })
})

describe('EnvelopeSender — handshake returned by the hub', () => {
  /** 发一封握手、让 hub 退回它，返回重发握手的时刻相对退回时刻晚了多久。 */
  async function bounceRound(t: ReturnType<typeof setup>, seq: number, to = A): Promise<number> {
    t.send(seq, to, 'handshake')
    const before = t.retries.length
    const at = Date.now()
    t.sender.onBackpressure(to, seq)
    expect(t.resets).toEqual([])
    expect(t.retries.length).toBe(before) // 不立即重发
    for (let waited = 0; waited <= HANDSHAKE_BACKOFF_CAP_MS + 1000; waited += 10) {
      if (t.retries.length > before) return t.retries[before]!.at - at
      await vi.advanceTimersByTimeAsync(10)
    }
    throw new Error('the handshake was never retried')
  }

  it('does not resend or reset right away: it asks for the handshake again after 300–1000 ms', async () => {
    const t = setup()
    t.send(1, A, 'handshake')
    t.sender.onBackpressure(A, 1)
    expect(t.resets).toEqual([])
    expect(t.retries).toEqual([])
    await vi.advanceTimersByTimeAsync(640)
    expect(t.retries).toEqual([]) // rng = 0.5 → 650 ms
    await vi.advanceTimersByTimeAsync(20)
    expect(t.retries.map((r) => r.to)).toEqual([A])
    expect(t.resets).toEqual([])
    // 范围的两端
    expect(await bounceRound(setup({ rng: () => 0 }), 1)).toBe(300)
    expect(await bounceRound(setup({ rng: () => 0.9999 }), 1)).toBeGreaterThanOrEqual(999)
  })

  it('doubles the wait on every consecutive round and caps it at 30 s', async () => {
    const t = setup()
    const waits: number[] = []
    for (let seq = 1; seq <= 9; seq++) waits.push(await bounceRound(t, seq))
    expect(waits.slice(0, 6)).toEqual([650, 1300, 2600, 5200, 10_400, 20_800])
    expect(waits.slice(6)).toEqual([HANDSHAKE_BACKOFF_CAP_MS, HANDSHAKE_BACKOFF_CAP_MS, HANDSHAKE_BACKOFF_CAP_MS])
    expect(t.resets).toEqual([])
  })

  it('starts over from the first step once the handshake completes, or after a long quiet period', async () => {
    const t = setup()
    expect(await bounceRound(t, 1)).toBe(650)
    expect(await bounceRound(t, 2)).toBe(1300)
    t.sender.handshakeDone(A)
    expect(await bounceRound(t, 3)).toBe(650)
    expect(await bounceRound(t, 4)).toBe(1300)
    await vi.advanceTimersByTimeAsync(61_000) // 安静了一阵
    expect(await bounceRound(t, 5)).toBe(650)
  })

  it('follows rate_limited: waits for retry_after_ms instead of the default step, whichever message arrives first', async () => {
    // rate_limited 先到
    const a = setup()
    a.sender.onRateLimited(1500)
    a.send(1, A, 'handshake') // 被 rate_limited 暂停，排队
    expect(a.writes).toEqual([])
    await vi.advanceTimersByTimeAsync(1500 + 1)
    expect(a.writes).toEqual([1])
    a.sender.onRateLimited(1500) // hub 又说了一次
    a.sender.onBackpressure(A, 1)
    await vi.advanceTimersByTimeAsync(1600)
    expect(a.retries).toEqual([]) // 1500 + 抖动 200 = 1700 ms
    await vi.advanceTimersByTimeAsync(150)
    expect(a.retries).toHaveLength(1)

    // backpressure 先到、rate_limited 晚几毫秒：到点时发现还在窗口里，继续等
    const b = setup()
    b.send(1, A, 'handshake')
    b.sender.onBackpressure(A, 1) // 650 ms 后到点
    await vi.advanceTimersByTimeAsync(10)
    b.sender.onRateLimited(3000)
    await vi.advanceTimersByTimeAsync(3000)
    expect(b.retries).toEqual([])
    await vi.advanceTimersByTimeAsync(300)
    expect(b.retries).toHaveLength(1)
    expect(b.retries[0]!.at).toBeGreaterThanOrEqual(3010)
    expect(b.resets).toEqual([])
  })

  it('a repeat bounce while a retry is already waiting only refreshes the timer: one retry, and no extra doubling', async () => {
    const t = setup()
    t.send(1, A, 'handshake')
    t.sender.onBackpressure(A, 1)
    await vi.advanceTimersByTimeAsync(300)
    t.sender.onBackpressure(A, 1) // 又来一次：计时从现在重新算
    await vi.advanceTimersByTimeAsync(600)
    expect(t.retries).toEqual([]) // 300 + 600 = 900 < 300 + 650
    await vi.advanceTimersByTimeAsync(100)
    expect(t.retries).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(40_000)
    expect(t.retries).toHaveLength(1) // 没有多余的重试
    // 重试后再被退回：这是第二轮（1300），不是第三轮
    expect(await bounceRound(t, 2)).toBe(1300)
  })

  it('ignores the bounce of a handshake that has been replaced by a newer one', async () => {
    const t = setup()
    t.send(1, A, 'handshake')
    t.send(2, A, 'handshake') // 新的握手取代了 1
    t.sender.onBackpressure(A, 1)
    await vi.advanceTimersByTimeAsync(5000)
    expect(t.retries).toEqual([])
    expect(t.resets).toEqual([])
    t.sender.onBackpressure(A, 2)
    await vi.advanceTimersByTimeAsync(700)
    expect(t.retries).toHaveLength(1)
  })

  it('abandoning the handshake (discardPeer) cancels the pending retry and ignores the late bounce', async () => {
    const t = setup()
    t.send(1, A, 'handshake')
    t.sender.onBackpressure(A, 1)
    t.sender.discardPeer(A)
    await vi.advanceTimersByTimeAsync(5000)
    expect(t.retries).toEqual([])
    t.send(2, A, 'handshake')
    t.sender.discardPeer(A)
    t.sender.onBackpressure(A, 2) // 握手已经放弃，它的退回晚到
    await vi.advanceTimersByTimeAsync(5000)
    expect(t.retries).toEqual([])
    expect(t.resets).toEqual([])
  })

  it('discardPeer also drops a handshake that is still queued behind a rate_limited pause', async () => {
    const t = setup()
    t.sender.onRateLimited(2000)
    t.send(1, A, 'handshake')
    t.sender.discardPeer(A)
    await vi.advanceTimersByTimeAsync(3000)
    expect(t.writes).toEqual([])
  })

  it('reports the handshake as held while a retry is pending or while it waits in the outbox', async () => {
    const t = setup()
    expect(t.sender.isHandshakeHeld(A)).toBe(false)
    t.send(1, A, 'handshake')
    expect(t.sender.isHandshakeHeld(A)).toBe(false) // 已经写出去了
    t.sender.onBackpressure(A, 1)
    expect(t.sender.isHandshakeHeld(A)).toBe(true)
    await vi.advanceTimersByTimeAsync(700)
    expect(t.sender.isHandshakeHeld(A)).toBe(false) // 链路正在重发
    t.sender.onRateLimited(2000)
    t.send(2, A, 'handshake')
    expect(t.sender.isHandshakeHeld(A)).toBe(true) // 在 outbox 里等 rate_limited
    expect(t.sender.isHandshakeHeld(B)).toBe(false)
    await vi.advanceTimersByTimeAsync(2100)
    expect(t.sender.isHandshakeHeld(A)).toBe(false)
  })

  it('does not disturb data: a data seq is still resent verbatim, and an unknown seq still resets the peer', async () => {
    const t = setup({ capacity: 1 })
    t.send(1, A, 'handshake')
    t.send(2, A)
    t.send(3, A)
    t.hub.deliver()
    t.sender.onBackpressure(A, 1) // 握手的退回
    await vi.advanceTimersByTimeAsync(700)
    expect(t.retries).toHaveLength(1)
    t.sender.onBackpressure(A, 999_999) // 从没发过的 seq
    expect(t.resets).toHaveLength(1)
  })
})
