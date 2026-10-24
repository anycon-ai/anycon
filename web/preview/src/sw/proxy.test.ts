import { TunnelFrameType as T } from '@anycon/protocol'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { TestDaemon, flushMicrotasks, type DaemonReply, type Handler } from '../test/daemon'
import { MemoryCacheStorage, MemoryCookies, MemoryIndex } from '../test/memory'
import { CookieStore } from './cookies'
import { HttpCache } from './http/cache'
import { MAX_REQUEST_BODY } from './http/request'
import { handleRequest, type ProxyDeps } from './proxy'
import { TunnelUnavailable } from './tunnel/link'

const ORIGIN = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app'
const HUB = 'https://hub.example'

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] }))
afterEach(() => vi.useRealTimers())

function setup(handler: Handler, over: Partial<ProxyDeps> = {}) {
  const daemon = new TestDaemon(handler)
  const cookiesChanged = vi.fn()
  const cookies = new CookieStore(new MemoryCookies())
  const storage = new MemoryCacheStorage()
  const index = new MemoryIndex()
  const cache = new HttpCache({ storage, index })
  const deps: ProxyDeps = {
    whenReady: () => Promise.resolve(daemon.session),
    cookies,
    cache,
    hubOrigin: () => Promise.resolve(HUB),
    lang: 'en',
    selfOrigin: ORIGIN,
    userAgent: 'TestAgent/1',
    languages: ['en-US'],
    cookiesChanged,
    ...over,
  }
  const get = (path: string, init: RequestInit = {}) => handleRequest(new Request(`${ORIGIN}${path}`, init), deps)
  return { daemon, deps, cookies, cookiesChanged, cache, storage, index, get }
}

const reply = (r: DaemonReply): Handler => () => r
const text = async (res: Response): Promise<string> => res.text()

describe('a plain request', () => {
  it('goes through the tunnel and comes back with the daemon’s status, headers and body', async () => {
    const t = setup(reply({ status: 201, headers: [['content-type', 'text/javascript'], ['x-from', 'dev']], body: 'export const a = 1' }))
    const res = await t.get('/src/a.js?x=1', { headers: { accept: '*/*' } })
    expect(res.status).toBe(201)
    expect(res.headers.get('content-type')).toBe('text/javascript')
    expect(res.headers.get('x-from')).toBe('dev')
    expect(await text(res)).toBe('export const a = 1')
    const sent = t.daemon.requests[0]!.head
    expect(sent).toMatchObject({ method: 'GET', path: '/src/a.js?x=1' })
    expect(Object.fromEntries(sent.headers)).toMatchObject({ accept: '*/*', 'user-agent': 'TestAgent/1', 'accept-language': 'en-US', 'sec-fetch-site': 'same-origin' })
  })

  it('lets only the hub and the preview itself frame the response, on top of the dev server’s own CSP', async () => {
    const t = setup(reply({ headers: [['content-security-policy', "script-src 'self'"], ['x-frame-options', 'DENY']], body: 'x' }))
    const res = await t.get('/')
    expect(res.headers.get('content-security-policy')).toBe("script-src 'self', frame-ancestors 'self' https://hub.example")
    expect(res.headers.has('x-frame-options')).toBe(false)
  })

  it('falls back to frame-ancestors self when the hub origin is unknown', async () => {
    const t = setup(reply({ body: 'x' }), { hubOrigin: () => Promise.resolve(undefined) })
    expect((await t.get('/')).headers.get('content-security-policy')).toBe("frame-ancestors 'self'")
  })

  it('decompresses gzip bodies', async () => {
    const gz = new Uint8Array(await new Response(new Blob(['compressed hello']).stream().pipeThrough(new CompressionStream('gzip'))).arrayBuffer())
    const t = setup(reply({ gzip: true, body: gz, headers: [['content-type', 'text/plain']] }))
    const res = await t.get('/x.txt')
    expect(res.headers.has('content-encoding')).toBe(false)
    expect(await text(res)).toBe('compressed hello')
  })

  it('sends POST bodies with content-length and the preview origin', async () => {
    const t = setup(reply({ status: 200, body: 'ok' }))
    await t.get('/api', { method: 'POST', body: JSON.stringify({ a: 1 }), headers: { 'content-type': 'application/json' } })
    const req = t.daemon.requests[0]!
    expect(new TextDecoder().decode(req.body)).toBe('{"a":1}')
    const h = Object.fromEntries(req.head.headers)
    expect(h['content-length']).toBe('7')
    expect(h.origin).toBe(ORIGIN)
    expect(h['content-type']).toBe('application/json')
  })

  it('answers 413 for a body over 16 MiB without touching the tunnel', async () => {
    const t = setup(reply({ body: 'x' }))
    const res = await t.get('/upload', { method: 'PUT', body: new Uint8Array(MAX_REQUEST_BODY + 1) })
    expect(res.status).toBe(413)
    expect(t.daemon.requests).toHaveLength(0)
  })

  it('passes statuses without a body as null bodies', async () => {
    for (const status of [204, 205, 304]) {
      const t = setup(reply({ status }))
      const res = await t.get('/x')
      expect(res.status).toBe(status)
      expect(res.body).toBeNull()
    }
  })

  it('does not return a body for HEAD', async () => {
    const t = setup(reply({ body: 'ignored', headers: [['content-length', '7']] }))
    const res = await t.get('/x', { method: 'HEAD' })
    expect(res.status).toBe(200)
    expect(res.body).toBeNull()
    expect(res.headers.get('content-length')).toBe('7')
  })

  it('turns an impossible status into an error instead of throwing', async () => {
    const t = setup(reply({ status: 101 }))
    const res = await t.get('/x')
    expect(res.status).toBe(502)
  })
})

