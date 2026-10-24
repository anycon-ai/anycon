import {
  TUNNEL_ACK_EVERY_BYTES,
  TUNNEL_CLIENT_INFLIGHT,
  TUNNEL_MAX_MESSAGE,
  TUNNEL_MAX_STREAMS,
  TUNNEL_STREAM_WINDOW,
  TUNNEL_WS_BINARY,
  TUNNEL_WS_FIN,
  TunnelFrameType as T,
  decodeGoaway,
  decodeRequestHead,
  decodeReset,
  encodeGoaway,
  parseWindow,
  parseWsClose,
  parseWsMessage,
  type PreviewRequestHead,
  type PreviewResponseHead,
  type TunnelFrame,
} from '@anycon/protocol'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { fakeTransportPair } from '../../test/fakeNoise'
import { TestPeer, dataFrame, endFrame, frame, resetFrame, responseFrame, text, windowFrame, wsCloseFrame, wsMessageFrame } from '../../test/peer'
import { TunnelSession, type StreamEnd, type StreamListener } from './session'

const GET = (path: string): PreviewRequestHead => ({ method: 'GET', path, headers: [['accept', '*/*']], websocket: false })

function setup() {
  const { page, daemon } = fakeTransportPair()
  const peer = new TestPeer(daemon)
  const pending: Array<{ ct: Uint8Array; index: number; pure: boolean }> = []
  const sizes = new Map<number, number>()
  const acks: number[] = []
  const goaways: string[] = []
  const desyncs: string[] = []
  let inflight = 0
  let released = 0
  const session = new TunnelSession({
    transport: page,
    send: (ct, index, pure) => {
      pending.push({ ct, index, pure })
      sizes.set(index, ct.length)
      inflight += ct.length
    },
    inflightBytes: () => inflight,
    onPeerAck: (n) => {
      acks.push(n)
      for (; released < n; released++) inflight -= sizes.get(released) ?? 0
    },
    onGoaway: (g) => goaways.push(g.code),
    onDesync: (r) => desyncs.push(r),
  })
  /** 排空微任务后，取出 P 新发出的消息（解密成帧）。 */
  const flushed = async (): Promise<Array<{ ack: number; frames: TunnelFrame[]; pure: boolean }>> => {
    await Promise.resolve()
    await Promise.resolve()
    return pending.splice(0).map((o) => ({ ...peer.read(o.ct), pure: o.pure }))
  }
  return {
    session,
    peer,
    pending,
    acks,
    goaways,
    desyncs,
    flushed,
    /** 假装有这么多字节还没被回执（先于触发发送的操作调用）。 */
    setInflight: (n: number) => {
      inflight = n
    },
  }
}

interface Recorder extends StreamListener {
  events: string[]
  head?: PreviewResponseHead
  chunks: Uint8Array[]
  ended?: StreamEnd
  messages: Array<{ binary: boolean; data: Uint8Array }>
}

function recorder(): Recorder {
  const r: Recorder = {
    events: [],
    chunks: [],
    messages: [],
    response(head) {
      r.events.push('response')
      r.head = head
    },
    data(chunk) {
      r.events.push('data')
      r.chunks.push(chunk)
    },
    end() {
      r.events.push('end')
    },
    wsMessage(m) {
      r.events.push('ws.message')
      r.messages.push(m)
    },
    wsClose(code) {
      r.events.push(`ws.close:${code}`)
    },
    failed(end) {
      r.events.push('failed')
      r.ended = end
    },
  }
  return r
}

beforeEach(() => vi.useFakeTimers())
afterEach(() => vi.useRealTimers())

