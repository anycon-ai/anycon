// 页面的 HTTP 请求 → 隧道 → 响应（spec/web.md § 13.2「拦截」「响应」「注入 shim」「HTTP 缓存」「错误」）。
// 所有依赖都从外面传进来，所以不碰全局 `self`，可以直接在 Node 里测。
import type { Lang } from '../shared/i18n'
import type { CookieStore } from './cookies'
import type { HttpCache } from './http/cache'
import { ExchangeError, openHttp, type HttpExchange } from './http/exchange'
import { TOO_LARGE, buildRequestHead, readRequestBody } from './http/request'
import {
  filterResponseHeaders,
  hasNullBody,
  isAsciiCompatibleCharset,
  isHtml,
  isUsableStatus,
  tapStream,
  transformBody,
  withFrameAncestors,
} from './http/response'
import { errorResponse, type ErrorKind } from './pages'
import { TunnelUnavailable } from './tunnel/link'
import type { TunnelSession } from './tunnel/session'

export interface ProxyDeps {
  /** 等隧道就绪。`navigate`：导航请求，顺便重读启动参数。 */
  whenReady(navigate: boolean): Promise<TunnelSession>
  cookies: CookieStore
  cache?: HttpCache
  /** hub 的公网 origin（`frame-ancestors` 用）；读不到是 `undefined`。 */
  hubOrigin(): Promise<string | undefined>
  lang: Lang
  selfOrigin: string
  userAgent?: string
  languages?: readonly string[]
  /** 隧道已经是终态时的原因；请求中途断开时用它换一个比「还在连接」更准的错误页。 */
  terminalReason?(): TunnelUnavailable | undefined
  /** 非 HttpOnly 的 cookie 变了：通知本 origin 的页面。 */
  cookiesChanged(): void
}

function upstreamKind(err: ExchangeError): ErrorKind {
  const f = err.failure
  if (f.kind === 'reset') {
    if (f.code === 'refused') return 'upstream_refused'
    if (f.code === 'timeout') return 'upstream_timeout'
  }
  if (f.kind === 'timeout') return 'upstream_timeout'
  // 隧道在请求中途断了：会自己重连，导航请求的错误页自动重试
  if (f.kind === 'transport') return 'connecting'
  return 'upstream_error'
}

export async function handleRequest(req: Request, deps: ProxyDeps): Promise<Response> {
  const navigate = req.mode === 'navigate'
  const fail = async (kind: ErrorKind, detail?: string): Promise<Response> =>
    errorResponse({ kind, lang: deps.lang, navigate, hubOrigin: await deps.hubOrigin(), detail })
  try {
    return await proxy(req, deps, navigate, true)
  } catch (err) {
    if (err instanceof TunnelUnavailable) return fail(err.kind, err.detail)
    if (err instanceof ExchangeError) {
      const terminal = err.failure.kind === 'transport' ? deps.terminalReason?.() : undefined
      if (terminal) return fail(terminal.kind, terminal.detail)
      return fail(upstreamKind(err), err.failure.kind === 'reset' ? err.failure.message : undefined)
    }
    return fail('upstream_error', err instanceof Error ? err.message : undefined)
  }
}

async function proxy(req: Request, deps: ProxyDeps, navigate: boolean, allowRetry: boolean, skipCache = false): Promise<Response> {
  const url = new URL(req.url)
  const hubOrigin = await deps.hubOrigin()

  let validators: { etag?: string; lastModified?: string } | undefined
  if (deps.cache && !skipCache) {
    const hit = await deps.cache.lookup(req)
    if (hit.kind === 'fresh') return serveCached(hit.response, hubOrigin)
    if (hit.kind === 'stale') validators = hit.validators
  }

  const body = await readRequestBody(req)
  if (body === TOO_LARGE) return errorResponse({ kind: 'too_large', lang: deps.lang, navigate, hubOrigin })

  const session = await deps.whenReady(navigate)
  const bodyless = req.method === 'GET' || req.method === 'HEAD'
  const head = buildRequestHead(
    req,
    {
      selfOrigin: deps.selfOrigin,
      cookie: await deps.cookies.header(url.pathname),
      validators,
      userAgent: deps.userAgent,
      languages: deps.languages,
    },
    bodyless ? undefined : (body?.length ?? 0),
  )
  let exchange: HttpExchange
  try {
    exchange = await openHttp(session, head, body)
  } catch (err) {
    // 隧道在我们拿到它之后、发请求之前刚好断了：等它重连再试一次
    if (allowRetry && err instanceof Error && err.message === 'tunnel closed') return proxy(req, deps, navigate, false, skipCache)
    throw err
  }
  const respHead = await exchange.head

  // cookie 先存起来、通知页面，再把响应交出去：页面拿到响应之后马上读 document.cookie 就能看到
  const filtered = filterResponseHeaders(respHead, false)
  if (await deps.cookies.storeSetCookies(filtered.setCookies, url.pathname)) deps.cookiesChanged()

  if (respHead.status === 304 && validators && deps.cache) {
    exchange.abort()
    const cached = await deps.cache.revalidated(req.url)
    if (cached) return serveCached(cached, hubOrigin)
    // 缓存里的已经被清掉了：不带条件头重来一次
    return proxy(req, deps, navigate, allowRetry, true)
  }

  if (!isUsableStatus(respHead.status)) {
    exchange.abort()
    return errorResponse({ kind: 'upstream_error', lang: deps.lang, navigate, hubOrigin, detail: `status ${respHead.status}` })
  }
  // 没有正文的状态码（204 / 205 / 304）和 HEAD：`new Response` 不接受带正文
  if (hasNullBody(respHead.status) || req.method === 'HEAD') {
    exchange.abort()
    return new Response(null, { status: respHead.status, headers: withFrameAncestors(filtered.headers, hubOrigin) })
  }

  const contentType = filtered.headers.get('content-type')
  const inject = navigate && isHtml(contentType) && isAsciiCompatibleCharset(contentType) && !filtered.headers.has('content-encoding')
  const outHeaders = inject ? filterResponseHeaders(respHead, true).headers : filtered.headers
  const collector = deps.cache?.collector(req, respHead.status, outHeaders)

  let stream = exchange.body
  stream = transformBody(stream, {
    gzip: respHead.encoding === 'gzip',
    inject: inject ? { cookies: await deps.cookies.documentCookie(url.pathname) } : undefined,
  })
  if (collector) {
    stream = tapStream(stream, {
      chunk: (c) => collector.chunk(c),
      end: () => void collector.end(),
      abort: () => collector.abort(),
    })
  }
  return new Response(stream, { status: respHead.status, headers: withFrameAncestors(outHeaders, hubOrigin) })
}

/** 缓存里取出的响应的头是只读的，而且里面没有 CSP：换一份头再交出去。 */
function serveCached(hit: Response, hubOrigin: string | undefined): Response {
  return new Response(hit.body, { status: hit.status, headers: withFrameAncestors(hit.headers, hubOrigin) })
}
