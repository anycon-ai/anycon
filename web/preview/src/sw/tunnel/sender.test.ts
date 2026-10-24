import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { TunnelSender } from './sender'

const PEER = 'node-1'

function setup(opts: { writable?: () => boolean } = {}) {
  const written: number[] = []
  const resets: string[] = []
  const sender = new TunnelSender({
    peer: PEER,
    rng: () => 0, // 抖动取下限：data 重发 100 ms，握手 300 ms
    write: (bytes) => {
      if (opts.writable && !opts.writable()) return false
      written.push(bytes[0]!)
      return true
    },
    needReset: (r) => resets.push(r),
  })
  // 第一个字节当 seq，方便断言写出的顺序
  const send = (seq: number, index = seq - 1, size = 10) => sender.sendData(seq, index, Uint8Array.from({ length: size }, (_, i) => (i === 0 ? seq : 0)))
  return { sender, written, resets, send }
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(1_000_000)
})
afterEach(() => vi.useRealTimers())

describe('retention by end-to-end ack', () => {
  it('writes at once, keeps the original until the peer acks, and counts bytes in flight', () => {
    const { sender, written, send } = setup()
    send(1, 0, 100)
    send(2, 1, 50)
    expect(written).toEqual([1, 2])
    expect(sender.inflightBytes).toBe(150)
    sender.onAck(1)
    expect(sender.inflightBytes).toBe(50)
    sender.onAck(1) // 重复的回执不改变什么
    expect(sender.inflightBytes).toBe(50)
    sender.onAck(2)
    expect(sender.inflightBytes).toBe(0)
  })

  it('ignores a bounce that arrives after the envelope was acked', () => {
    const { sender, written, resets, send } = setup()
    send(1)
    send(2)
    sender.onAck(2)
    sender.onBackpressure(2)
    vi.advanceTimersByTime(5_000)
    expect(resets).toEqual([])
    expect(written).toEqual([1, 2])
    send(3)
    expect(written).toEqual([1, 2, 3]) // 没被暂停
  })
})

describe('hub backpressure', () => {
  it('pauses new envelopes, then resends the returned ones in ascending order, unchanged, before the queued ones', () => {
    const { sender, written, send } = setup()
    send(1)
    send(2)
    send(3)
    expect(written).toEqual([1, 2, 3])
    // hub 把 2、3 退回（乱序到达）
    sender.onBackpressure(3)
    sender.onBackpressure(2)
    send(4) // 暂停中：排队
    send(5)
    expect(written).toEqual([1, 2, 3])
    vi.advanceTimersByTime(99)
    expect(written).toEqual([1, 2, 3])
    vi.advanceTimersByTime(1)
    expect(written).toEqual([1, 2, 3, 2, 3, 4, 5])
    // 之后恢复正常
    send(6)
    expect(written.at(-1)).toBe(6)
  })

  it('waits for another round when the resend is bounced again', () => {
    const { sender, written, send } = setup()
    send(1)
    send(2)
    sender.onBackpressure(1)
    sender.onBackpressure(2)
    vi.advanceTimersByTime(100)
    expect(written).toEqual([1, 2, 1, 2])
    sender.onBackpressure(1)
    sender.onBackpressure(2)
    send(3)
    expect(written).toEqual([1, 2, 1, 2])
    vi.advanceTimersByTime(100)
    expect(written).toEqual([1, 2, 1, 2, 1, 2, 3])
  })

  it('keeps the bounced envelope retained until it is acked after the resend', () => {
    const { sender, send } = setup()
    send(1, 0, 40)
    sender.onBackpressure(1)
    vi.advanceTimersByTime(100)
    expect(sender.inflightBytes).toBe(40)
    sender.onAck(1)
    expect(sender.inflightBytes).toBe(0)
  })

  it('does not resend what was acked while waiting', () => {
    const { sender, written, send } = setup()
    send(1)
    send(2)
    sender.onBackpressure(2)
    sender.onAck(2) // 2 号其实被 daemon 收到了（早先的一份）
    vi.advanceTimersByTime(1_000)
    expect(written).toEqual([1, 2])
  })

  it('asks for a transport reset when a returned envelope is gone and was never acked', () => {
    const { sender, resets, send } = setup()
    send(1)
    send(2)
    sender.forget(2) // 例如 undeliverable 之后又来了一条退回
    sender.onBackpressure(2)
    expect(resets).toHaveLength(1)
  })

  it('ignores bounces for envelopes of an older transport', () => {
    const { sender, resets, send } = setup()
    send(1)
    send(2)
    sender.discardTransport(10)
    sender.onBackpressure(2)
    sender.onBackpressure(1)
    expect(resets).toEqual([])
    expect(sender.inflightBytes).toBe(0)
  })

  it('ignores bounces for seqs it never sent as data (reset fillers, unknown)', () => {
    const { sender, resets, send } = setup()
    send(5)
    sender.onBackpressure(9)
    expect(resets).toEqual([])
  })
})