describe('cookies', () => {
  it('keeps Set-Cookie in the jar, sends them on later requests and tells the pages first', async () => {
    let calls = 0
    const t = setup(() => (calls++ === 0 ? { headers: [['set-cookie', 'sid=1; Path=/; HttpOnly'], ['set-cookie', 'theme=dark; Path=/']], body: 'login' } : { body: 'next' }))
    const first = await t.get('/login')
    expect(first.headers.has('set-cookie')).toBe(false)
    expect(t.cookiesChanged).toHaveBeenCalledTimes(1)
    await t.get('/home')
    expect(Object.fromEntries(t.daemon.requests[1]!.head.headers).cookie).toBe('sid=1; theme=dark')
  })

  it('does not notify the pages for HttpOnly-only changes', async () => {
    const t = setup(reply({ headers: [['set-cookie', 'sid=1; HttpOnly']], body: 'x' }))
    await t.get('/')
    expect(t.cookiesChanged).not.toHaveBeenCalled()
  })
})

describe('navigations', () => {
  const html = '<!doctype html><html><head><title>t</title></head><body>hi</body></html>'
  const nav = (path = '/') => new Request(`${ORIGIN}${path}`, { headers: { accept: 'text/html' } })
  // Node 的 Request 没法设 mode: 'navigate'，用一个带 mode 的壳
  const navigate = (req: Request): Request => new Proxy(req, { get: (t, p) => (p === 'mode' ? 'navigate' : Reflect.get(t, p) instanceof Function ? (Reflect.get(t, p) as (...a: unknown[]) => unknown).bind(t) : Reflect.get(t, p)) })

  it('injects the shim after <head> with the document’s non-HttpOnly cookies, and drops content-length', async () => {
    const t = setup(reply({ headers: [['content-type', 'text/html; charset=utf-8'], ['content-length', String(html.length)]], body: html }))
    await t.cookies.storeSetCookies(['a=1; Path=/', 'secret=s; HttpOnly', 'q="x"&y; Path=/'], '/')
    const res = await handleRequest(navigate(nav('/')), t.deps)
    const body = await res.text()
    expect(body).toBe(html.replace('<head>', '<head><script src="/__anycon/shim.js" data-cookies="a=1; q=&quot;x&quot;&amp;y"></script>'))
    expect(res.headers.has('content-length')).toBe(false)
    expect(res.headers.get('content-type')).toBe('text/html; charset=utf-8')
  })

  it('injects into gzip HTML too', async () => {
    const gz = new Uint8Array(await new Response(new Blob([html]).stream().pipeThrough(new CompressionStream('gzip'))).arrayBuffer())
    const t = setup(reply({ gzip: true, body: gz, headers: [['content-type', 'text/html']] }))
    const body = await (await handleRequest(navigate(nav()), t.deps)).text()
    expect(body).toContain('<head><script src="/__anycon/shim.js"')
    expect(body).toContain('<title>t</title>')
  })

  it('leaves non-navigation HTML, non-HTML and UTF-16 pages alone', async () => {
    const t1 = setup(reply({ headers: [['content-type', 'text/html']], body: html }))
    expect(await text(await t1.get('/fragment.html'))).toBe(html)
    const t2 = setup(reply({ headers: [['content-type', 'application/json']], body: '{"head":1}' }))
    expect(await text(await handleRequest(navigate(nav('/data')), t2.deps))).toBe('{"head":1}')
    const t3 = setup(reply({ headers: [['content-type', 'text/html; charset=utf-16le']], body: html }))
    expect(await text(await handleRequest(navigate(nav()), t3.deps))).toBe(html)
  })

  it('shows an error page for a port nobody listens on', async () => {
    const t = setup(reply({ reset: 'refused' }))
    const res = await handleRequest(navigate(nav()), t.deps)
    expect(res.status).toBe(502)
    expect(res.headers.get('content-type')).toContain('text/html')
    expect(await res.text()).toContain('Nothing is listening on that port')
  })
})

