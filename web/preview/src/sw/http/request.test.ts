import { describe, expect, it } from 'vitest'
import { MAX_REQUEST_BODY, TOO_LARGE, acceptLanguage, buildRequestHead, readRequestBody } from './request'

const ORIGIN = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app'
const ctx = { selfOrigin: ORIGIN, cookie: '' }
const headersOf = (h: [string, string][]): Record<string, string> => Object.fromEntries(h)

describe('buildRequestHead', () => {
  it('maps method, path and query, with lowercase header names', () => {
    const req = new Request(`${ORIGIN}/src/main.ts?t=1#hash`, { headers: { Accept: '*/*', 'X-Custom': 'v' } })
    const head = buildRequestHead(req, ctx)
    expect(head).toMatchObject({ method: 'GET', path: '/src/main.ts?t=1', websocket: false })
    expect(headersOf(head.headers)).toMatchObject({ accept: '*/*', 'x-custom': 'v' })
    expect(head.headers.every(([n]) => n === n.toLowerCase())).toBe(true)
  })

  it('takes cookie only from the jar, never from the request, and drops hop-by-hop headers', () => {
    const req = new Request(`${ORIGIN}/`, { headers: { cookie: 'stolen=1', connection: 'keep-alive', 'proxy-authorization': 'x', te: 'trailers', 'transfer-encoding': 'chunked', upgrade: 'h2c' } })
    const h = headersOf(buildRequestHead(req, { ...ctx, cookie: 'sid=1; theme=dark' }).headers)
    expect(h.cookie).toBe('sid=1; theme=dark')
    for (const dropped of ['connection', 'proxy-authorization', 'te', 'transfer-encoding', 'upgrade', 'host']) expect(h).not.toHaveProperty(dropped)
    expect(headersOf(buildRequestHead(req, ctx).headers)).not.toHaveProperty('cookie')
  })

  it('adds origin only for requests with a body method', () => {
    const get = headersOf(buildRequestHead(new Request(`${ORIGIN}/`), ctx).headers)
    expect(get).not.toHaveProperty('origin')
    const post = headersOf(buildRequestHead(new Request(`${ORIGIN}/api`, { method: 'POST', body: 'x' }), ctx, 1).headers)
    expect(post.origin).toBe(ORIGIN)
    // 页面自己设了就保持
    const own = headersOf(buildRequestHead(new Request(`${ORIGIN}/api`, { method: 'POST', body: 'x', headers: { origin: 'https://other.example' } }), ctx, 1).headers)
    expect(own.origin).toBe('https://other.example')
  })

  it('sets content-length from the body that was read, including an empty one', () => {
    const post = new Request(`${ORIGIN}/api`, { method: 'POST', body: 'hello' })
    expect(headersOf(buildRequestHead(post, ctx, 5).headers)['content-length']).toBe('5')
    expect(headersOf(buildRequestHead(new Request(`${ORIGIN}/api`, { method: 'POST' }), ctx, 0).headers)['content-length']).toBe('0')
    expect(headersOf(buildRequestHead(new Request(`${ORIGIN}/`), ctx).headers)).not.toHaveProperty('content-length')
  })

  it('adds the headers the browser would send at the network layer', () => {
    const req = new Request(`${ORIGIN}/a`, { referrer: `${ORIGIN}/page?x=1#f` })
    const h = headersOf(buildRequestHead(req, { ...ctx, userAgent: 'UA/1', languages: ['en-US', 'en', 'zh'] }).headers)
    expect(h).toMatchObject({
      'user-agent': 'UA/1',
      'accept-language': 'en-US,en;q=0.9,zh;q=0.8',
      referer: `${ORIGIN}/page?x=1`,
      'sec-fetch-site': 'same-origin',
      'sec-fetch-mode': 'cors',
      'sec-fetch-dest': 'empty',
    })
  })

  it('follows the referrer policy', () => {
    const mk = (referrerPolicy: ReferrerPolicy) => new Request(`${ORIGIN}/a`, { referrer: `${ORIGIN}/page?x=1`, referrerPolicy })
    expect(headersOf(buildRequestHead(mk('no-referrer'), ctx).headers)).not.toHaveProperty('referer')
    expect(headersOf(buildRequestHead(mk('origin'), ctx).headers).referer).toBe(`${ORIGIN}/`)
    expect(headersOf(buildRequestHead(mk('same-origin'), ctx).headers).referer).toBe(`${ORIGIN}/page?x=1`)
    expect(headersOf(buildRequestHead(new Request(`${ORIGIN}/a`), ctx).headers)).not.toHaveProperty('referer')
  })

  it('sends the cache validators it was given', () => {
    const h = headersOf(buildRequestHead(new Request(`${ORIGIN}/a.js`), { ...ctx, validators: { etag: 'W/"1"', lastModified: 'Wed, 21 Oct 2015 07:28:00 GMT' } }).headers)
    expect(h['if-none-match']).toBe('W/"1"')
    expect(h['if-modified-since']).toBe('Wed, 21 Oct 2015 07:28:00 GMT')
  })

  it('formats Accept-Language like a browser', () => {
    expect(acceptLanguage(['zh-CN'])).toBe('zh-CN')
    expect(acceptLanguage(['zh-CN', 'zh', 'en'])).toBe('zh-CN,zh;q=0.9,en;q=0.8')
  })
})

describe('readRequestBody', () => {
  it('has no body for GET / HEAD or when there is none', async () => {
    expect(await readRequestBody(new Request(`${ORIGIN}/`))).toBeUndefined()
    expect(await readRequestBody(new Request(`${ORIGIN}/`, { method: 'HEAD' }))).toBeUndefined()
    expect(await readRequestBody(new Request(`${ORIGIN}/`, { method: 'POST' }))).toBeUndefined()
  })

  it('reads the whole body', async () => {
    const body = await readRequestBody(new Request(`${ORIGIN}/`, { method: 'POST', body: 'héllo' }))
    expect(new TextDecoder().decode(body as Uint8Array)).toBe('héllo')
  })

  it('refuses a body over 16 MiB, whether declared or only discovered while reading', async () => {
    const big = new Uint8Array(MAX_REQUEST_BODY + 1)
    expect(await readRequestBody(new Request(`${ORIGIN}/`, { method: 'POST', body: big }))).toBe(TOO_LARGE)
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new Uint8Array(MAX_REQUEST_BODY))
        c.enqueue(new Uint8Array(1))
        c.close()
      },
    })
    const req = new Request(`${ORIGIN}/`, { method: 'POST', body: stream, duplex: 'half' } as RequestInit)
    expect(req.headers.get('content-length')).toBeNull()
    expect(await readRequestBody(req)).toBe(TOO_LARGE)
    const exact = await readRequestBody(new Request(`${ORIGIN}/`, { method: 'POST', body: new Uint8Array(MAX_REQUEST_BODY) }))
    expect((exact as Uint8Array).length).toBe(MAX_REQUEST_BODY)
  })
})
