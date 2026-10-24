import {
  CLOSE_CODE,
  TunnelFrameType as T,
  decodeEnvelope,
  decodeFrame,
  encodeEnvelope,
  encodeGoaway,
  makeEnvelope,
  parseHandshakeInit,
  type Control,
} from '@anycon/protocol'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { NODE_ID, Rig, SELF_ID, makeLaunch, settle } from '../../test/rig'
import { dataFrame, endFrame, frame, responseFrame } from '../../test/peer'
import { TunnelUnavailable, type TunnelState } from './link'

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(1_700_000_000_000)
})
afterEach(() => vi.useRealTimers())

const phases = (rig: Rig): TunnelState['phase'][] => {
  const seen: TunnelState['phase'][] = []
  rig.tunnel.onState((s) => seen.push(s.phase))
  return seen
}

describe('connecting and authenticating', () => {
  it('connects to the preview websocket and authenticates with the preview token only', async () => {
    const rig = new Rig()
    const seen = phases(rig)
    rig.tunnel.start()
    expect(rig.sockets).toHaveLength(1)
    expect(rig.socket.url).toBe('wss://abcdefghijklmnopqrstuvwxyz.preview.example/v1/ws')
    expect(rig.socket.binaryType).toBe('arraybuffer')
    rig.socket.open()
    const [auth] = rig.sentControls()
    expect(auth).toMatchObject({
      type: 'auth',
      preview_token: 'preview-token',
      client: { kind: 'web', origin: 'https://abcdefghijklmnopqrstuvwxyz.preview.example' },
    })
    expect((auth as Extract<Control, { type: 'auth' }>).device_token ?? null).toBeNull()
    expect(rig.sent()[0]).toMatchObject({ from: '', to: '', seq: 1 })
    expect(seen).toEqual(['connecting'])
  })

  it('waits for the machine when it is not online, and handshakes when it comes online', async () => {
    const rig = new Rig()
    rig.tunnel.start()
    await rig.connect([])
    expect(rig.tunnel.state.phase).toBe('waiting_peer')
    expect(rig.sent().some((e) => e.kind === 'handshake')).toBe(false)
    rig.hubControl({ type: 'peer_online', device: NODE_ID })
    await settle()
    expect(rig.tunnel.state.phase).toBe('handshaking')
    await rig.answerHandshake()
    expect(rig.tunnel.state.phase).toBe('ready')
  })

  it('ignores peer_online for other devices', async () => {
    const rig = new Rig()
    rig.tunnel.start()
    await rig.connect([])
    rig.hubControl({ type: 'peer_online', device: 'someone-else' })
    await settle()
    expect(rig.tunnel.state.phase).toBe('waiting_peer')
  })
})

describe('handshake', () => {
  it('sends version, ticket id and the first Noise message, then becomes ready on the reply', async () => {
    const rig = new Rig()
    const states = phases(rig)
    await rig.ready()
    const hs = rig.sent().find((e) => e.kind === 'handshake')!
    expect(hs).toMatchObject({ from: SELF_ID, to: NODE_ID })
    const { ticketId } = parseHandshakeInit(hs.body)
    expect(Array.from(ticketId, (b) => b.toString(16).padStart(2, '0')).join('')).toBe(rig.launch.ticket_id)
    expect(states).toEqual(['connecting', 'handshaking', 'ready'])
    expect(rig.tunnel.session).not.toBeNull()
  })

  it.each([
    ['unknown_ticket', 'rejected'],
    ['label_mismatch', 'rejected'],
    ['busy', 'rejected'],
    ['malformed', 'rejected'],
    ['expired', 'expired'],
    ['port_not_allowed', 'port_not_allowed'],
    ['disabled', 'disabled'],
    ['something_new', 'rejected'],
  ])('a rejection (%s) is final and is reported as %s', async (reason, kind) => {
    const rig = new Rig()
    rig.handshakeMode = `reject:${reason}`
    rig.tunnel.start()
    await rig.connect()
    await rig.answerHandshake()
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind, detail: reason })
    expect(rig.socket.closedByClient).not.toBeNull()
    vi.advanceTimersByTime(60_000)
    expect(rig.sockets).toHaveLength(1) // 不再重连
  })

  it('treats a reply from something other than the pinned daemon as a failure, final after three tries', async () => {
    const rig = new Rig()
    rig.handshakeMode = 'garbage'
    rig.tunnel.start()
    await rig.connect()
    for (let i = 0; i < 3; i++) {
      expect(rig.tunnel.state.phase).toBe('handshaking')
      await rig.answerHandshake()
    }
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'handshake_failed' })
  })

  it('treats a hello for another port as a failure', async () => {
    const rig = new Rig()
    rig.hello = (l) => ({ port: l.port + 1, expires_at_ms: l.expires_at_ms })
    rig.tunnel.start()
    await rig.connect()
    await rig.answerHandshake()
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'handshake_failed' })
  })

  it('retries when the machine does not answer, and reconnects after three attempts', async () => {
    const rig = new Rig()
    rig.handshakeMode = 'silent'
    rig.tunnel.start()
    await rig.connect()
    await rig.answerHandshake()
    expect(rig.sent().filter((e) => e.kind === 'handshake')).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(8_000)
    expect(rig.sent().filter((e) => e.kind === 'handshake')).toHaveLength(2)
    await vi.advanceTimersByTimeAsync(8_000)
    expect(rig.sent().filter((e) => e.kind === 'handshake')).toHaveLength(3)
    await vi.advanceTimersByTimeAsync(8_000)
    expect(rig.tunnel.state.phase).toBe('backoff')
    expect(rig.socket.closedByClient).not.toBeNull()
    await vi.advanceTimersByTimeAsync(600)
    expect(rig.sockets).toHaveLength(2)
  })

  it('goes terminal when the crypto module cannot be loaded', async () => {
    const rig = new Rig({ crypto: () => Promise.reject(new Error('anycon crypto wasm is not built')) })
    rig.tunnel.start()
    await rig.connect()
    await settle()
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'crypto_unavailable', detail: 'anycon crypto wasm is not built' })
  })
})

