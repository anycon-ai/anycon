// 粘住规则（docs/protocol.md § 3.1）在 AnyconClient 层面的行为：假 hub 实现真 hub 的「粘住」规则（退回 seq N 之后，同一 (from, to) 的信封全部退回，
// 直到 N 重发成功），假 daemon 的假 Noise 传输校验 nonce 顺序——只要有一片乱序到达就会解密失败。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DAEMON_ID, FakeHub, FakeSocket, setupClient } from '@/test/harness'

async function ready() {
  const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }), echo: (p) => p } })
  const t = setupClient({ hub })
  await t.client.start('acct1')
  await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
  return { ...t, hub }
}

const FAKE = ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'Date'] as const

beforeEach(() => {
  FakeSocket.instances = []
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  vi.restoreAllMocks()
  vi.useRealTimers()
})

describe('AnyconClient — the hub returns envelopes (sticky rule)', () => {
  it('pauses, holds new requests, resends the returned fragments in ascending order, and the daemon never sees a gap', async () => {
    const t = await ready()
    vi.useFakeTimers({ toFake: [...FAKE] })
    t.hub.sticky = { capacity: 2 }
    const big = 'z'.repeat(900_000) // 5 个 200 KiB 的分片
    const req = t.client.request(DAEMON_ID, 'echo', { big }, 120_000)
    await vi.advanceTimersByTimeAsync(0)
    // 前两片转发，后三片被退回（第 3 片粘住，4、5 跟着被退回）
    expect(t.hub.forwardedSeqs).toHaveLength(2)
    expect(t.hub.backpressured).toHaveLength(3)

    // 暂停期间发出的另一个请求排队，不写出去（写出去也只会被退回）
    const arrived = t.hub.arrivedSeqs.length
    const info = t.client.request(DAEMON_ID, 'node.info', {}, 120_000)
    await vi.advanceTimersByTimeAsync(50)
    expect(t.hub.arrivedSeqs.length).toBe(arrived)

    // 目标读走缓冲，到点后升序重发
    for (let i = 0; i < 30 && t.hub.forwardedSeqs.length < 6; i++) {
      t.hub.stickyRead()
      await vi.advanceTimersByTimeAsync(250)
    }
    await expect(req).resolves.toEqual({ big })
    await expect(info).resolves.toEqual({ name: 'n' })

    const seqs = t.hub.forwardedSeqs
    expect(seqs).toHaveLength(6) // 5 片 + 1 个小请求，一封不少、没有重复
    expect(seqs).toEqual([...seqs].sort((a, b) => a - b)) // 接收方看到的是升序
    expect(t.hub.decryptFailures).toBe(0)
    expect(t.hub.ikHandshakes).toBe(1) // 没有重置传输
    // 重发的是同一批字节
    for (const bounced of t.hub.backpressured) {
      const again = t.hub.dataFrames.find((e) => e.seq === bounced.seq)
      expect(again && Array.from(again.body)).toEqual(Array.from(bounced.body))
    }
    t.client.stop()
  })

  it('a hub that returns "seq N and everything after it until N is resent" never makes the daemon desync, however slowly it reads', async () => {
    const t = await ready()
    vi.useFakeTimers({ toFake: [...FAKE] })
    t.hub.sticky = { capacity: 1 }
    const requests = Array.from({ length: 6 }, (_, i) => t.client.request(DAEMON_ID, 'echo', { i, pad: 'p'.repeat(1000) }, 120_000))
    for (let i = 0; i < 40 && t.hub.forwardedSeqs.length < 6; i++) {
      await vi.advanceTimersByTimeAsync(120)
      t.hub.stickyRead(1) // 每轮只读走一封
    }
    const results = await Promise.all(requests)
    expect(results.map((r) => (r as { i: number }).i)).toEqual([0, 1, 2, 3, 4, 5])
    expect(t.hub.forwardedSeqs).toEqual([...t.hub.forwardedSeqs].sort((a, b) => a - b))
    expect(t.hub.decryptFailures).toBe(0)
    expect(t.hub.ikHandshakes).toBe(1)
    t.client.stop()
  })

  it('waits for retry_after_ms from rate_limited before resending', async () => {
    const t = await ready()
    vi.useFakeTimers({ toFake: [...FAKE] })
    t.hub.sticky = { capacity: 1, rateLimitedMs: 1500 }
    const first = t.client.request(DAEMON_ID, 'node.info', {}, 120_000)
    const second = t.client.request(DAEMON_ID, 'node.info', {}, 120_000) // 被退回 + rate_limited
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hub.backpressured).toHaveLength(1)
    const arrived = t.hub.arrivedSeqs.length
    t.hub.stickyRead()
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.hub.arrivedSeqs.length).toBe(arrived) // 默认的 100–300 ms 早过了，但 rate_limited 要求停 1.5 s
    await vi.advanceTimersByTimeAsync(700)
    expect(t.hub.arrivedSeqs.length).toBe(arrived + 1)
    await expect(Promise.all([first, second])).resolves.toEqual([{ name: 'n' }, { name: 'n' }])
    t.client.stop()
  })

  it('rebuilds the transport when the hub returns an envelope that is not in the resend buffer', async () => {
    const t = await ready()
    expect(t.hub.ikHandshakes).toBe(1)
    t.hub.socket.control({ type: 'backpressure', to: DAEMON_ID, seq: 987_654 })
    await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(2))
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('never sends envelopes that were queued or returned on a transport that has since been reset', async () => {
    const t = await ready()
    vi.useFakeTimers({ toFake: [...FAKE] })
    t.hub.sticky = { capacity: 1 }
    const ok = t.client.request(DAEMON_ID, 'node.info', {}, 120_000)
    const bounced = t.client.request(DAEMON_ID, 'node.info', {}, 120_000)
    const held = t.client.request(DAEMON_ID, 'node.info', {}, 120_000)
    const failures = [bounced, held].map((p) => p.then(() => null, (e: unknown) => e))
    await vi.advanceTimersByTimeAsync(0)
    await expect(ok).resolves.toEqual({ name: 'n' })
    const staleSeqs = [...t.hub.arrivedSeqs]
    // daemon 发 RESET：旧传输作废，排队的、被退回的都不能再发
    t.hub.daemonSendReset()
    await vi.advanceTimersByTimeAsync(0)
    t.hub.stickyRead()
    await vi.advanceTimersByTimeAsync(2000)
    expect(await Promise.all(failures)).toEqual([expect.objectContaining({ kind: 'closed' }), expect.objectContaining({ kind: 'closed' })])
    expect(t.hub.ikHandshakes).toBe(2)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    // 重新握手之后到达 hub 的 data 信封一个也不是旧传输上的（它们的 seq 都在 staleSeqs 里，不该再出现第二次）
    const later = t.hub.arrivedSeqs.slice(staleSeqs.length)
    expect(later.filter((s) => staleSeqs.includes(s))).toEqual([])
    await expect(t.client.request(DAEMON_ID, 'node.info', {}, 120_000)).resolves.toEqual({ name: 'n' })
    expect(t.hub.decryptFailures).toBe(0)
    t.client.stop()
  })
})