describe('opening streams', () => {
  it('puts REQUEST, body and END of an exchange into one message, ids strictly increasing from 1', async () => {
    const t = setup()
    const [a, b] = await Promise.all([
      t.session.openStream(GET('/a'), recorder()),
      t.session.openStream({ ...GET('/b'), method: 'POST' }, recorder(), new TextEncoder().encode('hello')),
    ])
    expect([a.id, b.id]).toEqual([1, 2])
    const [m] = await t.flushed()
    expect(m!.frames.map((f) => [f.type, f.stream])).toEqual([
      [T.Request, 1],
      [T.Request, 2],
      [T.End, 1],
      [T.Data, 2],
      [T.End, 2],
    ])
    expect(decodeRequestHead(m!.frames[0]!.payload).path).toBe('/a')
    expect(decodeRequestHead(m!.frames[1]!.payload)).toMatchObject({ method: 'POST', path: '/b', websocket: false })
    expect(text(m!.frames[3]!.payload)).toBe('hello')
  })

  it('numbers messages from 0 and stamps the current ack on each', async () => {
    const t = setup()
    await t.session.openStream(GET('/'), recorder())
    const [first] = await t.flushed()
    expect(first!.ack).toBe(0)
    expect(t.pending).toHaveLength(0)
    t.session.recv(t.peer.write([responseFrame(1, 200), endFrame(1)]))
    await t.session.openStream(GET('/2'), recorder())
    const [second] = await t.flushed()
    expect(second!.ack).toBe(1)
    expect(t.session.counters).toEqual({ sent: 2, received: 1 })
  })
})

describe('responses', () => {
  it('delivers response, data and end in order, then frees the stream', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 200, [['content-type', 'text/plain']]), dataFrame(1, 'he'), dataFrame(1, 'llo'), endFrame(1)]))
    expect(rec.events).toEqual(['response', 'data', 'data', 'end'])
    expect(rec.head?.status).toBe(200)
    expect(text(Uint8Array.from(rec.chunks.flatMap((c) => [...c])))).toBe('hello')
    await t.flushed()
    expect(t.session.openStreams).toBe(0)
  })

  it('ignores frames for streams it does not know, and anything after a RESET', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    t.session.recv(t.peer.write([dataFrame(99, 'stray'), windowFrame(99, 10)]))
    t.session.recv(t.peer.write([resetFrame(1, 'refused', 'nope')]))
    expect(rec.ended).toEqual({ kind: 'reset', code: 'refused', message: 'nope' })
    t.session.recv(t.peer.write([windowFrame(1, 100), dataFrame(1, 'late')]))
    expect(rec.events).toEqual(['failed'])
    expect(t.session.openStreams).toBe(0)
    expect(t.desyncs).toEqual([])
  })

  it('resets a stream that sends data before its response, or twice a response', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    t.session.recv(t.peer.write([dataFrame(1, 'early')]))
    expect(rec.ended).toMatchObject({ kind: 'reset', code: 'protocol' })
    const [m] = await t.flushed()
    const reset = m!.frames.find((f) => f.type === T.Reset)!
    expect(decodeReset(reset.payload).code).toBe('protocol')

    const rec2 = recorder()
    await t.session.openStream(GET('/y'), rec2)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(2, 200), responseFrame(2, 200)]))
    expect(rec2.ended).toMatchObject({ code: 'protocol' })
  })

  it('resets with flow_control when the daemon overruns the window', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    const chunk = new Uint8Array(60 * 1024)
    t.session.recv(t.peer.write([responseFrame(1, 200), dataFrame(1, chunk), dataFrame(1, chunk)]))
    t.session.recv(t.peer.write([dataFrame(1, chunk), dataFrame(1, chunk)]))
    // 4 × 60 KiB = 240 KiB < 256 KiB：没问题
    expect(rec.ended).toBeUndefined()
    t.session.recv(t.peer.write([dataFrame(1, chunk)]))
    expect(rec.ended).toMatchObject({ kind: 'reset', code: 'flow_control' })
  })
})

describe('receive window', () => {
  it('returns the window in half-window steps as the page consumes', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    const chunk = new Uint8Array(32 * 1024)
    t.session.recv(t.peer.write([responseFrame(1, 200), dataFrame(1, chunk), dataFrame(1, chunk), dataFrame(1, chunk)]))
    s.consumed(32 * 1024)
    s.consumed(32 * 1024)
    s.consumed(32 * 1024)
    // 96 KiB < 128 KiB：还不还
    vi.advanceTimersByTime(0)
    let sent = await t.flushed()
    expect(sent.flatMap((m) => m.frames).filter((f) => f.type === T.Window)).toHaveLength(0)
    s.consumed(32 * 1024)
    sent = await t.flushed()
    const windows = sent.flatMap((m) => m.frames).filter((f) => f.type === T.Window)
    expect(windows).toHaveLength(1)
    expect(parseWindow(windows[0]!.payload)).toBe(128 * 1024)
    // 归还之后，对端又可以发满
    t.session.recv(t.peer.write([dataFrame(1, new Uint8Array(100 * 1024))]))
    t.session.recv(t.peer.write([dataFrame(1, new Uint8Array(TUNNEL_STREAM_WINDOW - 32 * 1024 - 100 * 1024))]))
    expect(rec.ended).toBeUndefined()
  })

  it('does not return window once the daemon has finished the body', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 200), dataFrame(1, new Uint8Array(100 * 1024))]))
    t.session.recv(t.peer.write([dataFrame(1, new Uint8Array(100 * 1024)), endFrame(1)]))
    s.consumed(200 * 1024)
    const sent = await t.flushed()
    expect(sent.flatMap((m) => m.frames).filter((f) => f.type === T.Window)).toHaveLength(0)
  })
})