describe('errors', () => {
  it.each([
    ['refused', 'Nothing is listening'],
    ['timeout', 'did not respond'],
    ['internal', 'request failed'],
  ] as const)('RESET %s becomes a 502 text for sub-resources', async (code, words) => {
    const t = setup(reply({ reset: code }))
    const res = await t.get('/x.js')
    expect(res.status).toBe(502)
    expect(await res.text()).toContain(words)
  })

  it('reports an unavailable tunnel; transient ones ask the page to retry', async () => {
    const t = setup(reply({}), { whenReady: () => Promise.reject(new TunnelUnavailable('node_offline')) })
    const sub = await t.get('/x.js')
    expect(sub.status).toBe(502)
    expect(await sub.text()).toContain('The machine is offline')
    const navReq = new Proxy(new Request(`${ORIGIN}/`), { get: (target, p) => (p === 'mode' ? 'navigate' : Reflect.get(target, p)) }) as Request
    const page = await handleRequest(navReq, t.deps)
    expect(page.status).toBe(503)
    expect(await page.text()).toContain('http-equiv="refresh"')
  })

  it('terminal reasons say to reopen from Anycon', async () => {
    const t = setup(reply({}), { whenReady: () => Promise.reject(new TunnelUnavailable('closed')) })
    const navReq = new Proxy(new Request(`${ORIGIN}/`), { get: (target, p) => (p === 'mode' ? 'navigate' : Reflect.get(target, p)) }) as Request
    expect(await (await handleRequest(navReq, t.deps)).text()).toContain('open the preview again')
  })

  it('when the tunnel dies for good in the middle of a request, the error page says why', async () => {
    const t = setup(reply({ hang: true }), { terminalReason: () => new TunnelUnavailable('closed') })
    const pending = t.get('/x')
    await flushMicrotasks()
    t.daemon.session.destroy({ kind: 'transport', message: 'goaway' })
    const res = await pending
    expect(await res.text()).toContain('This preview was stopped')
    // 没有终态：当作暂时断开
    const t2 = setup(reply({ hang: true }))
    const pending2 = t2.get('/x')
    await flushMicrotasks()
    t2.daemon.session.destroy({ kind: 'transport', message: 'hub closed' })
    expect(await (await pending2).text()).toContain('Still connecting')
  })

  it('gives up on a daemon that never answers', async () => {
    const t = setup(reply({ hang: true }))
    const pending = t.get('/slow')
    await vi.advanceTimersByTimeAsync(125_000)
    const res = await pending
    expect(res.status).toBe(502)
    expect(await res.text()).toContain('did not respond')
    expect(t.daemon.resets).toHaveLength(1)
  })

  it('retries once when the tunnel closes between whenReady and the request', async () => {
    const t = setup(reply({ body: 'second try' }))
    let calls = 0
    const dead = new TestDaemon(reply({}))
    dead.session.destroy({ kind: 'transport', message: 'gone' })
    t.deps.whenReady = () => Promise.resolve(calls++ === 0 ? dead.session : t.daemon.session)
    expect(await text(await t.get('/x'))).toBe('second try')
    expect(calls).toBe(2)
  })

  it('an error in the middle of the body breaks the stream the page is reading', async () => {
    const t = setup(reply({ chunks: ['part'], keepOpen: true }))
    const res = await t.get('/stream')
    const reader = res.body!.getReader()
    expect(new TextDecoder().decode((await reader.read()).value)).toBe('part')
    t.daemon.send([{ type: T.Reset, stream: 1, payload: Uint8Array.of(0x81, 0xa4, 0x63, 0x6f, 0x64, 0x65, 0xa7, 0x72, 0x65, 0x66, 0x75, 0x73, 0x65, 0x64) }])
    await expect(reader.read()).rejects.toThrow()
  })
})

describe('streaming and flow control', () => {
  it('lets the daemon send no more than the window until the page reads', async () => {
    const big = new Uint8Array(1024 * 1024).map((_, i) => i & 0xff)
    const t = setup(reply({ body: big }))
    const res = await t.get('/big.bin')
    await flushMicrotasks()
    // 页面还一个字节都没读：daemon 最多发了一个窗口（256 KiB）
    const queued = (t.daemon as unknown as { outq: Map<number, unknown[]> }).outq.get(1)
    expect(queued?.length).toBeGreaterThan(0)
    const body = new Uint8Array(await res.arrayBuffer())
    expect(body.length).toBe(big.length)
    expect(body.every((b, i) => b === (i & 0xff))).toBe(true)
  })

  it('cancelling the response body resets the stream', async () => {
    const t = setup(reply({ chunks: ['a'], keepOpen: true }))
    const res = await t.get('/events')
    const reader = res.body!.getReader()
    await reader.read()
    await reader.cancel()
    await flushMicrotasks()
    expect(t.daemon.resets).toHaveLength(1)
  })
})

