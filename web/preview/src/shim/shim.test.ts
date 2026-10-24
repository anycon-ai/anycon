// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PageMessage, WorkerMessage } from '../shared/messages'
import type { Channel } from './channel'
import { serviceWorkerChannel } from './channel'
import { installCookies } from './cookies'
import { KEEPALIVE_INTERVAL_MS, installShim } from './install'
import { blockServiceWorkerRegistration } from './serviceWorker'
import { installWebSocket } from './websocket'

class FakeChannel implements Channel {
  posted: PageMessage[] = []
  transfers: Array<Transferable[] | undefined> = []
  controlled = true
  private listeners = new Set<(m: WorkerMessage) => void>()
  post(message: PageMessage, transfer?: Transferable[]): boolean {
    if (!this.controlled) return false
    this.posted.push(message)
    this.transfers.push(transfer)
    return true
  }
  subscribe(l: (m: WorkerMessage) => void): () => void {
    this.listeners.add(l)
    return () => this.listeners.delete(l)
  }
  emit(m: WorkerMessage): void {
    for (const l of [...this.listeners]) l(m)
  }
  of<T extends PageMessage['type']>(type: T): Array<Extract<PageMessage, { type: T }>> {
    return this.posted.filter((m): m is Extract<PageMessage, { type: T }> => m.type === type)
  }
}

const w = window as unknown as Window & typeof globalThis
const HOST = window.location.host

/** 原生 WebSocket 的替身：只记录有人用它建过连接，不真的联网。 */
class NativeStub extends EventTarget {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3
  static created: Array<{ url: string; protocols: unknown }> = []
  constructor(
    readonly url: string,
    readonly protocols?: unknown,
  ) {
    super()
    NativeStub.created.push({ url, protocols })
  }
}

let realWebSocket: typeof WebSocket
beforeEach(() => {
  realWebSocket = w.WebSocket
  w.WebSocket = NativeStub as unknown as typeof WebSocket
  NativeStub.created = []
})
afterEach(() => {
  w.WebSocket = realWebSocket
  vi.useRealTimers()
})

const flush = async (): Promise<void> => {
  for (let i = 0; i < 50; i++) await Promise.resolve()
}

function wsSetup() {
  const channel = new FakeChannel()
  const uninstall = installWebSocket(w, channel)
  const WS = w.WebSocket
  const open = (url = `ws://${HOST}/hmr`, protocols?: string | string[]) => {
    const ws = new WS(url, protocols)
    const id = channel.of('ws.open').at(-1)!.id
    return { ws, id }
  }
  return { channel, uninstall, WS, open }
}

describe('WebSocket replacement: choosing', () => {
  it('tunnels same-host ws / wss / http / https URLs and resolves relative ones', () => {
    const { channel, open, uninstall } = wsSetup()
    open(`ws://${HOST}/a?x=1`)
    open(`wss://${HOST}/b`)
    new w.WebSocket(`http://${HOST}/c`)
    new w.WebSocket('/d')
    expect(channel.of('ws.open').map((m) => m.url)).toEqual([`ws://${HOST}/a?x=1`, `wss://${HOST}/b`, `ws://${HOST}/c`, `ws://${HOST}/d`])
    expect(NativeStub.created).toEqual([])
    uninstall()
  })

  it('uses the native WebSocket for other hosts', () => {
    const { channel, uninstall } = wsSetup()
    const ws = new w.WebSocket('wss://elsewhere.example/ws', ['p'])
    expect(ws).toBeInstanceOf(NativeStub)
    expect(NativeStub.created).toEqual([{ url: 'wss://elsewhere.example/ws', protocols: ['p'] }])
    expect(channel.posted).toEqual([])
    uninstall()
  })

  it('lets the native constructor report URLs that are not valid', () => {
    const { uninstall } = wsSetup()
    new w.WebSocket('ftp://x.example/')
    expect(NativeStub.created).toHaveLength(1)
    uninstall()
  })

  it('keeps the static constants and instanceof working', () => {
    const { open, WS, uninstall } = wsSetup()
    expect([WS.CONNECTING, WS.OPEN, WS.CLOSING, WS.CLOSED]).toEqual([0, 1, 2, 3])
    const { ws } = open()
    expect(ws).toBeInstanceOf(WS)
    expect(ws.CONNECTING).toBe(0)
    expect(ws).toBeInstanceOf(EventTarget)
    uninstall()
  })

  it('restores the native WebSocket when uninstalled', () => {
    const { uninstall } = wsSetup()
    uninstall()
    expect(w.WebSocket).toBe(NativeStub)
  })
})

