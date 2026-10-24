import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { WorkerMessage } from '../shared/messages'
import { TestDaemon, flushMicrotasks } from '../test/daemon'
import { MemoryCookies } from '../test/memory'
import { WsBridge, type ClientLike } from './bridge'
import { CookieStore } from './cookies'
import { TunnelUnavailable } from './tunnel/link'

const ORIGIN = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app'

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] }))
afterEach(() => vi.useRealTimers())

class FakeClient implements ClientLike {
  readonly messages: WorkerMessage[] = []
  constructor(readonly id: string) {}
  postMessage(message: WorkerMessage): void {
    this.messages.push(message)
  }
  types(): string[] {
    return this.messages.map((m) => m.type)
  }
}

function setup(over: { ready?: () => Promise<unknown> } = {}) {
  const daemon = new TestDaemon(() => ({}))
  const cookies = new CookieStore(new MemoryCookies())
  const bridge = new WsBridge({
    whenReady: (over.ready ?? (() => Promise.resolve(daemon.session))) as () => Promise<typeof daemon.session>,
    cookies,
    selfOrigin: ORIGIN,
    userAgent: 'UA',
    languages: ['en'],
  })
  const client = new FakeClient('c1')
  const open = async (id = 1, url = `wss://${new URL(ORIGIN).host}/?token=abc`, protocols: string[] = []) => {
    await bridge.onMessage(client, { type: 'ws.open', id, url, protocols })
    await flushMicrotasks()
  }
  return { daemon, cookies, bridge, client, open }
}

describe('opening', () => {
  it('opens a websocket stream with origin, cookies and subprotocols, and tells the page when upgraded', async () => {
    const t = setup()
    t.daemon.wsProtocol = 'vite-hmr'
    await t.open(1, `wss://${new URL(ORIGIN).host}/hmr?token=abc`, ['vite-hmr', 'other'])
    expect(t.client.messages).toEqual([{ type: 'ws.open', id: 1, protocol: 'vite-hmr' }])
  })

  it('sends a proper upgrade request head', async () => {
    const t = setup()
    const heads: unknown[] = []
    const orig = t.daemon.session.openStream.bind(t.daemon.session)
    t.daemon.session.openStream = async (head, listener, body) => {
      heads.push(head)
      return orig(head, listener, body)
    }
    await t.cookies.storeSetCookies(['sid=1; HttpOnly'], '/')
    await t.open(1, `wss://${new URL(ORIGIN).host}/hmr?token=abc`, ['vite-hmr', 'other'])
    expect(heads).toHaveLength(1)
    const head = heads[0] as { method: string; path: string; websocket: boolean; headers: [string, string][] }
    expect(head).toMatchObject({ method: 'GET', path: '/hmr?token=abc', websocket: true })
    expect(Object.fromEntries(head.headers)).toMatchObject({
      origin: ORIGIN,
      cookie: 'sid=1',
      'sec-websocket-protocol': 'vite-hmr, other',
      'user-agent': 'UA',
    })
  })

  it('accepts http(s) URLs for the same host and relative URLs', async () => {
    const t = setup()
    await t.open(1, `${ORIGIN}/a`)
    await t.open(2, '/b')
    expect(t.client.types()).toEqual(['ws.open', 'ws.open'])
  })

  it('refuses another host, another scheme, or garbage', async () => {
    const t = setup()
    await t.open(1, 'wss://elsewhere.example/ws')
    await t.open(2, 'ftp://' + new URL(ORIGIN).host)
    await t.open(3, 'http://[bad')
    expect(t.client.messages.map((m) => [m.type, 'id' in m ? m.id : -1])).toEqual([
      ['ws.error', 1],
      ['ws.close', 1],
      ['ws.error', 2],
      ['ws.close', 2],
      ['ws.error', 3],
      ['ws.close', 3],
    ])
    expect(t.client.messages.filter((m) => m.type === 'ws.close').every((m) => m.type === 'ws.close' && m.code === 1006 && !m.wasClean)).toBe(true)
    expect(t.bridge.openCount).toBe(0)
  })

  it('fails the socket when the tunnel is unavailable', async () => {
    const t = setup({ ready: () => Promise.reject(new TunnelUnavailable('node_offline')) })
    await t.open()
    expect(t.client.types()).toEqual(['ws.error', 'ws.close'])
  })

  it('fails the socket when the daemon refuses the upgrade', async () => {
    const t = setup()
    t.daemon.wsMode = 'reject'
    await t.open()
    expect(t.client.types()).toEqual(['ws.error', 'ws.close'])
    expect(t.bridge.openCount).toBe(0)
    expect(t.daemon.session.openStreams).toBe(0)
  })

  it('ignores a second open with the same id from the same page', async () => {
    const t = setup()
    await t.open(1)
    await t.open(1)
    expect(t.client.types()).toEqual(['ws.open'])
  })
})