describe('rate_limited', () => {
  it('holds back everything for retry_after_ms and then releases in order', () => {
    const { sender, written, send } = setup()
    send(1)
    sender.onRateLimited(2_000)
    send(2)
    send(3)
    expect(written).toEqual([1])
    vi.advanceTimersByTime(1_999)
    expect(written).toEqual([1])
    vi.advanceTimersByTime(1)
    expect(written).toEqual([1, 2, 3])
  })

  it('delays the resend of bounced envelopes until the limit is over', () => {
    const { sender, written, send } = setup()
    send(1)
    sender.onRateLimited(3_000)
    sender.onBackpressure(1)
    vi.advanceTimersByTime(2_999)
    expect(written).toEqual([1])
    vi.advanceTimersByTime(1)
    expect(written).toEqual([1, 1])
  })

  it('ignores a limit addressed to another peer, honours one addressed to the daemon, and caps the wait', () => {
    const { sender, written, send } = setup()
    sender.onRateLimited(5_000, 'someone-else')
    send(1)
    expect(written).toEqual([1])
    sender.onRateLimited(5_000, PEER)
    send(2)
    expect(written).toEqual([1])
    sender.onRateLimited(3_600_000, null)
    vi.advanceTimersByTime(30_000)
    expect(written).toEqual([1, 2])
  })
})

describe('connection not writable', () => {
  it('keeps envelopes queued and retries', () => {
    let up = false
    const { written, send } = setup({ writable: () => up })
    send(1)
    send(2)
    expect(written).toEqual([])
    up = true
    vi.advanceTimersByTime(1_000)
    expect(written).toEqual([1, 2])
  })
})

describe('handshake envelope', () => {
  const hs = (seq: number) => Uint8Array.of(0xa0 + seq, 0)

  it('is written at once and resent unchanged after a bounce, with doubling delays', () => {
    const { sender, written } = setup()
    sender.sendHandshake(1, hs(1))
    expect(written).toEqual([0xa1])
    sender.onBackpressure(1)
    vi.advanceTimersByTime(299)
    expect(written).toEqual([0xa1])
    vi.advanceTimersByTime(1)
    expect(written).toEqual([0xa1, 0xa1])
    sender.onBackpressure(1)
    vi.advanceTimersByTime(599)
    expect(written).toHaveLength(2)
    vi.advanceTimersByTime(1)
    expect(written).toHaveLength(3)
    sender.handshakeDone()
    sender.onBackpressure(1)
    vi.advanceTimersByTime(60_000)
    expect(written).toHaveLength(3)
  })

  it('does not treat a handshake bounce as a lost data envelope', () => {
    const { sender, resets } = setup()
    sender.sendHandshake(1, hs(1))
    sender.onBackpressure(1)
    expect(resets).toEqual([])
    expect(sender.inflightBytes).toBe(0)
  })

  it('waits out rate_limited before the first write', () => {
    const { sender, written } = setup()
    sender.onRateLimited(1_000)
    sender.sendHandshake(1, hs(1))
    expect(written).toEqual([])
    vi.advanceTimersByTime(1_100)
    expect(written).toEqual([0xa1])
    expect(sender.handshakeHeld).toBe(false)
  })

  it('a newer handshake replaces the one being retried', () => {
    const { sender, written } = setup()
    sender.sendHandshake(1, hs(1))
    sender.onBackpressure(1)
    sender.sendHandshake(2, hs(2))
    vi.advanceTimersByTime(5_000)
    expect(written).toEqual([0xa1, 0xa2])
  })
})

describe('clear', () => {
  it('drops everything, including timers', () => {
    const { sender, written, send } = setup()
    send(1)
    sender.onBackpressure(1)
    send(2)
    sender.clear()
    vi.advanceTimersByTime(10_000)
    expect(written).toEqual([1])
    expect(sender.inflightBytes).toBe(0)
  })
})