describe('WebSocket replacement: validation like the native one', () => {
  it('rejects duplicate and invalid subprotocols, and leaves a fragment URL to the native constructor to reject', () => {
    const { uninstall } = wsSetup()
    new w.WebSocket(`ws://${HOST}/x#frag`)
    expect(NativeStub.created).toHaveLength(1)
    expect(() => new w.WebSocket(`ws://${HOST}/x`, ['a', 'a'])).toThrow(expect.objectContaining({ name: 'SyntaxError' }))
    expect(() => new w.WebSocket(`ws://${HOST}/x`, 'bad protocol')).toThrow(expect.objectContaining({ name: 'SyntaxError' }))
    uninstall()
  })

  it('passes the subprotocols along', () => {
    const { channel, open, uninstall } = wsSetup()
    open(`ws://${HOST}/x`, ['vite-hmr', 'other'])
    open(`ws://${HOST}/y`, 'single')
    expect(channel.of('ws.open').map((m) => m.protocols)).toEqual([['vite-hmr', 'other'], ['single']])
    uninstall()
  })

  it('validates close() arguments', () => {
    const { open, uninstall } = wsSetup()
    const { ws } = open()
    expect(() => ws.close(1001)).toThrow(expect.objectContaining({ name: 'InvalidAccessError' }))
    expect(() => ws.close(2999)).toThrow(expect.objectContaining({ name: 'InvalidAccessError' }))
    expect(() => ws.close(1000, 'é'.repeat(62))).toThrow(expect.objectContaining({ name: 'SyntaxError' }))
    expect(ws.readyState).toBe(0)
    uninstall()
  })
})