describe('send window and request bodies', () => {
  it('sends at most the granted window, in frames of at most 64 KiB, and continues on WINDOW', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream({ ...GET('/up'), method: 'PUT' }, rec, new Uint8Array(600 * 1024))
    let frames = (await t.flushed()).flatMap((m) => m.frames)
    const datas = frames.filter((f) => f.type === T.Data)
    expect(datas.reduce((n, f) => n + f.payload.length, 0)).toBe(TUNNEL_STREAM_WINDOW)
    expect(Math.max(...datas.map((f) => f.payload.length))).toBe(64 * 1024)
    expect(frames.some((f) => f.type === T.End)).toBe(false)
    // daemon 消费了一半、归还 128 KiB
    t.session.recv(t.peer.write([windowFrame(1, 128 * 1024)]))
    frames = (await t.flushed()).flatMap((m) => m.frames)
    expect(frames.filter((f) => f.type === T.Data).reduce((n, f) => n + f.payload.length, 0)).toBe(128 * 1024)
    t.session.recv(t.peer.write([windowFrame(1, 1024 * 1024)]))
    frames = (await t.flushed()).flatMap((m) => m.frames)
    expect(frames.filter((f) => f.type === T.Data).reduce((n, f) => n + f.payload.length, 0)).toBe(600 * 1024 - 256 * 1024 - 128 * 1024)
    expect(frames.at(-1)!.type).toBe(T.End)
  })

  it('keeps every message within 128 KiB of plaintext', async () => {
    const t = setup()
    await t.session.openStream({ ...GET('/up'), method: 'PUT' }, recorder(), new Uint8Array(256 * 1024))
    const sent = await t.flushed()
    expect(sent.length).toBeGreaterThanOrEqual(2)
    for (const m of sent) {
      const len = 8 + m.frames.reduce((n, f) => n + 9 + f.payload.length, 0)
      expect(len).toBeLessThanOrEqual(TUNNEL_MAX_MESSAGE)
    }
  })

  it('shares messages between streams round-robin so one upload cannot starve another', async () => {
    const t = setup()
    await Promise.all([
      t.session.openStream({ ...GET('/a'), method: 'PUT' }, recorder(), new Uint8Array(200 * 1024)),
      t.session.openStream({ ...GET('/b'), method: 'PUT' }, recorder(), new Uint8Array(100 * 1024)),
    ])
    const frames = (await t.flushed()).flatMap((m) => m.frames).filter((f) => f.type === T.Data)
    const order = frames.map((f) => f.stream)
    // b 的第一帧出现在 a 的 200 KiB 发完之前
    expect(order.indexOf(2)).toBeLessThan(order.lastIndexOf(1))
  })

  it('treats a WINDOW that would overflow as a protocol error', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(GET('/x'), rec)
    await t.flushed()
    t.session.recv(t.peer.write([windowFrame(1, 0x7fff_ffff)]))
    expect(rec.ended).toMatchObject({ code: 'protocol' })
  })
})

