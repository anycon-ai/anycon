// 测试用的假 hub REST：按 `"METHOD /path"`（或 `"METHOD /prefix/*"`）登记处理函数，记下所有调用。没登记的路径回 404。
import { vi } from 'vitest'

export interface ApiCall {
  method: string
  path: string
  body: unknown
  headers: Record<string, string>
}

export interface ApiReply {
  status?: number
  body?: unknown
  headers?: Record<string, string>
}

export type ApiHandler = (call: ApiCall) => ApiReply | Record<string, unknown> | unknown[] | undefined | Promise<ApiReply | Record<string, unknown> | unknown[] | undefined>

function isReply(v: unknown): v is ApiReply {
  return typeof v === 'object' && v !== null && !Array.isArray(v) && ('status' in v || 'body' in v) && Object.keys(v).every((k) => k === 'status' || k === 'body' || k === 'headers')
}

export function errorBody(code: string, message = code): ApiReply['body'] {
  return { error: { code, message } }
}

export function installFakeApi(handlers: Record<string, ApiHandler>) {
  const calls: ApiCall[] = []
  const table = new Map(Object.entries(handlers))
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, 'http://localhost')
    const method = (init?.method ?? 'GET').toUpperCase()
    const headers = Object.fromEntries(Object.entries((init?.headers ?? {}) as Record<string, string>))
    const call: ApiCall = { method, path: url.pathname + url.search, body: init?.body ? JSON.parse(String(init.body)) : undefined, headers }
    calls.push(call)
    // 精确匹配优先，其次是 "METHOD /prefix/*" 这样的通配
    const handler = table.get(`${method} ${url.pathname}`) ?? [...table].find(([k]) => k.endsWith('/*') && `${method} ${url.pathname}`.startsWith(k.slice(0, -1)))?.[1]
    if (!handler) return new Response(JSON.stringify(errorBody('not_found', `${method} ${url.pathname}`)), { status: 404, headers: { 'Content-Type': 'application/json' } })
    const out = await handler(call)
    if (out === undefined) return new Response(null, { status: 204 })
    const reply: ApiReply = isReply(out) ? out : { status: 200, body: out }
    const status = reply.status ?? 200
    return new Response(reply.body === undefined ? null : JSON.stringify(reply.body), { status, headers: { 'Content-Type': 'application/json', ...reply.headers } })
  })
  vi.stubGlobal('fetch', fetchMock)
  return {
    calls,
    /** 某个 "METHOD /path" 被调了几次 / 请求体。 */
    callsTo: (key: string): ApiCall[] => calls.filter((c) => `${c.method} ${c.path.split('?')[0]}` === key),
    set: (key: string, handler: ApiHandler): void => void table.set(key, handler),
  }
}