describe('WebSocket replacement: life cycle', () => {
  it('goes CONNECTING → OPEN with the negotiated protocol, then fires events to on* handlers and listeners', async () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open(`ws://${HOST}/hmr`, ['vite-hmr'])
    const seen: string[] = []
    ws.onopen = () => seen.push('onopen')
    ws.addEventListener('open', () => seen.push('listener'))
    expect(ws.readyState).toBe(0)
    channel.emit({ type: 'ws.open', id, protocol: 'vite-hmr' })
    expect(ws.readyState).toBe(1)
    expect(ws.protocol).toBe('vite-hmr')
    expect(ws.extensions).toBe('')
    expect(ws.url).toBe(`ws://${HOST}/hmr`)
    expect(seen).toEqual(['onopen', 'listener'])
    uninstall()
  })

  it('delivers text, ArrayBuffer and Blob messages according to binaryType', async () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open()
    channel.emit({ type: 'ws.open', id, protocol: '' })
    const got: unknown[] = []
    ws.onmessage = (e) => got.push(e.data)
    channel.emit({ type: 'ws.message', id, binary: false, data: 'hi' })
    channel.emit({ type: 'ws.message', id, binary: true, data: new Uint8Array([1, 2]).buffer })
    ws.binaryType = 'arraybuffer'
    channel.emit({ type: 'ws.message', id, binary: true, data: new Uint8Array([3]).buffer })
    expect(got[0]).toBe('hi')
    expect(got[1]).toBeInstanceOf(Blob)
    expect(got[2]).toBeInstanceOf(ArrayBuffer)
    expect(Array.from(new Uint8Array(got[2] as ArrayBuffer))).toEqual([3])
    ws.binaryType = 'bogus' as never
    expect(ws.binaryType).toBe('arraybuffer')
    uninstall()
  })

  it('routes messages by socket id', () => {
    const { channel, open, uninstall } = wsSetup()
    const a = open()
    const b = open(`ws://${HOST}/other`)
    channel.emit({ type: 'ws.open', id: b.id, protocol: '' })
    expect([a.ws.readyState, b.ws.readyState]).toEqual([0, 1])
    uninstall()
  })

  it('throws when sending while connecting, ignores sends after close', async () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open()
    expect(() => ws.send('x')).toThrow(expect.objectContaining({ name: 'InvalidStateError' }))
    channel.emit({ type: 'ws.open', id, protocol: '' })
    ws.close()
    ws.send('late')
    await flush()
    expect(channel.of('ws.send')).toEqual([])
    uninstall()
  })

  it('close() moves to CLOSING, asks the worker to close, and ends with a close event carrying the worker’s code', async () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open()
    channel.emit({ type: 'ws.open', id, protocol: '' })
    const events: Array<[number, string, boolean]> = []
    ws.onclose = (e) => events.push([e.code, e.reason, e.wasClean])
    ws.close(4000, 'bye')
    await flush()
    expect(ws.readyState).toBe(2)
    expect(channel.of('ws.close')).toEqual([{ type: 'ws.close', id, code: 4000, reason: 'bye' }])
    ws.close() // 再关一次不重复发
    await flush()
    expect(channel.of('ws.close')).toHaveLength(1)
    channel.emit({ type: 'ws.close', id, code: 4000, reason: 'bye', wasClean: true })
    expect(ws.readyState).toBe(3)
    expect(events).toEqual([[4000, 'bye', true]])
    // 关了就不再收消息
    channel.emit({ type: 'ws.close', id, code: 1006, reason: '', wasClean: false })
    expect(events).toHaveLength(1)
    uninstall()
  })

  it('an error from the worker fires error and then close(1006)', () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open()
    const order: string[] = []
    ws.onerror = () => order.push('error')
    ws.onclose = (e) => order.push(`close ${e.code} ${e.wasClean}`)
    channel.emit({ type: 'ws.error', id })
    channel.emit({ type: 'ws.close', id, code: 1006, reason: '', wasClean: false })
    expect(order).toEqual(['error', 'close 1006 false'])
    uninstall()
  })

  it('without a controlling service worker fails like a refused connection', async () => {
    const { channel, WS, uninstall } = wsSetup()
    channel.controlled = false
    const ws = new WS(`ws://${HOST}/x`)
    const order: string[] = []
    ws.onerror = () => order.push('error')
    ws.onclose = (e) => order.push(`close ${e.code}`)
    expect(ws.readyState).toBe(0)
    await flush()
    expect(order).toEqual(['error', 'close 1006'])
    expect(ws.readyState).toBe(3)
    uninstall()
  })

  it('tells the worker to close open sockets when the page is hidden for good', () => {
    const { channel, open, uninstall } = wsSetup()
    const { ws, id } = open()
    channel.emit({ type: 'ws.open', id, protocol: '' })
    w.dispatchEvent(new Event('pagehide'))
    expect(channel.of('ws.close')).toEqual([{ type: 'ws.close', id, code: 1001, reason: 'going away' }])
    expect(ws.readyState).toBe(1) // 页面走了，事件不会再有人听
    uninstall()
  })
})