describe('acknowledgements', () => {
  it('sends a pure ack 50 ms after a message when it has nothing else to send', async () => {
    const t = setup()
    t.session.recv(t.peer.write([dataFrame(77, 'x')])) // 未知 stream，但算一条带帧的消息
    await t.flushed()
    vi.advanceTimersByTime(49)
    expect(t.pending).toHaveLength(0)
    vi.advanceTimersByTime(1)
    const [m] = await t.flushed()
    expect(m).toMatchObject({ ack: 1, pure: true })
    expect(m!.frames).toEqual([])
  })

  it('piggybacks the ack on the next outgoing message and cancels the timer', async () => {
    const t = setup()
    t.session.recv(t.peer.write([dataFrame(77, 'x')]))
    await t.session.openStream(GET('/'), recorder())
    const [m] = await t.flushed()
    expect(m!.ack).toBe(1)
    expect(m!.pure).toBe(false)
    vi.advanceTimersByTime(1_000)
    expect(t.pending).toHaveLength(0)
  })

  it('acks immediately after 32 unacknowledged messages', async () => {
    const t = setup()
    for (let i = 0; i < 31; i++) t.session.recv(t.peer.write([dataFrame(77, 'x')]))
    await t.flushed()
    expect(t.pending).toHaveLength(0)
    t.session.recv(t.peer.write([dataFrame(77, 'x')]))
    const [m] = await t.flushed()
    expect(m).toMatchObject({ ack: 32, pure: true })
  })

  it('acks immediately after 256 KiB of unacknowledged ciphertext', async () => {
    const t = setup()
    const big = new Uint8Array(100 * 1024)
    t.session.recv(t.peer.write([dataFrame(77, big)]))
    t.session.recv(t.peer.write([dataFrame(77, big)]))
    await t.flushed()
    expect(t.pending).toHaveLength(0)
    t.session.recv(t.peer.write([dataFrame(77, big)]))
    expect(100 * 1024 * 3).toBeGreaterThan(TUNNEL_ACK_EVERY_BYTES)
    const [m] = await t.flushed()
    expect(m).toMatchObject({ ack: 3, pure: true })
  })

  it('does not answer a pure ack with another ack', async () => {
    const t = setup()
    t.session.recv(t.peer.write([]))
    t.session.recv(t.peer.write([]))
    vi.advanceTimersByTime(1_000)
    expect(await t.flushed()).toEqual([])
    // 但回执数记着：下一条带帧的消息的 ack 包含它们
    await t.session.openStream(GET('/'), recorder())
    const [m] = await t.flushed()
    expect(m!.ack).toBe(2)
  })

  it('reports the daemon ack and rejects an ack for messages that were never sent', async () => {
    const t = setup()
    await t.session.openStream(GET('/'), recorder())
    await t.flushed()
    t.session.recv(t.peer.write([], 1))
    expect(t.acks).toEqual([1])
    t.session.recv(t.peer.write([], 2))
    expect(t.desyncs).toHaveLength(1)
  })
})

describe('in-flight limit', () => {
  it('holds messages with frames while 2 MiB are unacknowledged, but still sends pure acks', async () => {
    const t = setup()
    t.setInflight(TUNNEL_CLIENT_INFLIGHT - 10)
    await t.session.openStream(GET('/'), recorder())
    expect(await t.flushed()).toEqual([])
    // 收到一条消息：纯回执不受限
    t.session.recv(t.peer.write([dataFrame(77, 'x')]))
    vi.advanceTimersByTime(60)
    const sent = await t.flushed()
    expect(sent).toHaveLength(1)
    expect(sent[0]).toMatchObject({ pure: true })
    // 窗口空出来之后再发
    t.setInflight(0)
    t.session.pump()
    const more = await t.flushed()
    expect(more.flatMap((m) => m.frames.map((f) => f.type))).toEqual([T.Request, T.End])
  })

  it('shrinks the message so that in-flight bytes never pass the limit', async () => {
    const t = setup()
    t.setInflight(TUNNEL_CLIENT_INFLIGHT - 40 * 1024)
    await t.session.openStream({ ...GET('/up'), method: 'PUT' }, recorder(), new Uint8Array(100 * 1024))
    const sent = await t.flushed()
    // 假传输每条消息多 6 字节，真实的最多多 37（两片的标签和长度 + 框架头）：按真实的算，总数不超过剩余的 40 KiB
    const total = sent.reduce((n, m) => n + m.frames.reduce((k, f) => k + 9 + f.payload.length, 8) + 37, 0)
    expect(total).toBeLessThanOrEqual(40 * 1024)
    expect(total).toBeGreaterThan(30 * 1024)
  })
})

describe('stream limit', () => {
  it('queues the 257th stream until one finishes', async () => {
    const t = setup()
    const opened: number[] = []
    for (let i = 0; i < TUNNEL_MAX_STREAMS; i++) {
      const s = await t.session.openStream(GET(`/${i}`), recorder())
      opened.push(s.id)
    }
    expect(t.session.openStreams).toBe(TUNNEL_MAX_STREAMS)
    let late: number | undefined
    const pending = t.session.openStream(GET('/late'), recorder()).then((s) => (late = s.id))
    await Promise.resolve()
    expect(late).toBeUndefined()
    t.session.recv(t.peer.write([resetFrame(5, 'cancelled')]))
    await pending
    expect(late).toBe(TUNNEL_MAX_STREAMS + 1)
  })
})

