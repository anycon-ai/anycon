// ADR-30：`data` 信封的 body 框架（WHOLE / FRAGMENT / RESET）在 web 传输层的用法。
// 字节格式以 crates/protocol/src/envelope.rs 的 `mod frame` 为准：
//   WHOLE    = 0x00 + 密文
//   FRAGMENT = 0x01 + u32 be msg_id + u16 be index + u16 be count + 密文切片
//   RESET    = 0x02
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DAEMON_ID, FakeHub, FakeSocket, setupClient } from '@/test/harness'
import { MAX_FRAGMENT } from './wire'

const FRAGMENT_HEADER = 9

function dataBodies(): Uint8Array[] {
  return FakeSocket.instances.flatMap((s) => s.sent).filter((e) => e.kind === 'data').map((e) => e.body)
}

function view(b: Uint8Array): DataView {
  return new DataView(b.buffer, b.byteOffset, b.byteLength)
}

async function ready(opts: ConstructorParameters<typeof FakeHub>[0] = {}) {
  const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }), echo: (p) => p, ...(opts.rpc ?? {}) }, ...opts })
  const t = setupClient({ hub })
  await t.client.start('acct1')
  await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
  return { ...t, hub }
}

beforeEach(() => {
  vi.spyOn(console, 'warn').mockImplementation(() => {})
  vi.spyOn(console, 'error').mockImplementation(() => {})
})
afterEach(() => {
  vi.restoreAllMocks()
  vi.useRealTimers()
})

describe('data frames — outbound', () => {
  it('sends a small request as a single WHOLE frame (prefix 0x00)', async () => {
    const t = await ready()
    await t.client.request(DAEMON_ID, 'node.info')
    const bodies = dataBodies()
    expect(bodies).toHaveLength(1)
    expect(bodies[0]![0]).toBe(0x00)
    t.client.stop()
  })

  it('splits a >200 KiB request into FRAGMENT frames with the Rust header layout, and the daemon reassembles it intact', async () => {
    const t = await ready()
    const big = 'x'.repeat(520_000)
    await expect(t.client.request(DAEMON_ID, 'echo', { big })).resolves.toEqual({ big })
    const bodies = dataBodies()
    expect(bodies.length).toBeGreaterThanOrEqual(3)
    const msgIds = new Set<number>()
    bodies.forEach((b, i) => {
      expect(b[0]).toBe(0x01)
      expect(b.length).toBeLessThanOrEqual(FRAGMENT_HEADER + MAX_FRAGMENT)
      const v = view(b)
      msgIds.add(v.getUint32(1, false))
      expect(v.getUint16(5, false)).toBe(i) // index，大端
      expect(v.getUint16(7, false)).toBe(bodies.length) // count，大端
    })
    expect(msgIds.size).toBe(1)
    // 所有信封都在 hub 的上限之内（不会收到 dropped{too_large}）
    expect(t.hub.oversize).toBe(0)
    expect(t.hub.received.some((m) => 'method' in m && m.method === 'echo')).toBe(true)
    t.client.stop()
  })

  it('uses a fresh msg_id (u32, counting up per node) for every fragmented message', async () => {
    const t = await ready()
    const big = 'y'.repeat(300_000)
    await t.client.request(DAEMON_ID, 'echo', { big })
    await t.client.request(DAEMON_ID, 'echo', { big })
    const ids = dataBodies().filter((b) => b[0] === 0x01).map((b) => view(b).getUint32(1, false))
    const distinct = [...new Set(ids)]
    expect(distinct).toHaveLength(2)
    expect(distinct[1]).toBe((distinct[0]! + 1) >>> 0)
    t.client.stop()
  })

  it('resends exactly the same bytes, in order, when the hub answers backpressure for every fragment', async () => {
    const t = await ready()
    t.hub.backpressureOnce = 3
    const big = 'z'.repeat(450_000)
    await expect(t.client.request(DAEMON_ID, 'echo', { big })).resolves.toEqual({ big })
    expect(t.hub.backpressured).toHaveLength(3)
    // 重发的是同一批字节（不重新加密：Noise nonce 是顺序的），顺序不变
    const resent = t.hub.dataFrames.slice(-3).map((e) => e.body)
    t.hub.backpressured.forEach((e, i) => expect(Array.from(resent[i]!)).toEqual(Array.from(e.body)))
    t.client.stop()
  })

  it('fails a request that is far beyond what the peer will reassemble, without sending anything', async () => {
    const t = await ready()
    const before = dataBodies().length
    await expect(t.client.request(DAEMON_ID, 'echo', { big: 'q'.repeat(9 * 1024 * 1024) })).rejects.toMatchObject({ kind: 'too_large' })
    expect(dataBodies().length).toBe(before)
    // 传输仍然可用
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('logs dropped{too_large} as an error, tells the UI, fails the request as undeliverable and re-handshakes (no resend)', async () => {
    const t = await ready()
    t.hub.maxEnvelopeBytes = 500 // 模拟 hub 上限比 WHOLE 帧还小（正常不会发生：分片阈值 200 KiB）
    const p = t.client.request(DAEMON_ID, 'echo', { big: 'a'.repeat(2000) })
    await expect(p).rejects.toMatchObject({ kind: 'undeliverable' })
    expect(console.error).toHaveBeenCalledWith(expect.stringContaining('too large'))
    expect(t.drops).toEqual([['too_large', DAEMON_ID]])
    t.hub.maxEnvelopeBytes = 262_144
    await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(2))
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.hub.oversize).toBe(1) // 只发了一次，没有重发
    t.client.stop()
  })

  it.each(['not_paired', 'rate_limited'] as const)('dropped{%s} is not a resend signal: the request fails, the UI is told, and the node transport is rebuilt', async (reason) => {
    const t = await ready()
    t.hub.dropNextWith = reason
    await expect(t.client.request(DAEMON_ID, 'node.info')).rejects.toMatchObject({ kind: 'undeliverable' })
    expect(t.drops).toEqual([[reason, DAEMON_ID]])
    expect(t.hub.droppedEnvelopes).toHaveLength(1)
    expect(t.hub.dataFrames).toHaveLength(0) // 没有把被丢的那封再发一遍
    await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(2))
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })
})