describe('terminal states', () => {
  it.each([
    ['invalid', 'unauthorized'],
    ['revoked', 'unauthorized'],
    ['version', 'version'],
    ['protocol', 'version'],
  ] as const)('auth_fail %s is final (%s)', async (reason, kind) => {
    const rig = new Rig()
    rig.tunnel.start()
    rig.socket.open()
    rig.hubControl({ type: 'auth_fail', reason })
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind })
    await expect(rig.tunnel.whenReady()).rejects.toMatchObject({ kind, terminal: true })
    vi.advanceTimersByTime(60_000)
    expect(rig.sockets).toHaveLength(1)
  })

  it('auth_fail quota is not final: it backs off and tries again', async () => {
    const rig = new Rig()
    rig.tunnel.start()
    rig.socket.open()
    rig.hubControl({ type: 'auth_fail', reason: 'quota' })
    expect(rig.tunnel.state.phase).toBe('backoff')
    await vi.advanceTimersByTimeAsync(500)
    expect(rig.sockets).toHaveLength(2)
  })

  it('close code 4004 is final', async () => {
    const rig = new Rig()
    await rig.ready()
    rig.socket.serverClose(CLOSE_CODE.REVOKED)
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'revoked' })
    vi.advanceTimersByTime(60_000)
    expect(rig.sockets).toHaveLength(1)
  })

  it.each([
    ['expired', 'expired'],
    ['closed', 'closed'],
    ['revoked', 'revoked'],
    ['disabled', 'disabled'],
    ['port_not_allowed', 'port_not_allowed'],
    ['shutdown', 'shutdown'],
  ] as const)('GOAWAY %s is final and fails the open streams', async (code, kind) => {
    const rig = new Rig()
    await rig.ready()
    const failures: string[] = []
    const session = rig.tunnel.session!
    await session.openStream({ method: 'GET', path: '/', headers: [], websocket: false }, { failed: (e) => failures.push(e.kind) })
    await settle()
    rig.daemonSend([frame(T.Goaway, 0, encodeGoaway({ code }))])
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind })
    expect(failures).toEqual(['transport'])
    expect(rig.socket.closedByClient).not.toBeNull()
  })

  it('the launch expiring is final', async () => {
    const rig = new Rig({ launch: makeLaunch({ expires_at_ms: Date.now() + 5_000 }) })
    await rig.ready()
    await vi.advanceTimersByTimeAsync(5_000)
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'expired' })
  })

  it('an already expired launch never connects', async () => {
    const rig = new Rig({ launch: makeLaunch({ expires_at_ms: Date.now() - 1 }) })
    rig.tunnel.start()
    expect(rig.sockets).toHaveLength(0)
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'expired' })
  })

  it('the daemon hello can shorten the lifetime', async () => {
    const rig = new Rig()
    rig.hello = (l) => ({ port: l.port, expires_at_ms: Date.now() + 2_000 })
    await rig.ready()
    await vi.advanceTimersByTimeAsync(2_000)
    expect(rig.tunnel.state).toMatchObject({ phase: 'terminal', kind: 'expired' })
  })
})

