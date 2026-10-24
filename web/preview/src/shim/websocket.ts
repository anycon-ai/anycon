// 页面里的 `WebSocket` 替换类（spec/web.md § 13.2 shim「WebSocket」）。
//
// 同 origin 的 `ws:` / `wss:` 地址经 service worker 走隧道（`ws.open` / `ws.send` / `ws.close`，worker 回
// `ws.open` / `ws.message` / `ws.close` / `ws.error`）；别的地址用原生的。实现 `readyState`、`protocol`、
// `extensions`、`binaryType`（`blob` / `arraybuffer`）、`bufferedAmount`、`send`（string / ArrayBuffer / TypedArray / Blob）、
// `close`、`on*` 和 `addEventListener`。
import type { WorkerMessage } from '../shared/messages'
import type { Channel } from './channel'

type Win = Window & typeof globalThis

const CONNECTING = 0
const OPEN = 1
const CLOSING = 2
const CLOSED = 3

/** 关闭原因的字节上限（RFC 6455）。 */
const REASON_MAX = 123
const TOKEN = /^[\x21\x23-\x27\x2a\x2b\x2d\x2e\x30-\x39\x41-\x5a\x5e-\x7a\x7c\x7e]+$/

export function installWebSocket(win: Win, channel: Channel): () => void {
  const Native = win.WebSocket
  const sockets = new Map<number, TunnelWebSocket>()
  let nextId = 1

  /** 把页面给的地址按原生 `WebSocket` 的规则解析：http(s) 换成 ws(s)，不能带 fragment。 */
  function resolve(url: string | URL): URL {
    const u = new win.URL(String(url), win.location.href)
    if (u.protocol === 'http:') u.protocol = 'ws:'
    else if (u.protocol === 'https:') u.protocol = 'wss:'
    if (u.protocol !== 'ws:' && u.protocol !== 'wss:') throw new win.DOMException(`The URL's scheme must be either 'ws' or 'wss'.`, 'SyntaxError')
    if (u.hash !== '') throw new win.DOMException('The URL contains a fragment identifier.', 'SyntaxError')
    return u
  }

  function parseProtocols(protocols: string | string[] | undefined): string[] {
    const list = protocols === undefined ? [] : typeof protocols === 'string' ? [protocols] : Array.from(protocols, String)
    for (const p of list) {
      if (!TOKEN.test(p)) throw new win.DOMException(`The subprotocol '${p}' is invalid.`, 'SyntaxError')
    }
    if (new Set(list).size !== list.length) throw new win.DOMException('The subprotocol is duplicated.', 'SyntaxError')
    return list
  }

  class TunnelWebSocket extends win.EventTarget {
    static readonly CONNECTING = CONNECTING
    static readonly OPEN = OPEN
    static readonly CLOSING = CLOSING
    static readonly CLOSED = CLOSED
    readonly CONNECTING = CONNECTING
    readonly OPEN = OPEN
    readonly CLOSING = CLOSING
    readonly CLOSED = CLOSED

    readonly url: string
    readyState: number = CONNECTING
    protocol = ''
    readonly extensions = ''
    private binary: BinaryType = 'blob'
    private buffered = 0
    private readonly id: number
    private readonly handlers: Record<'open' | 'message' | 'error' | 'close', ((ev: Event) => unknown) | null> = {
      open: null,
      message: null,
      error: null,
      close: null,
    }
    /** 保证页面发出去的顺序：Blob 要异步读，后面的消息得排在它后面。 */
    private sendChain: Promise<void> = Promise.resolve()

    constructor(protocols: string | string[] | undefined, resolved: URL) {
      super()
      this.url = resolved.href
      this.id = nextId++
      for (const type of ['open', 'message', 'error', 'close'] as const) {
        this.addEventListener(type, (ev) => this.handlers[type]?.call(this, ev))
      }
      const list = parseProtocols(protocols)
      sockets.set(this.id, this)
      if (!channel.post({ type: 'ws.open', id: this.id, url: this.url, protocols: list })) {
        // 页面没有被 service worker 控制：和连不上一样
        queueMicrotask(() => this.fail())
      }
    }

    get binaryType(): BinaryType {
      return this.binary
    }
    set binaryType(value: BinaryType) {
      if (value === 'blob' || value === 'arraybuffer') this.binary = value
    }
    get bufferedAmount(): number {
      return this.buffered
    }

    get onopen(): ((ev: Event) => unknown) | null {
      return this.handlers.open
    }
    set onopen(fn: ((ev: Event) => unknown) | null) {
      this.handlers.open = typeof fn === 'function' ? fn : null
    }
    get onmessage(): ((ev: MessageEvent) => unknown) | null {
      return this.handlers.message as ((ev: MessageEvent) => unknown) | null
    }
    set onmessage(fn: ((ev: MessageEvent) => unknown) | null) {
      this.handlers.message = typeof fn === 'function' ? (fn as (ev: Event) => unknown) : null
    }
    get onerror(): ((ev: Event) => unknown) | null {
      return this.handlers.error
    }
    set onerror(fn: ((ev: Event) => unknown) | null) {
      this.handlers.error = typeof fn === 'function' ? fn : null
    }
    get onclose(): ((ev: CloseEvent) => unknown) | null {
      return this.handlers.close as ((ev: CloseEvent) => unknown) | null
    }
    set onclose(fn: ((ev: CloseEvent) => unknown) | null) {
      this.handlers.close = typeof fn === 'function' ? (fn as (ev: Event) => unknown) : null
    }

    send(data: string | ArrayBufferLike | ArrayBufferView | Blob): void {
      if (this.readyState === CONNECTING) throw new win.DOMException('Still in CONNECTING state.', 'InvalidStateError')
      if (this.readyState !== OPEN) return
      if (typeof data === 'string') {
        const size = new win.TextEncoder().encode(data).length
        return this.enqueue(size, () => channel.post({ type: 'ws.send', id: this.id, binary: false, data }))
      }
      if (typeof win.Blob !== 'undefined' && data instanceof win.Blob) {
        // Blob 要异步读：后面的消息排在它后面
        return this.enqueue(data.size, async () => {
          const bytes = await data.arrayBuffer()
          channel.post({ type: 'ws.send', id: this.id, binary: true, data: bytes }, [bytes])
        })
      }
      const copy = ArrayBuffer.isView(data) ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : (data as ArrayBuffer).slice(0)
      this.enqueue(copy.byteLength, () => channel.post({ type: 'ws.send', id: this.id, binary: true, data: copy as ArrayBuffer }, [copy as ArrayBuffer]))
    }

    /** 按页面发出的顺序交给 service worker；交出去之前这些字节算在 `bufferedAmount` 里。 */
    private enqueue(size: number, send: () => unknown): void {
      this.buffered += size
      this.sendChain = this.sendChain
        .then(send)
        .catch(() => {})
        .then(() => {
          this.buffered = Math.max(0, this.buffered - size)
        })
    }

    close(code?: number, reason?: string): void {
      if (code !== undefined && code !== 1000 && !(code >= 3000 && code <= 4999)) {
        throw new win.DOMException(`The code must be either 1000, or between 3000 and 4999. ${code} is neither.`, 'InvalidAccessError')
      }
      if (reason !== undefined && new win.TextEncoder().encode(reason).length > REASON_MAX) {
        throw new win.DOMException('The message must not be greater than 123 bytes.', 'SyntaxError')
      }
      if (this.readyState === CLOSING || this.readyState === CLOSED) return
      this.readyState = CLOSING
      this.sendChain = this.sendChain.then(() => {
        channel.post({ type: 'ws.close', id: this.id, code, reason })
      })
    }

    /** service worker 发来的消息。 */
    receive(message: WorkerMessage): void {
      switch (message.type) {
        case 'ws.open':
          if (this.readyState !== CONNECTING) return
          this.readyState = OPEN
          this.protocol = message.protocol
          this.dispatchEvent(new win.Event('open'))
          return
        case 'ws.message': {
          if (this.readyState !== OPEN && this.readyState !== CLOSING) return
          let data: string | ArrayBuffer | Blob = message.data
          if (message.binary && typeof data !== 'string' && this.binary === 'blob') data = new win.Blob([data])
          this.dispatchEvent(new win.MessageEvent('message', { data, origin: win.location.origin }))
          return
        }
        case 'ws.error':
          this.dispatchEvent(new win.Event('error'))
          return
        case 'ws.close':
          this.finish(message.code, message.reason, message.wasClean)
          return
        default:
          return
      }
    }

    private fail(): void {
      if (this.readyState === CLOSED) return
      this.dispatchEvent(new win.Event('error'))
      this.finish(1006, '', false)
    }

    finish(code: number, reason: string, wasClean: boolean): void {
      if (this.readyState === CLOSED) return
      this.readyState = CLOSED
      sockets.delete(this.id)
      this.dispatchEvent(new win.CloseEvent('close', { code, reason, wasClean }))
    }

    /** 页面要走了：告诉 worker 关掉，免得它一直占着隧道。 */
    unload(): void {
      if (this.readyState === CLOSED) return
      channel.post({ type: 'ws.close', id: this.id, code: 1001, reason: 'going away' })
    }
  }

  // 原型链接到原生 WebSocket.prototype：`socket instanceof WebSocket` 仍然成立（上面所有成员都自己实现了，不会落到原生的 getter 上）
  if (Native.prototype instanceof win.EventTarget) Object.setPrototypeOf(TunnelWebSocket.prototype, Native.prototype)

  const unsubscribe = channel.subscribe((message) => {
    if ('id' in message) sockets.get(message.id)?.receive(message)
  })
  const onPageHide = (): void => {
    for (const s of sockets.values()) s.unload()
  }
  win.addEventListener('pagehide', onPageHide)

  const Replacement = new Proxy(Native, {
    construct(target, args: [string | URL, (string | string[])?], newTarget) {
      const [url, protocols] = args
      let resolved: URL
      try {
        resolved = resolve(url)
      } catch {
        // 地址本身不合法：让原生的构造函数抛它自己的错
        return Reflect.construct(target, args, newTarget)
      }
      if (resolved.host !== win.location.host) return Reflect.construct(target, args, newTarget)
      return new TunnelWebSocket(protocols, resolved)
    },
  })
  win.WebSocket = Replacement

  return () => {
    unsubscribe()
    win.removeEventListener('pagehide', onPageHide)
    win.WebSocket = Native
  }
}
