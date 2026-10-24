// 页面发出的请求 → 隧道的 `REQUEST` 头和请求体（spec/web.md § 13.2「拦截」）。
import type { PreviewRequestHead } from '@anycon/protocol'

/** 请求体先整个读出来再分段发；超过这个大小回 `413`。 */
export const MAX_REQUEST_BODY = 16 * 1024 * 1024

/** 不转发的请求头：`cookie` 由 cookie jar 重算；`content-length` 按读出来的请求体重算。 */
const DROP = new Set(['cookie', 'content-length', 'host', 'connection', 'keep-alive', 'proxy-connection', 'te', 'trailer', 'transfer-encoding', 'upgrade'])

export interface RequestContext {
  /** 预览 origin（`https://<label>.<预览域名>`）：非 GET 请求带的 `origin` 头，daemon 会把它换成 `http://localhost:<端口>`。 */
  selfOrigin: string
  /** cookie jar 里路径匹配的 cookie（`a=1; b=2`），空串表示不带。 */
  cookie: string
  /** 条件请求：缓存里有这个 URL 的旧响应。 */
  validators?: { etag?: string; lastModified?: string }
  userAgent?: string
  languages?: readonly string[]
}

const BODYLESS = new Set(['GET', 'HEAD'])

/** `Accept-Language` 值：`en-US,en;q=0.9` 的形式。 */
export function acceptLanguage(languages: readonly string[]): string {
  return languages
    .slice(0, 5)
    .map((l, i) => (i === 0 ? l : `${l};q=${Math.max(0.1, 1 - i * 0.1).toFixed(1)}`))
    .join(',')
}

/**
 * 请求头：`event.request.headers`（名字小写）加上浏览器在网络层才会补的几个——`origin`（非 GET）、`referer`、
 * `sec-fetch-*`、`user-agent`、`accept-language`——因为 service worker 的 `request.headers` 里没有它们，而 dev server
 * （CSRF 校验、按语言渲染、按 `sec-fetch-dest` 决定是否回退到 index.html）会看。再加 cookie jar 的 `cookie` 和条件请求头。
 */
export function buildRequestHead(req: Request, ctx: RequestContext, bodyLength?: number): PreviewRequestHead {
  const url = new URL(req.url)
  const headers: [string, string][] = []
  const have = new Set<string>()
  req.headers.forEach((value, name) => {
    const lower = name.toLowerCase()
    if (DROP.has(lower) || lower.startsWith('proxy-')) return
    headers.push([lower, value])
    have.add(lower)
  })
  const add = (name: string, value: string | undefined): void => {
    if (value === undefined || value === '' || have.has(name)) return
    headers.push([name, value])
    have.add(name)
  }
  add('user-agent', ctx.userAgent)
  if (ctx.languages && ctx.languages.length > 0) add('accept-language', acceptLanguage(ctx.languages))
  if (!BODYLESS.has(req.method)) add('origin', ctx.selfOrigin)
  add('referer', referrerOf(req, ctx.selfOrigin))
  add('sec-fetch-dest', req.destination === '' ? 'empty' : req.destination)
  add('sec-fetch-mode', req.mode)
  add('sec-fetch-site', 'same-origin')
  if (ctx.validators?.etag) add('if-none-match', ctx.validators.etag)
  if (ctx.validators?.lastModified) add('if-modified-since', ctx.validators.lastModified)
  add('cookie', ctx.cookie)
  if (bodyLength !== undefined) add('content-length', String(bodyLength))
  return { method: req.method, path: url.pathname + url.search, headers, websocket: false }
}

/** 按 `referrerPolicy` 算出浏览器会发的 `Referer`；没有就不发。 */
function referrerOf(req: Request, selfOrigin: string): string | undefined {
  const ref = req.referrer
  if (!ref || ref === 'about:client' || !/^https?:/i.test(ref)) return undefined
  switch (req.referrerPolicy) {
    case 'no-referrer':
      return undefined
    case 'origin':
    case 'strict-origin':
      return `${selfOrigin}/`
    default:
      return ref.split('#')[0]
  }
}

export const TOO_LARGE = Symbol('request body too large')

/** 读出整个请求体。GET / HEAD 或没有请求体返回 `undefined`；超过上限返回 `TOO_LARGE`（并停止读取）。 */
export async function readRequestBody(req: Request): Promise<Uint8Array | undefined | typeof TOO_LARGE> {
  if (BODYLESS.has(req.method) || !req.body) return undefined
  const declared = Number(req.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_REQUEST_BODY) {
    void req.body.cancel().catch(() => {})
    return TOO_LARGE
  }
  const reader = req.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    total += value.length
    if (total > MAX_REQUEST_BODY) {
      void reader.cancel().catch(() => {})
      return TOO_LARGE
    }
    chunks.push(value)
  }
  const out = new Uint8Array(total)
  let at = 0
  for (const c of chunks) {
    out.set(c, at)
    at += c.length
  }
  return out
}
