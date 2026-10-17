import { isNodeRuntime } from './runtime.js'

/** 浏览器 `WebSocket` 与 `ws` 包的公共子集。 */
export interface WebSocketLike {
  binaryType: string
  readonly readyState: number
  send(data: Uint8Array): void
  close(code?: number, reason?: string): void
  onopen: ((event: unknown) => void) | null
  onmessage: ((event: { data: unknown }) => void) | null
  onclose: ((event: { code: number; reason: string }) => void) | null
  onerror: ((event: unknown) => void) | null
}

/** 建立 WebSocket 连接时的额外参数。 */
export interface WebSocketInit {
  /**
   * 升级请求要带的 HTTP 头（例如 `Cookie`、反向代理要求的 `CF-Access-Client-Id`）。
   * 只有 Node / Bun（`ws`）能设置；浏览器的原生 WebSocket 不允许自定义头，会被忽略（浏览器自己带同源 cookie）。
   */
  headers?: Record<string, string>
}

/** 自定义 WebSocket 构造（测试 / 特殊运行时）。 */
export type WebSocketFactory = (
  url: string,
  init?: WebSocketInit,
) => WebSocketLike | Promise<WebSocketLike>

export const WS_OPEN = 1

type WsConstructor = new (url: string, options?: object) => WebSocketLike

/**
 * 同构默认实现：浏览器 / Web Worker 用原生 `WebSocket`；Node / Bun 用 `ws` 包。
 * `ws` 用变量指定符动态导入，浏览器打包器不会去解析它（package.json 里 `browser: { ws: false }` 也兜底）。
 */
export const defaultWebSocket: WebSocketFactory = async (url, init) => {
  if (!isNodeRuntime()) {
    const Native = (globalThis as { WebSocket?: new (url: string) => WebSocketLike }).WebSocket
    if (!Native) throw new Error('no WebSocket implementation available in this environment')
    return new Native(url)
  }
  const specifier = 'ws'
  const mod = (await import(/* @vite-ignore */ specifier)) as {
    default?: WsConstructor
    WebSocket?: WsConstructor
  }
  const WS = mod.WebSocket ?? mod.default
  if (!WS) throw new Error('the `ws` package did not export a WebSocket constructor')
  return new WS(url, {
    perMessageDeflate: false,
    maxPayload: 16 * 1024 * 1024,
    ...(init?.headers ? { headers: init.headers } : {}),
  })
}

/**
 * hub 地址 → WebSocket 地址。
 * `https://h` → `wss://h/v1/ws`；`http://h:8080` → `ws://h:8080/v1/ws`；已是 `ws(s)://` 的保留；
 * 没有协议头的按 `https://` 处理；已经带 `/v1/ws` 的不重复追加。
 */
export function toWsUrl(hubUrl: string): string {
  const url = new URL(hubUrl.includes('://') ? hubUrl : `https://${hubUrl}`)
  if (url.protocol === 'https:') url.protocol = 'wss:'
  else if (url.protocol === 'http:') url.protocol = 'ws:'
  else if (url.protocol !== 'ws:' && url.protocol !== 'wss:') {
    throw new Error(`unsupported hub URL scheme: ${url.protocol}`)
  }
  url.hash = ''
  url.search = ''
  const base = url.pathname.replace(/\/+$/, '')
  url.pathname = base.endsWith('/v1/ws') ? base : `${base}/v1/ws`
  return url.toString()
}

/** 把 `onmessage` 的 `data` 规范成 `Uint8Array`；文本帧返回 `undefined`。 */
export function toBytes(data: unknown): Uint8Array | undefined {
  if (data instanceof Uint8Array) return data
  if (data instanceof ArrayBuffer) return new Uint8Array(data)
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
  if (Array.isArray(data) && data.every((d) => d instanceof Uint8Array)) {
    // ws 的 fragments 模式
    const parts = data as Uint8Array[]
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
    let at = 0
    for (const p of parts) {
      out.set(p, at)
      at += p.length
    }
    return out
  }
  return undefined
}