describe('WebSocket replacement: sending', () => {
  async function opened() {
    const t = wsSetup()
    const { ws, id } = t.open()
    t.channel.emit({ type: 'ws.open', id, protocol: '' })
    return { ...t, ws, id }
  }

  it('sends text as text and binary as a copied ArrayBuffer, in order', async () => {
    const { channel, ws, id, uninstall } = await opened()
    const src = new Uint8Array([9, 8, 7])
    ws.send('hello')
    ws.send(src)
    ws.send(src.subarray(1))
    ws.send(new Uint8Array([1, 2, 3, 4]).buffer)
    await flush()
    const sent = channel.of('ws.send')
    expect(sent.map((m) => [m.id, m.binary, typeof m.data === 'string' ? m.data : Array.from(new Uint8Array(m.data))])).toEqual([
      [id, false, 'hello'],
      [id, true, [9, 8, 7]],
      [id, true, [8, 7]],
      [id, true, [1, 2, 3, 4]],
    ])
    src[0] = 0 // 发出去的是拷贝
    expect(Array.from(new Uint8Array(sent[1]!.data as ArrayBuffer))).toEqual([9, 8, 7])
    uninstall()
  })

  it('keeps the order when a Blob is in the middle', async () => {
    const { channel, ws, uninstall } = await opened()
    ws.send('first')
    ws.send(new Blob([new Uint8Array([5, 6])]))
    ws.send('last')
    await new Promise((r) => setTimeout(r, 20))
    expect(channel.of('ws.send').map((m) => (typeof m.data === 'string' ? m.data : Array.from(new Uint8Array(m.data))))).toEqual(['first', [5, 6], 'last'])
    uninstall()
  })

  it('counts unsent bytes in bufferedAmount until they are handed over', async () => {
    const { ws, uninstall } = await opened()
    ws.send('héllo') // 6 字节
    ws.send(new Uint8Array(10))
    expect(ws.bufferedAmount).toBe(16)
    await flush()
    expect(ws.bufferedAmount).toBe(0)
    uninstall()
  })
})

describe('document.cookie', () => {
  let doc: Document
  let restore: () => void
  const channel = new FakeChannel()
  beforeEach(() => {
    doc = document
    channel.posted = []
    channel.controlled = true
    window.history.replaceState(null, '', '/')
  })
  afterEach(() => restore?.())

  const install = (initial = '') => {
    restore = installCookies(doc, window.location, channel, initial)
  }

  it('starts from the cookies the worker rendered into the page and asks for a full snapshot', () => {
    install('a=1; b=2')
    expect(doc.cookie).toBe('a=1; b=2')
    expect(channel.posted).toEqual([{ type: 'cookies.sync' }])
  })

  it('applies writes locally at once and hands the raw assignment to the worker', () => {
    install()
    doc.cookie = 'theme=dark; path=/'
    expect(doc.cookie).toBe('theme=dark')
    expect(channel.of('cookie.set')).toEqual([{ type: 'cookie.set', value: 'theme=dark; path=/' }])
    doc.cookie = 'theme=; max-age=0; path=/'
    expect(doc.cookie).toBe('')
  })

  it('never writes the browser’s own cookie jar', () => {
    install()
    doc.cookie = 'leak=1; path=/'
    restore()
    expect(doc.cookie).not.toContain('leak')
  })

  it('does not forward writes that change nothing or that scripts may not make', () => {
    install()
    doc.cookie = 'x=1; HttpOnly'
    doc.cookie = 'nothing; no equals'
    doc.cookie = 'gone=; max-age=0'
    expect(channel.of('cookie.set')).toEqual([])
    expect(doc.cookie).toBe('')
  })

  it('applies the document path: a cookie without Path belongs to the directory it was set in', () => {
    install()
    window.history.replaceState(null, '', '/docs/page')
    doc.cookie = 'k=v'
    expect(doc.cookie).toBe('k=v')
    window.history.replaceState(null, '', '/elsewhere')
    expect(doc.cookie).toBe('')
    window.history.replaceState(null, '', '/docs/other')
    expect(doc.cookie).toBe('k=v')
  })

  it('takes the worker’s snapshot when it changes (cookies set by responses)', () => {
    install('old=1')
    channel.emit({
      type: 'cookies',
      cookies: [{ name: 'sid', value: 'abc', path: '/', expiresAt: null, createdAt: Date.now(), httpOnly: false }],
    })
    expect(doc.cookie).toBe('sid=abc')
  })

  it('restores the original property when removed', () => {
    install('a=1')
    restore()
    expect(Object.getOwnPropertyDescriptor(doc, 'cookie')).toBeUndefined()
  })
})