describe('abort', () => {
  it('sends RESET, forgets queued data and ignores the daemon afterwards', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream({ ...GET('/up'), method: 'PUT' }, rec, new Uint8Array(500 * 1024))
    await t.flushed()
    s.abort('cancelled')
    const frames = (await t.flushed()).flatMap((m) => m.frames)
    expect(frames.map((f) => f.type)).toEqual([T.Reset])
    expect(decodeReset(frames[0]!.payload).code).toBe('cancelled')
    t.session.recv(t.peer.write([responseFrame(1, 200), dataFrame(1, 'x')]))
    expect(rec.events).toEqual([]) // 自己中止的不回调
    expect(t.session.openStreams).toBe(0)
  })
})

describe('websocket streams', () => {
  const WS: PreviewRequestHead = { method: 'GET', path: '/ws', headers: [], websocket: true }

  it('opens, assembles fragmented messages, and closes in both directions', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(WS, rec)
    let sent = await t.flushed()
    expect(sent[0]!.frames.map((f) => f.type)).toEqual([T.Request]) // 升级请求没有 END
    t.session.recv(t.peer.write([responseFrame(1, 101, [['sec-websocket-protocol', 'vite-hmr']])]))
    expect(rec.head?.status).toBe(101)
    t.session.recv(
      t.peer.write([wsMessageFrame(1, TUNNEL_WS_BINARY, new Uint8Array([1, 2])), wsMessageFrame(1, TUNNEL_WS_BINARY | TUNNEL_WS_FIN, new Uint8Array([3]))]),
    )
    t.session.recv(t.peer.write([wsMessageFrame(1, TUNNEL_WS_FIN, '{"type":"connected"}')]))
    expect(rec.messages.map((m) => [m.binary, [...m.data]])).toEqual([
      [true, [1, 2, 3]],
      [false, [...new TextEncoder().encode('{"type":"connected"}')]],
    ])
    // 发一条文本消息
    s.sendWsMessage(false, new TextEncoder().encode('ping'))
    sent = await t.flushed()
    const msg = sent.flatMap((m) => m.frames).find((f) => f.type === T.WsMessage)!
    expect(parseWsMessage(msg.payload).flags).toBe(TUNNEL_WS_FIN)
    // 对端先关：回一个 WS_CLOSE，stream 结束
    t.session.recv(t.peer.write([wsCloseFrame(1, 1001, 'going away')]))
    expect(rec.events).toContain('ws.close:1001')
    sent = await t.flushed()
    const close = sent.flatMap((m) => m.frames).find((f) => f.type === T.WsClose)!
    expect(parseWsClose(close.payload).code).toBe(1001)
    await t.flushed()
    expect(t.session.openStreams).toBe(0)
  })

  it('splits an outgoing message into window-limited frames with FIN only on the last', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(WS, rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 101)]))
    s.sendWsMessage(true, new Uint8Array(150 * 1024))
    const frames = (await t.flushed()).flatMap((m) => m.frames).filter((f) => f.type === T.WsMessage)
    const parsed = frames.map((f) => parseWsMessage(f.payload))
    expect(parsed.reduce((n, p) => n + p.data.length, 0)).toBe(150 * 1024)
    expect(Math.max(...parsed.map((p) => p.data.length))).toBeLessThanOrEqual(64 * 1024)
    const flags = parsed.map((p) => p.flags)
    expect(flags.slice(0, -1).every((f) => f === TUNNEL_WS_BINARY)).toBe(true)
    expect(flags.at(-1)).toBe(TUNNEL_WS_BINARY | TUNNEL_WS_FIN)
  })

  it('sends an empty message as one FIN frame', async () => {
    const t = setup()
    const s = await t.session.openStream(WS, recorder())
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 101)]))
    s.sendWsMessage(false, new Uint8Array(0))
    const f = (await t.flushed()).flatMap((m) => m.frames).find((x) => x.type === T.WsMessage)!
    expect(parseWsMessage(f.payload)).toMatchObject({ flags: TUNNEL_WS_FIN })
  })

  it('closes from our side and waits for the daemon close before freeing the stream', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(WS, rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 101)]))
    s.closeWs(1000, 'bye')
    const sent = await t.flushed()
    expect(parseWsClose(sent.flatMap((m) => m.frames).find((f) => f.type === T.WsClose)!.payload)).toEqual({ code: 1000, reason: 'bye' })
    expect(t.session.openStreams).toBe(1)
    t.session.recv(t.peer.write([wsCloseFrame(1, 1000)]))
    await t.flushed()
    expect(t.session.openStreams).toBe(0)
    expect(rec.events).toContain('ws.close:1000')
  })

  it('answers a peer close with 1000 when the code cannot go on the wire', async () => {
    const t = setup()
    const s = await t.session.openStream(WS, recorder())
    void s
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 101)]))
    t.session.recv(t.peer.write([wsCloseFrame(1, 1005)]))
    const sent = await t.flushed()
    expect(parseWsClose(sent.flatMap((m) => m.frames).find((f) => f.type === T.WsClose)!.payload).code).toBe(1000)
  })

  it('treats a refused upgrade as a plain response and frees the stream at END', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(WS, rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 403), dataFrame(1, 'no'), endFrame(1)]))
    expect(rec.events).toEqual(['response', 'data', 'end'])
    expect(rec.head?.status).toBe(403)
    expect(t.session.openStreams).toBe(0)
  })

  it('resets a stream whose assembled message passes 16 MiB', async () => {
    const t = setup()
    const rec = recorder()
    const s = await t.session.openStream(WS, rec)
    await t.flushed()
    t.session.recv(t.peer.write([responseFrame(1, 101)]))
    const chunk = new Uint8Array(60 * 1024)
    for (let i = 0; i < 300 && !rec.ended; i++) {
      t.session.recv(t.peer.write([wsMessageFrame(1, TUNNEL_WS_BINARY, chunk)]))
      s.consumed(chunk.length) // 页面（桥）每收到一段就归还窗口
      await t.flushed()
    }
    expect(rec.ended).toMatchObject({ code: 'too_large' })
  })

  it('rejects websocket frames before the upgrade', async () => {
    const t = setup()
    const rec = recorder()
    await t.session.openStream(WS, rec)
    await t.flushed()
    t.session.recv(t.peer.write([wsMessageFrame(1, TUNNEL_WS_FIN, 'x')]))
    expect(rec.ended).toMatchObject({ code: 'protocol' })
  })
})

