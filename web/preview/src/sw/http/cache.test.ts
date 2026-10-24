import { beforeEach, describe, expect, it } from 'vitest'
import { MemoryCacheStorage, MemoryIndex } from '../../test/memory'
import { CACHE_NAME, HttpCache, parseCacheControl } from './cache'

const URL_A = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app/src/a.js'
const URL_B = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app/src/b.js'
const req = (url = URL_A, init: RequestInit = {}) => new Request(url, init)

let now = 1_000_000
let storage: MemoryCacheStorage
let index: MemoryIndex
const make = (over: { maxBytes?: number; entryMaxBytes?: number } = {}) => new HttpCache({ storage, index, now: () => now, ...over })

/** 走一遍「收集 → 存下」。 */
async function put(cache: HttpCache, url: string, body: string, headers: Record<string, string>, request: Request = req(url)): Promise<void> {
  const collector = cache.collector(request, 200, new Headers(headers))
  expect(collector, 'collector').toBeDefined()
  collector!.chunk(new TextEncoder().encode(body))
  await collector!.end()
}

beforeEach(() => {
  now = 1_000_000
  storage = new MemoryCacheStorage()
  index = new MemoryIndex()
})

describe('what gets stored', () => {
  const ok = new Headers({ etag: '"1"' })

  it('only GET 200 with a validator, not for navigations, ranges or no-store', () => {
    const cache = make()
    expect(cache.collector(req(), 200, ok)).toBeDefined()
    expect(cache.collector(req(URL_A, { headers: {} }), 200, new Headers({ 'last-modified': 'Wed, 21 Oct 2015 07:28:00 GMT' }))).toBeDefined()
    expect(cache.collector(req(), 200, new Headers())).toBeUndefined() // 没有 etag / last-modified
    expect(cache.collector(req(), 404, ok)).toBeUndefined()
    expect(cache.collector(req(), 206, ok)).toBeUndefined()
    expect(cache.collector(req(URL_A, { method: 'POST', body: 'x' }), 200, ok)).toBeUndefined()
    expect(cache.collector(req(URL_A, { headers: { range: 'bytes=0-1' } }), 200, ok)).toBeUndefined()
    expect(cache.collector(req(URL_A, { cache: 'no-store' }), 200, ok)).toBeUndefined()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', 'cache-control': 'private, no-store' }))).toBeUndefined()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', 'content-range': 'bytes 0-1/2' }))).toBeUndefined()
  })

  it('respects Vary: only accept-encoding and origin are harmless', () => {
    const cache = make()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', vary: 'Origin, Accept-Encoding' }))).toBeDefined()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', vary: 'Accept' }))).toBeUndefined()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', vary: 'Cookie' }))).toBeUndefined()
    expect(cache.collector(req(), 200, new Headers({ etag: '"1"', vary: '*' }))).toBeUndefined()
  })

  it('stores the body and headers in the preview cache and records the size', async () => {
    const cache = make()
    await put(cache, URL_A, 'console.log(1)', { etag: '"1"', 'content-type': 'text/javascript' })
    const stored = storage.caches.get(CACHE_NAME)!.items.get(URL_A)!
    expect(new TextDecoder().decode(stored.body)).toBe('console.log(1)')
    expect(Object.fromEntries(stored.headers)).toMatchObject({ etag: '"1"', 'content-type': 'text/javascript' })
    expect(index.entries.get(URL_A)).toMatchObject({ size: 14, storedAt: now, lastUsed: now })
    expect(await cache.totalBytes()).toBe(14)
  })

  it('does not store an aborted response, or one over the entry limit', async () => {
    const cache = make({ entryMaxBytes: 10 })
    const aborted = cache.collector(req(), 200, ok)!
    aborted.chunk(new Uint8Array(3))
    aborted.abort()
    await aborted.end()
    const big = cache.collector(req(URL_B), 200, ok)!
    big.chunk(new Uint8Array(6))
    big.chunk(new Uint8Array(6))
    await big.end()
    expect(storage.caches.get(CACHE_NAME)?.items.size ?? 0).toBe(0)
    expect(index.entries.size).toBe(0)
  })
})