describe('reconnecting', () => {
  it('backs off 0.5 s, 1 s, 2 s … capped at 10 s', async () => {
    const rig = new Rig()
    rig.tunnel.start()
    const expected = [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000]
    for (const [i, wait] of expected.entries()) {
      expect(rig.sockets).toHaveLength(i + 1)
      rig.socket.serverClose(1006)
      expect(rig.tunnel.state).toMatchObject({ phase: 'backoff', retryInMs: wait })
      await vi.advanceTimersByTimeAsync(wait - 1)
      expect(rig.sockets).toHaveLength(i + 1)
      await vi.advanceTimersByTimeAsync(1)
    }
  })

  it('starts the backoff over once a connection has been stable', async () => {
    const rig = new Rig()
    await rig.ready()
    rig.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(500)
    rig.socket.serverClose(1006) // 第二次：1 s
    expect(rig.tunnel.state).toMatchObject({ retryInMs: 1_000 })
    await vi.advanceTimersByTimeAsync(1_000)
    await rig.connect()
    await vi.advanceTimersByTimeAsync(10_000)
    rig.socket.serverClose(1006)
    expect(rig.tunnel.state).toMatchObject({ retryInMs: 500 })
  })

  it('reconnects after a hub restart (4002) in 1 to 3 seconds without counting it as a failure', async () => {
    const rig = new Rig()
    await rig.ready()
    rig.socket.serverClose(CLOSE_CODE.RESTARTING)
    expect(rig.tunnel.state).toMatchObject({ phase: 'backoff', retryInMs: 2_000 })
    await vi.advanceTimersByTimeAsync(2_000)
    expect(rig.sockets).toHaveLength(2)
    rig.socket.serverClose(1006)
    expect(rig.tunnel.state).toMatchObject({ retryInMs: 500 })
  })

  it('fails open streams on disconnect and handshakes again on the new connection', async () => {
    const rig = new Rig()
    await rig.ready()
    const failures: string[] = []
    await rig.tunnel.session!.openStream({ method: 'GET', path: '/', headers: [], websocket: false }, { failed: (e) => failures.push(e.kind) })
    rig.socket.serverClose(1006)
    expect(failures).toEqual(['transport'])
    expect(rig.tunnel.session).toBeNull()
    await vi.advanceTimersByTimeAsync(500)
    await rig.connect(undefined, rig.socket)
    await rig.answerHandshake()
    expect(rig.tunnel.state.phase).toBe('ready')
  })

  it('the machine going offline drops the transport; coming back handshakes again', async () => {
    const rig = new Rig()
    await rig.ready()
    const first = rig.tunnel.session
    rig.hubControl({ type: 'peer_offline', device: NODE_ID })
    expect(rig.tunnel.state.phase).toBe('waiting_peer')
    expect(first?.destroyed).toBe(true)
    rig.hubControl({ type: 'peer_online', device: NODE_ID })
    await settle()
    await rig.answerHandshake()
    expect(rig.tunnel.state.phase).toBe('ready')
    expect(rig.tunnel.session).not.toBe(first)
  })
})