describe('AnyconClient — the hub returns the handshake (rate limit)', () => {
  /** 相邻两次握手到达 hub 的间隔。 */
  const gaps = (arrivals: number[]): number[] => arrivals.slice(1).map((at, i) => at - arrivals[i]!)

  /** 假定时器下启动，一路推进到握手成功（或 `limitMs` 用完）。 */
  async function startAndWaitReady(t: ReturnType<typeof setupClient>, limitMs: number): Promise<void> {
    void t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    for (let waited = 0; waited < limitMs && !t.client.isReady(DAEMON_ID); waited += 50) {
      // 等待期间节点一直是「握手中」，从不会被误判成 daemon 不认识我们
      expect(t.client.nodeConn(DAEMON_ID)).not.toBe('noise_failed')
      await vi.advanceTimersByTimeAsync(50)
    }
  }

  it('retries only after a delay that doubles each round, never resets the transport, is not cut off with 1008, and finally completes the handshake', async () => {
    vi.useFakeTimers({ toFake: [...FAKE] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.bounceHandshakes = { remaining: 6 }
    const t = setupClient({ hub })
    await startAndWaitReady(t, 120_000)

    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    expect(hub.handshakeBounces).toBe(6)
    expect(hub.handshakeArrivals).toHaveLength(7) // 6 次被退回 + 1 次成功，没有多发
    // 每次都是延后再来：650、1300、2600、5200、10400、20800 ms（300–1000 ms 的抖动起步，每轮翻倍）
    expect(gaps(hub.handshakeArrivals)).toEqual([650, 1300, 2600, 5200, 10_400, 20_800])
    expect(hub.closed1008).toBe(false)
    expect(FakeSocket.instances).toHaveLength(1) // 连接始终没断过
    expect(FakeSocket.instances[0]!.closedWith).toBeNull()
    expect(t.nodeStates.some((s) => s.conn === 'noise_failed')).toBe(false)
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    expect(hub.decryptFailures).toBe(0)
    t.client.stop()
  })

  it('waits for retry_after_ms from rate_limited, however the two messages are ordered', async () => {
    vi.useFakeTimers({ toFake: [...FAKE] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.bounceHandshakes = { remaining: 3, retryAfterMs: 2000 }
    const t = setupClient({ hub })
    await startAndWaitReady(t, 60_000)

    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    expect(hub.handshakeArrivals).toHaveLength(4)
    // 第一、二轮的默认间隔（650、1300 ms）都短于 rate_limited 要求的 2 s，所以改成等到它过去（+ 100–300 ms 抖动）；第三轮翻倍到 2600 ms
    expect(gaps(hub.handshakeArrivals)).toEqual([2200, 2200, 2600])
    expect(hub.closed1008).toBe(false)
    expect(FakeSocket.instances).toHaveLength(1)
    t.client.stop()
  })

  it('does not time the handshake out while it is being held back (40 s of rate limiting is not "the daemon does not know us")', async () => {
    vi.useFakeTimers({ toFake: [...FAKE] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.bounceHandshakes = { remaining: 7, retryAfterMs: 0 } // 650 + 1300 + … + 30000（封顶）：超过 60 s
    const t = setupClient({ hub })
    await startAndWaitReady(t, 300_000)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    expect(Math.max(...gaps(hub.handshakeArrivals))).toBe(30_000) // 封顶
    expect(t.nodeStates.some((s) => s.conn === 'noise_failed')).toBe(false)
    expect(hub.ikHandshakes).toBe(1) // daemon 只见过一次握手
    t.client.stop()
  })

  it('a new peer_online while the handshake is held back neither sends an extra handshake early nor leaves the link stuck', async () => {
    vi.useFakeTimers({ toFake: [...FAKE] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.bounceHandshakes = { remaining: 2 }
    const t = setupClient({ hub })
    void t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    expect(hub.handshakeArrivals).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(1000) // 第一次被退回，第二次（650 ms 后）也被退回，正在等 1300 ms
    expect(hub.handshakeArrivals).toHaveLength(2)
    hub.socket.control({ type: 'peer_online', device: DAEMON_ID }) // daemon 重连
    await vi.advanceTimersByTimeAsync(0)
    expect(hub.handshakeArrivals).toHaveLength(2) // 不抢跑
    await vi.advanceTimersByTimeAsync(1500)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    expect(hub.handshakeArrivals).toHaveLength(3)
    expect(hub.ikHandshakes).toBe(1) // 没有「回复可疑、丢掉重来」的第二次握手
    t.client.stop()
  })
})