describe('messages', () => {
  it('delivers text and binary messages to the page and gives the window back', async () => {
    const t = setup()
    await t.open()
    t.daemon.wsSend(1, 'hello')
    t.daemon.wsSend(1, new Uint8Array([1, 2, 3]))
    await flushMicrotasks()
    const [, text, binary] = t.client.messages
    expect(text).toEqual({ type: 'ws.message', id: 1, binary: false, data: 'hello' })
    expect(binary).toMatchObject({ type: 'ws.message', id: 1, binary: true })
    expect(new Uint8Array((binary as { data: ArrayBuffer }).data)).toEqual(new Uint8Array([1, 2, 3]))
  })

  it('sends page messages to the daemon, text as text and ArrayBuffers as binary', async () => {
    const t = setup()
    await t.open()
    await t.bridge.onMessage(t.client, { type: 'ws.send', id: 1, binary: false, data: '{"type":"ping"}' })
    await t.bridge.onMessage(t.client, { type: 'ws.send', id: 1, binary: true, data: new Uint8Array([9, 8]).buffer })
    await flushMicrotasks()
    expect(t.daemon.wsEvents.map((e) => [e.kind, e.binary, e.data && Array.from(e.data)])).toEqual([
      ['message', false, Array.from(new TextEncoder().encode('{"type":"ping"}'))],
      ['message', true, [9, 8]],
    ])
  })

  it('does not send before the socket is open or after it closed', async () => {
    const t = setup()
    await t.bridge.onMessage(t.client, { type: 'ws.send', id: 7, binary: false, data: 'x' })
    await t.open()
    await t.bridge.onMessage(t.client, { type: 'ws.close', id: 1, code: 1000, reason: '' })
    await flushMicrotasks()
    await t.bridge.onMessage(t.client, { type: 'ws.send', id: 1, binary: false, data: 'late' })
    await flushMicrotasks()
    expect(t.daemon.wsEvents.filter((e) => e.kind === 'message')).toEqual([])
  })
})

describe('closing', () => {
  it('daemon closes first: page gets a clean close with the daemon’s code and reason', async () => {
    const t = setup()
    await t.open()
    t.daemon.send([{ type: 8, stream: 1, payload: Uint8Array.of(0x03, 0xe9, ...new TextEncoder().encode('going away')) }])
    await flushMicrotasks()
    expect(t.client.messages.at(-1)).toEqual({ type: 'ws.close', id: 1, code: 1001, reason: 'going away', wasClean: true })
    expect(t.bridge.openCount).toBe(0)
  })

  it('page closes first: close frame goes out, the daemon’s reply ends it cleanly', async () => {
    const t = setup()
    await t.open()
    await t.bridge.onMessage(t.client, { type: 'ws.close', id: 1, code: 4000, reason: 'bye' })
    await flushMicrotasks()
    expect(t.daemon.wsEvents.at(-1)).toMatchObject({ kind: 'close', code: 4000, reason: 'bye' })
    expect(t.client.messages.at(-1)).toEqual({ type: 'ws.close', id: 1, code: 4000, reason: '', wasClean: true })
    expect(t.daemon.session.openStreams).toBe(0)
  })

  it('page close without a code means 1000', async () => {
    const t = setup()
    await t.open()
    await t.bridge.onMessage(t.client, { type: 'ws.close', id: 1 })
    await flushMicrotasks()
    expect(t.daemon.wsEvents.at(-1)).toMatchObject({ kind: 'close', code: 1000 })
  })

  it('gives up waiting for the daemon’s close after 5 seconds', async () => {
    const t = setup()
    await t.open()
    t.daemon.wsEvents.length = 0
    // 让 daemon 不回 close：把它的处理换掉
    ;(t.daemon as unknown as { onFrame: () => void }).onFrame = () => {}
    await t.bridge.onMessage(t.client, { type: 'ws.close', id: 1, code: 1000 })
    await flushMicrotasks()
    await vi.advanceTimersByTimeAsync(5_000)
    expect(t.client.messages.at(-1)).toMatchObject({ type: 'ws.close', code: 1006, wasClean: false })
  })

  it('a page that closes while connecting gets error + close 1006 and no stream is left', async () => {
    const t = setup({ ready: () => new Promise(() => {}) })
    void t.bridge.onMessage(t.client, { type: 'ws.open', id: 1, url: `wss://${new URL(ORIGIN).host}/`, protocols: [] })
    await flushMicrotasks()
    await t.bridge.onMessage(t.client, { type: 'ws.close', id: 1 })
    expect(t.client.types()).toEqual(['ws.error', 'ws.close'])
    expect(t.bridge.openCount).toBe(0)
  })

  it('a tunnel failure closes every socket abnormally', async () => {
    const t = setup()
    await t.open(1)
    await t.open(2)
    t.daemon.session.destroy({ kind: 'transport', message: 'hub gone' })
    expect(t.client.messages.filter((m) => m.type === 'ws.close').map((m) => m.type === 'ws.close' && [m.id, m.code, m.wasClean])).toEqual([
      [1, 1006, false],
      [2, 1006, false],
    ])
    expect(t.bridge.openCount).toBe(0)
  })

  it('reaps the sockets of pages that are gone', async () => {
    const t = setup()
    await t.open(1)
    const other = new FakeClient('c2')
    await t.bridge.onMessage(other, { type: 'ws.open', id: 1, url: `wss://${new URL(ORIGIN).host}/`, protocols: [] })
    await flushMicrotasks()
    expect(t.bridge.openCount).toBe(2)
    t.bridge.reap(new Set(['c2']))
    await flushMicrotasks()
    expect(t.bridge.openCount).toBe(1)
    expect(t.daemon.resets).toHaveLength(1)
    expect(t.client.types()).toEqual(['ws.open']) // 页面已经没了，不再打扰
  })
})