describe('whenReady', () => {
  it('resolves once the tunnel is ready', async () => {
    const rig = new Rig()
    const ready = rig.tunnel.whenReady()
    expect(rig.sockets).toHaveLength(1) // 第一次等待启动隧道
    await rig.connect()
    await rig.answerHandshake()
    await expect(ready).resolves.toBe(rig.tunnel.session)
    await expect(rig.tunnel.whenReady()).resolves.toBe(rig.tunnel.session)
  })

  it('gives up on an offline machine after 3 seconds, and stays usable afterwards', async () => {
    const rig = new Rig()
    const ready = rig.tunnel.whenReady()
    const caught = ready.catch((e: unknown) => e)
    await rig.connect([])
    await vi.advanceTimersByTimeAsync(2_999)
    expect(rig.tunnel.state.phase).toBe('waiting_peer')
    await vi.advanceTimersByTimeAsync(1)
    const err = (await caught) as TunnelUnavailable
    expect(err).toBeInstanceOf(TunnelUnavailable)
    expect(err.kind).toBe('node_offline')
    expect(err.terminal).toBe(false)
    // 一个新的请求在节点已离线时同样只等 3 秒
    const again = rig.tunnel.whenReady().catch((e: unknown) => e)
    await vi.advanceTimersByTimeAsync(3_000)
    expect(((await again) as TunnelUnavailable).kind).toBe('node_offline')
  })

  it('gives up after 20 seconds when the hub cannot be reached', async () => {
    const rig = new Rig()
    const caught = rig.tunnel.whenReady().catch((e: unknown) => e)
    await vi.advanceTimersByTimeAsync(20_000)
    const err = (await caught) as TunnelUnavailable
    expect(err.kind).toBe('connecting')
    expect(err.terminal).toBe(false)
  })

  it('stop() fails waiters and leaves no timers or sockets behind', async () => {
    const rig = new Rig()
    const caught = rig.tunnel.whenReady().catch((e: unknown) => e)
    rig.tunnel.stop()
    expect(((await caught) as TunnelUnavailable).kind).toBe('connecting')
    expect(rig.socket.closedByClient).not.toBeNull()
    expect(vi.getTimerCount()).toBe(0)
    expect(rig.tunnel.state.phase).toBe('idle')
  })
})