describe('lookup', () => {
  it('misses when nothing is stored', async () => {
    expect((await make().lookup(req())).kind).toBe('miss')
  })

  it('bypasses for navigations, non-GET, ranges, cache: reload and cache: no-store', async () => {
    const cache = make()
    await put(cache, URL_A, 'x', { etag: '"1"', 'cache-control': 'immutable' })
    for (const r of [
      new Request(URL_A, { headers: { range: 'bytes=0-0' } }),
      new Request(URL_A, { method: 'POST', body: 'x' }),
      new Request(URL_A, { cache: 'reload' }),
      new Request(URL_A, { cache: 'no-store' }),
    ]) {
      expect((await cache.lookup(r)).kind).toBe('bypass')
    }
    expect((await cache.lookup(req())).kind).toBe('fresh')
  })

  it('serves immutable responses without asking, whatever their age', async () => {
    const cache = make()
    await put(cache, URL_A, 'dep', { etag: '"1"', 'cache-control': 'max-age=31536000, immutable' })
    now += 400 * 24 * 3600 * 1000
    const hit = await cache.lookup(req())
    expect(hit.kind).toBe('fresh')
    if (hit.kind === 'fresh') expect(await hit.response.text()).toBe('dep')
  })

  it('serves max-age responses until they expire, then asks with validators', async () => {
    const cache = make()
    await put(cache, URL_A, 'v1', { etag: '"abc"', 'last-modified': 'Wed, 21 Oct 2015 07:28:00 GMT', 'cache-control': 'max-age=60' })
    now += 59_999
    expect((await cache.lookup(req())).kind).toBe('fresh')
    now += 1
    const stale = await cache.lookup(req())
    expect(stale).toEqual({ kind: 'stale', validators: { etag: '"abc"', lastModified: 'Wed, 21 Oct 2015 07:28:00 GMT' } })
  })

  it('always asks when the response says no-cache or has no freshness, and when the request says cache: no-cache', async () => {
    const cache = make()
    await put(cache, URL_A, 'a', { etag: '"1"', 'cache-control': 'no-cache' })
    await put(cache, URL_B, 'b', { etag: '"2"', 'cache-control': 'max-age=3600' })
    expect((await cache.lookup(req(URL_A))).kind).toBe('stale')
    expect((await cache.lookup(req(URL_B))).kind).toBe('fresh')
    expect((await cache.lookup(req(URL_B, { cache: 'no-cache' }))).kind).toBe('stale')
    await put(cache, 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app/c', 'c', { 'last-modified': 'Wed, 21 Oct 2015 07:28:00 GMT' })
    expect(await cache.lookup(req('https://abcdefghijklmnopqrstuvwxyz.aiplace.app/c'))).toEqual({ kind: 'stale', validators: { etag: undefined, lastModified: 'Wed, 21 Oct 2015 07:28:00 GMT' } })
  })

  it('drops an entry that exists in only one of the two stores', async () => {
    const cache = make()
    await put(cache, URL_A, 'x', { etag: '"1"' })
    index.entries.delete(URL_A)
    const fresh = make()
    expect((await fresh.lookup(req())).kind).toBe('miss')
    expect(storage.caches.get(CACHE_NAME)!.items.has(URL_A)).toBe(false)
  })

  it('reads the index back after a restart', async () => {
    await put(make(), URL_A, 'x', { etag: '"1"', 'cache-control': 'immutable' })
    expect((await make().lookup(req())).kind).toBe('fresh')
  })
})

describe('revalidation (304)', () => {
  it('returns the stored response and starts a new freshness period', async () => {
    const cache = make()
    await put(cache, URL_A, 'body', { etag: '"1"', 'cache-control': 'max-age=60' })
    now += 120_000
    expect((await cache.lookup(req())).kind).toBe('stale')
    const hit = await cache.revalidated(URL_A)
    expect(await hit!.text()).toBe('body')
    expect(index.entries.get(URL_A)!.storedAt).toBe(now)
    expect((await cache.lookup(req())).kind).toBe('fresh')
  })

  it('returns nothing when the entry is gone', async () => {
    expect(await make().revalidated(URL_A)).toBeUndefined()
  })
})

describe('size limit', () => {
  it('evicts the least recently used entries first when the total passes the limit', async () => {
    const cache = make({ maxBytes: 100 })
    await put(cache, URL_A, 'a'.repeat(40), { etag: '"a"', 'cache-control': 'immutable' })
    now += 10
    await put(cache, URL_B, 'b'.repeat(40), { etag: '"b"', 'cache-control': 'immutable' })
    now += 10
    // 用一下 A，让 B 成为最久没用的
    expect((await cache.lookup(req(URL_A))).kind).toBe('fresh')
    now += 10
    const c = 'https://abcdefghijklmnopqrstuvwxyz.aiplace.app/src/c.js'
    await put(cache, c, 'c'.repeat(40), { etag: '"c"', 'cache-control': 'immutable' })
    const items = storage.caches.get(CACHE_NAME)!.items
    expect([...items.keys()].sort()).toEqual([URL_A, c].sort())
    expect([...index.entries.keys()].sort()).toEqual([URL_A, c].sort())
    expect(await cache.totalBytes()).toBe(80)
  })

  it('keeps the total at or below 200 MiB by default', async () => {
    const cache = make()
    expect((cache as unknown as { maxBytes: number }).maxBytes).toBe(200 * 1024 * 1024)
  })
})

describe('parseCacheControl', () => {
  it('reads the directives it needs', () => {
    expect(parseCacheControl('public, max-age=600, immutable')).toEqual({ immutable: true, noStore: false, noCache: false, maxAge: 600 })
    expect(parseCacheControl('No-Store')).toMatchObject({ noStore: true })
    expect(parseCacheControl('no-cache, max-age=0')).toMatchObject({ noCache: true, maxAge: 0 })
    expect(parseCacheControl('max-age="30"')).toMatchObject({ maxAge: 30 })
    expect(parseCacheControl('max-age=abc')).toEqual({ immutable: false, noStore: false, noCache: false })
    expect(parseCacheControl(null)).toEqual({ immutable: false, noStore: false, noCache: false })
  })
})