describe('transport failures', () => {
  it('reports a GOAWAY', () => {
    const t = setup()
    t.session.recv(t.peer.write([frame(T.Goaway, 0, encodeGoaway({ code: 'closed', message: 'bye' }))]))
    expect(t.goaways).toEqual(['closed'])
    t.session.recv(t.peer.write([frame(T.Goaway, 0, Uint8Array.of(0xc1))]))
    expect(t.goaways).toEqual(['closed', 'shutdown']) // 看不懂的载荷也当作终态
    void decodeGoaway
  })

  it('reports desync when a message cannot be decrypted', () => {
    const t = setup()
    t.session.recv(Uint8Array.of(1, 2, 3, 4, 5, 6, 7))
    expect(t.desyncs).toHaveLength(1)
  })

  it('fails every stream when destroyed and refuses new ones', async () => {
    const t = setup()
    const a = recorder()
    const b = recorder()
    await t.session.openStream(GET('/a'), a)
    await t.session.openStream(GET('/b'), b)
    t.session.destroy({ kind: 'transport', message: 'peer offline' })
    expect(a.ended).toEqual({ kind: 'transport', message: 'peer offline' })
    expect(b.ended).toEqual({ kind: 'transport', message: 'peer offline' })
    await expect(t.session.openStream(GET('/c'), recorder())).rejects.toThrow()
    t.session.recv(t.peer.write([]))
    expect(t.desyncs).toEqual([])
  })

  it('rejects oversized request heads without allocating a stream id', async () => {
    const t = setup()
    const big: PreviewRequestHead = { method: 'GET', path: '/', headers: [['x', 'a'.repeat(70_000)]], websocket: false }
    await expect(t.session.openStream(big, recorder())).rejects.toThrow()
    const s = await t.session.openStream(GET('/'), recorder())
    expect(s.id).toBe(1)
  })
})
