// 预览页面（shim）和 service worker 之间的消息。页面 → worker 用 `navigator.serviceWorker.controller.postMessage`，
// worker → 页面用 `client.postMessage`。两边都不信任对方：收到的东西先过 `parse*` 再用。
import { isCookieEntry, type CookieEntry } from './cookies'

/** 页面 → service worker。 */
export type PageMessage =
  /** 每 20 秒一条，让 service worker 不因空闲被浏览器回收。 */
  | { type: 'keepalive' }
  /** 页面刚加载完，要一份 cookie jar 里非 HttpOnly 的 cookie。 */
  | { type: 'cookies.sync' }
  /** 页面写了 `document.cookie`：原样的赋值字符串。 */
  | { type: 'cookie.set'; value: string }
  | { type: 'ws.open'; id: number; url: string; protocols: string[] }
  | { type: 'ws.send'; id: number; binary: boolean; data: string | ArrayBuffer }
  | { type: 'ws.close'; id: number; code?: number; reason?: string }

/** service worker → 页面。 */
export type WorkerMessage =
  | { type: 'ws.open'; id: number; protocol: string }
  | { type: 'ws.message'; id: number; binary: boolean; data: string | ArrayBuffer }
  | { type: 'ws.close'; id: number; code: number; reason: string; wasClean: boolean }
  | { type: 'ws.error'; id: number }
  /** 非 HttpOnly cookie 的全量快照。 */
  | { type: 'cookies'; cookies: CookieEntry[] }

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

function isId(v: unknown): v is number {
  return typeof v === 'number' && Number.isSafeInteger(v) && v >= 0
}

export function parsePageMessage(raw: unknown): PageMessage | undefined {
  if (!isRecord(raw)) return undefined
  switch (raw.type) {
    case 'keepalive':
    case 'cookies.sync':
      return { type: raw.type }
    case 'cookie.set':
      return typeof raw.value === 'string' ? { type: 'cookie.set', value: raw.value } : undefined
    case 'ws.open':
      if (!isId(raw.id) || typeof raw.url !== 'string') return undefined
      if (!Array.isArray(raw.protocols) || !raw.protocols.every((p) => typeof p === 'string')) return undefined
      return { type: 'ws.open', id: raw.id, url: raw.url, protocols: raw.protocols as string[] }
    case 'ws.send':
      if (!isId(raw.id) || typeof raw.binary !== 'boolean') return undefined
      if (typeof raw.data !== 'string' && !(raw.data instanceof ArrayBuffer)) return undefined
      return { type: 'ws.send', id: raw.id, binary: raw.binary, data: raw.data }
    case 'ws.close':
      if (!isId(raw.id)) return undefined
      return {
        type: 'ws.close',
        id: raw.id,
        code: typeof raw.code === 'number' ? raw.code : undefined,
        reason: typeof raw.reason === 'string' ? raw.reason : undefined,
      }
    default:
      return undefined
  }
}

export function parseWorkerMessage(raw: unknown): WorkerMessage | undefined {
  if (!isRecord(raw)) return undefined
  switch (raw.type) {
    case 'ws.open':
      return isId(raw.id) && typeof raw.protocol === 'string' ? { type: 'ws.open', id: raw.id, protocol: raw.protocol } : undefined
    case 'ws.message':
      if (!isId(raw.id) || typeof raw.binary !== 'boolean') return undefined
      if (typeof raw.data !== 'string' && !(raw.data instanceof ArrayBuffer)) return undefined
      return { type: 'ws.message', id: raw.id, binary: raw.binary, data: raw.data }
    case 'ws.close':
      if (!isId(raw.id) || typeof raw.code !== 'number' || typeof raw.reason !== 'string') return undefined
      return { type: 'ws.close', id: raw.id, code: raw.code, reason: raw.reason, wasClean: raw.wasClean === true }
    case 'ws.error':
      return isId(raw.id) ? { type: 'ws.error', id: raw.id } : undefined
    case 'cookies':
      return Array.isArray(raw.cookies) ? { type: 'cookies', cookies: raw.cookies.filter(isCookieEntry) } : undefined
    default:
      return undefined
  }
}