describe('service worker registration', () => {
  function fakeContainer() {
    const register = vi.fn()
    const container = { register, getRegistration: () => Promise.resolve({}), getRegistrations: () => Promise.resolve([{}]) } as unknown as ServiceWorkerContainer
    return { container, register }
  }

  it('refuses register() with a warning and hides the preview’s registration from queries', async () => {
    const { container, register } = fakeContainer()
    const warn = vi.fn()
    const undo = blockServiceWorkerRegistration(container, warn)
    await expect(container.register('/sw.js')).rejects.toMatchObject({ name: 'SecurityError' })
    expect(warn).toHaveBeenCalledWith(expect.stringContaining('cannot register its own service worker'))
    expect(register).not.toHaveBeenCalled()
    expect(await container.getRegistrations()).toEqual([])
    expect(await container.getRegistration()).toBeUndefined()
    undo()
    expect((await container.getRegistrations()).length).toBe(1)
  })

  it('does nothing in a browser without service workers', () => {
    expect(() => blockServiceWorkerRegistration(undefined, () => {})()).not.toThrow()
  })
})

describe('installShim', () => {
  it('wires everything, keeps the worker alive every 20 seconds and installs only once', () => {
    vi.useFakeTimers()
    const channel = new FakeChannel()
    const undo = installShim(w, { cookies: 'a=1', channel })
    expect(document.cookie).toBe('a=1')
    expect(w.WebSocket).not.toBe(NativeStub)
    expect(installShim(w, { cookies: 'ignored', channel })()).toBeUndefined() // 第二次是空操作
    expect(document.cookie).toBe('a=1')
    vi.advanceTimersByTime(KEEPALIVE_INTERVAL_MS * 2)
    expect(channel.of('keepalive')).toHaveLength(2)
    undo()
    vi.advanceTimersByTime(KEEPALIVE_INTERVAL_MS * 2)
    expect(channel.of('keepalive')).toHaveLength(2)
    expect(w.WebSocket).toBe(NativeStub)
  })
})

describe('serviceWorkerChannel', () => {
  it('posts to the controller and parses what comes back', () => {
    const listeners: Array<(e: MessageEvent) => void> = []
    const posted: unknown[] = []
    const container = {
      controller: { postMessage: (m: unknown) => posted.push(m) },
      addEventListener: (_: string, l: (e: MessageEvent) => void) => listeners.push(l),
      removeEventListener: (_: string, l: (e: MessageEvent) => void) => listeners.splice(listeners.indexOf(l), 1),
    } as unknown as ServiceWorkerContainer
    const channel = serviceWorkerChannel(container)
    expect(channel.post({ type: 'keepalive' })).toBe(true)
    expect(posted).toEqual([{ type: 'keepalive' }])
    const got: WorkerMessage[] = []
    const off = channel.subscribe((m) => got.push(m))
    listeners[0]!({ data: { type: 'ws.error', id: 3 } } as MessageEvent)
    listeners[0]!({ data: { type: 'nonsense' } } as MessageEvent)
    listeners[0]!({ data: 'junk' } as MessageEvent)
    expect(got).toEqual([{ type: 'ws.error', id: 3 }])
    off()
    expect(listeners).toHaveLength(0)
    const orphan = serviceWorkerChannel({ controller: null } as unknown as ServiceWorkerContainer)
    expect(orphan.post({ type: 'keepalive' })).toBe(false)
  })
})