describe('data frames — inbound', () => {
  it('delivers a 1 MB session.history page intact (fragmented by the daemon, reassembled by the client)', async () => {
    const events = Array.from({ length: 250 }, (_, i) => ({ session_id: 's1', seq: i + 1, ts: i, event: { type: 'text_delta', text: `${i}:` + 'h'.repeat(4000) } }))
    const body = { events, has_more: true }
    expect(JSON.stringify(body).length).toBeGreaterThan(1_000_000)
    const t = await ready({ rpc: { 'session.history': () => body } })
    const before = t.hub.daemonSeq
    const res = await t.client.request<typeof body>(DAEMON_ID, 'session.history', { session_id: 's1', before_seq: 1e15, limit: 250 })
    expect(res.events).toHaveLength(250)
    expect(res).toEqual(body)
    expect(t.hub.daemonSeq - before).toBeGreaterThanOrEqual(5) // 至少 5 个 data 信封
    t.client.stop()
  })

  it('reassembles a fragmented notification too', async () => {
    const t = await ready()
    const big = { text: 'n'.repeat(700_000) }
    t.hub.notify('session.event', big)
    await vi.waitFor(() => expect(t.notes).toEqual([[DAEMON_ID, 'session.event', big]]))
    t.client.stop()
  })

  it('drops the transport and re-runs the IK handshake when the daemon sends RESET', async () => {
    const t = await ready()
    expect(t.hub.ikHandshakes).toBe(1)
    t.hub.daemonSendReset()
    await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(2))
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('rejects in-flight requests when the transport is reset under them', async () => {
    const t = await ready({ rpc: {} })
    const pending = t.client.request(DAEMON_ID, 'nobody.home', {}, 5000)
    t.hub.daemonSendReset()
    await expect(pending).rejects.toMatchObject({ kind: 'closed' })
    t.client.stop()
  })

  it('re-handshakes on a malformed frame, an unknown flag byte and an out-of-order fragment', async () => {
    const t = await ready()
    const inject = (body: Uint8Array): void => t.hub.socket.deliver({ v: 1, from: DAEMON_ID, to: 'c'.repeat(32), kind: 'data', seq: 1, body })
    let handshakes = t.hub.ikHandshakes
    for (const bad of [
      new Uint8Array(0), // 空 body
      Uint8Array.of(0x07, 1, 2, 3), // 未知 flags
      Uint8Array.of(0x01, 0, 0, 0, 1, 0, 1, 0, 2, 9), // 分片：index 1 先到（没有 index 0）
    ]) {
      inject(bad)
      await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(handshakes + 1))
      await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
      handshakes = t.hub.ikHandshakes
    }
    t.client.stop()
  })

  it('re-handshakes when the ciphertext does not decrypt (tampered / lost message)', async () => {
    const t = await ready()
    // WHOLE 帧里装一段解不开的密文
    t.hub.socket.deliver({ v: 1, from: DAEMON_ID, to: 'c'.repeat(32), kind: 'data', seq: 1, body: Uint8Array.of(0x00, 0xe0, 0, 0, 0, 9, 1, 2, 3) })
    await vi.waitFor(() => expect(t.hub.ikHandshakes).toBe(2))
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.client.stop()
  })
})