describe('HTTP cache', () => {
  it('asks with the validator the second time and serves the stored body on 304', async () => {
    let n = 0
    const t = setup((req) => {
      n++
      const inm = Object.fromEntries(req.head.headers)['if-none-match']
      return inm === '"v1"' ? { status: 304 } : { headers: [['etag', '"v1"'], ['cache-control', 'no-cache'], ['content-type', 'text/javascript']], body: 'source' }
    })
    expect(await text(await t.get('/a.js'))).toBe('source')
    await flushMicrotasks()
    const second = await t.get('/a.js')
    expect(second.status).toBe(200)
    expect(await second.text()).toBe('source')
    expect(second.headers.get('content-type')).toBe('text/javascript')
    expect(second.headers.get('content-security-policy')).toContain('frame-ancestors')
    expect(n).toBe(2)
    expect(Object.fromEntries(t.daemon.requests[1]!.head.headers)['if-none-match']).toBe('"v1"')
  })

  it('serves immutable responses without a request', async () => {
    const t = setup(reply({ headers: [['etag', '"d"'], ['cache-control', 'max-age=31536000, immutable']], body: 'dep' }))
    await text(await t.get('/deps/react.js?v=1'))
    await flushMicrotasks()
    const again = await t.get('/deps/react.js?v=1')
    expect(await again.text()).toBe('dep')
    expect(t.daemon.requests).toHaveLength(1)
    // 另一个 URL 不受影响
    await text(await t.get('/deps/react.js?v=2'))
    expect(t.daemon.requests).toHaveLength(2)
  })

  it('works offline for fresh entries', async () => {
    const t = setup(reply({ headers: [['etag', '"d"'], ['cache-control', 'immutable']], body: 'dep' }))
    await text(await t.get('/dep.js'))
    await flushMicrotasks()
    t.deps.whenReady = () => Promise.reject(new TunnelUnavailable('node_offline'))
    expect(await text(await t.get('/dep.js'))).toBe('dep')
  })

  it('skips the cache for cache: reload and does not store cache: no-store responses', async () => {
    const t = setup(reply({ headers: [['etag', '"d"'], ['cache-control', 'immutable']], body: 'dep' }))
    await text(await t.get('/dep.js'))
    await flushMicrotasks()
    await text(await t.get('/dep.js', { cache: 'reload' }))
    expect(t.daemon.requests).toHaveLength(2)
    const t2 = setup(reply({ headers: [['etag', '"d"'], ['cache-control', 'immutable']], body: 'dep' }))
    await text(await t2.get('/dep.js', { cache: 'no-store' }))
    await flushMicrotasks()
    await text(await t2.get('/dep.js'))
    expect(t2.daemon.requests).toHaveLength(2)
  })

  it('stores the decompressed body', async () => {
    const gz = new Uint8Array(await new Response(new Blob(['plain text']).stream().pipeThrough(new CompressionStream('gzip'))).arrayBuffer())
    const t = setup(reply({ gzip: true, body: gz, headers: [['etag', '"g"'], ['cache-control', 'immutable']] }))
    await text(await t.get('/g.txt'))
    await flushMicrotasks()
    const stored = [...t.storage.caches.values()][0]!.items.get(`${ORIGIN}/g.txt`)!
    expect(new TextDecoder().decode(stored.body)).toBe('plain text')
    expect(await text(await t.get('/g.txt'))).toBe('plain text')
  })

  it('fetches again, unconditionally, when a 304 arrives but the stored copy is gone', async () => {
    let n = 0
    const t = setup((req) => {
      n++
      const conditional = 'if-none-match' in Object.fromEntries(req.head.headers)
      if (n === 2 && conditional) return { status: 304 }
      return { headers: [['etag', '"v"'], ['cache-control', 'no-cache']], body: `body ${n}` }
    })
    await text(await t.get('/a.js'))
    await flushMicrotasks()
    // 查询缓存之后、收到 304 之前，条目被清掉了
    const realLookup = t.cache.lookup.bind(t.cache)
    t.cache.lookup = async (req) => {
      const hit = await realLookup(req)
      ;[...t.storage.caches.values()][0]!.items.clear()
      t.index.entries.clear()
      return hit
    }
    t.cache.revalidated = async () => undefined
    expect(await text(await t.get('/a.js'))).toBe('body 3')
  })
})