describe('data path', () => {
  async function open(rig: Rig, path = '/') {
    const events: string[] = []
    const chunks: string[] = []
    const stream = await rig.tunnel.session!.openStream(
      { method: 'GET', path, headers: [['accept', '*/*']], websocket: false },
      {
        response: (h) => events.push(`response:${h.status}`),
        data: (c) => chunks.push(new TextDecoder().decode(c)),
        end: () => events.push('end'),
        failed: (e) => events.push(`failed:${e.kind}`),
      },
    )
    await settle()
    return { stream, events, chunks }
  }

  it('wraps tunnel messages into data envelopes from P to the machine with increasing seq', async () => {
    const rig = new Rig()
    await rig.ready()
    const seqBefore = rig.sent().at(-1)!.seq
    await open(rig, '/a')
    await open(rig, '/b')
    const data = rig.readData()
    expect(data).toHaveLength(2)
    expect(data.map((d) => [d.env.from, d.env.to])).toEqual([
      [SELF_ID, NODE_ID],
      [SELF_ID, NODE_ID],
    ])
    expect(data[0]!.env.seq).toBe(seqBefore + 1)
    expect(data[1]!.env.seq).toBe(seqBefore + 2)
    expect(data[0]!.frames[0]!.type).toBe(T.Request)
    expect(decodeFrame(data[0]!.env.body).kind).toBe('whole')
  })

  it('delivers the daemon response and acks it', async () => {
    const rig = new Rig()
    await rig.ready()
    const x = await open(rig)
    rig.daemonSend([responseFrame(1, 200), dataFrame(1, 'hi'), endFrame(1)], 1)
    await settle()
    expect(x.events).toEqual(['response:200', 'end'])
    expect(x.chunks).toEqual(['hi'])
    await vi.advanceTimersByTimeAsync(50)
    const acks = rig.readData().filter((d) => d.frames.length === 0)
    expect(acks.at(-1)!.ack).toBe(1)
  })

  it('resends envelopes the hub returned, byte for byte and in order, after the retry delay', async () => {
    const rig = new Rig()
    await rig.ready()
    await open(rig, '/a')
    await open(rig, '/b')
    const [first, second] = rig.sent().filter((e) => e.kind === 'data')
    const count = rig.socket.sent.length
    rig.hubControl({ type: 'backpressure', to: NODE_ID, seq: second!.seq })
    rig.hubControl({ type: 'backpressure', to: NODE_ID, seq: first!.seq })
    await open(rig, '/c') // 暂停中：排队
    expect(rig.socket.sent).toHaveLength(count) // 暂停期间没有新的写出
    await vi.advanceTimersByTimeAsync(100) // 抖动取 0.5：100 + 0.5 × 200 ms
    expect(rig.socket.sent).toHaveLength(count)
    await vi.advanceTimersByTimeAsync(100)
    const resent = rig.socket.sent.slice(count)
    expect(resent).toHaveLength(3)
    expect(resent.slice(0, 2).map((b) => decodeEnvelope(b).seq)).toEqual([first!.seq, second!.seq])
    // 原样：和第一次发出的字节完全一样
    expect(resent[0]).toEqual(rig.socket.sent[rig.socket.sent.findIndex((b) => decodeEnvelope(b).seq === first!.seq)])
    expect(decodeEnvelope(resent[2]!).seq).toBeGreaterThan(second!.seq)
  })

  it('pauses on rate_limited { to: machine } for retry_after_ms', async () => {
    const rig = new Rig()
    await rig.ready()
    const before = rig.socket.sent.length
    rig.hubControl({ type: 'rate_limited', retry_after_ms: 3_000, to: NODE_ID })
    await open(rig)
    expect(rig.socket.sent).toHaveLength(before)
    await vi.advanceTimersByTimeAsync(2_999)
    expect(rig.socket.sent).toHaveLength(before)
    await vi.advanceTimersByTimeAsync(1)
    expect(rig.socket.sent).toHaveLength(before + 1)
  })

  it('re-handshakes when the machine sends RESET, failing the open streams', async () => {
    const rig = new Rig()
    await rig.ready()
    const x = await open(rig)
    const handshakes = rig.sent().filter((e) => e.kind === 'handshake').length
    rig.daemonReset()
    await settle()
    expect(x.events).toEqual(['failed:transport'])
    expect(rig.sent().filter((e) => e.kind === 'handshake')).toHaveLength(handshakes + 1)
    await rig.answerHandshake()
    expect(rig.tunnel.state.phase).toBe('ready')
    // 新传输：stream id 和消息编号都从头开始
    const mark = rig.sent().length
    await open(rig, '/again')
    const fresh = rig.readData(rig.socket, mark)
    expect(fresh).toHaveLength(1)
    expect(fresh[0]!.ack).toBe(0)
    expect(fresh[0]!.frames[0]).toMatchObject({ type: T.Request, stream: 1 })
  })

  it('answers an undecryptable message with RESET and a new handshake', async () => {
    const rig = new Rig()
    await rig.ready()
    const x = await open(rig)
    rig.daemonEnvelope(rig.socket, 'data', Uint8Array.of(0, 1, 2, 3, 4, 5, 6, 7, 8))
    await settle()
    expect(x.events).toEqual(['failed:transport'])
    const sent = rig.sent()
    const resetAt = sent.findIndex((e) => e.kind === 'data' && e.body.length === 1 && e.body[0] === 2)
    expect(resetAt).toBeGreaterThan(0)
    expect(sent.slice(resetAt).some((e) => e.kind === 'handshake')).toBe(true)
  })

  it('ignores envelopes that do not come from the machine', async () => {
    const rig = new Rig()
    await rig.ready()
    const x = await open(rig)
    const ct = rig.peer!.write([responseFrame(1, 200)], 1)
    const env = encodeEnvelopeFrom('ffffffffffffffffffffffffffffffff', ct)
    rig.socket.deliver(env)
    await settle()
    expect(x.events).toEqual([])
  })

  it('ignores a late bounce for an envelope the machine has already acknowledged', async () => {
    const rig = new Rig()
    await rig.ready()
    await open(rig)
    const seq = rig.sent().filter((e) => e.kind === 'data')[0]!.seq
    rig.daemonSend([responseFrame(1, 200), endFrame(1)], 1)
    await settle()
    const before = rig.socket.sent.length
    rig.hubControl({ type: 'backpressure', to: NODE_ID, seq })
    await vi.advanceTimersByTimeAsync(1_000)
    expect(rig.tunnel.state.phase).toBe('ready')
    // 回执晚到的那条没有被重发
    expect(rig.socket.sent.slice(before).some((b) => decodeEnvelope(b).seq === seq)).toBe(false)
  })

  it('a bounce for an envelope that was never acknowledged but is gone resets the transport', async () => {
    const rig = new Rig()
    await rig.ready()
    await open(rig)
    const seq = rig.sent().filter((e) => e.kind === 'data')[0]!.seq
    rig.hubControl({ type: 'undeliverable', to: NODE_ID, seq }) // 发送方丢了这份原文
    rig.hubControl({ type: 'backpressure', to: NODE_ID, seq })
    await settle()
    expect(rig.sent().filter((e) => e.kind === 'handshake')).toHaveLength(2)
  })
})

function encodeEnvelopeFrom(from: string, body: Uint8Array): Uint8Array {
  const frames = new Uint8Array(1 + body.length)
  frames.set(body, 1)
  return encodeEnvelope(makeEnvelope(from, SELF_ID, 'data', 1, frames))
}
