// service worker 自己做的 HTTP 缓存（spec/web.md § 13.2「HTTP 缓存」）。
//
// 浏览器不会把 service worker 合成的响应放进 HTTP 缓存，所以每次刷新 Vite 未打包模式的几百个模块都要重新走一遍隧道。
// 这里把 `GET` 200、带 `etag` 或 `last-modified`、没有 `no-store` 的响应存进 Cache Storage（`anycon-preview-http`，
// 总量 200 MiB，超了按最近使用删），索引（大小、存入时间、最近使用）放在 IndexedDB。
//
// - `cache-control` 带 `immutable`（或 `max-age` 没过期）：直接用缓存，不走隧道；
// - 否则带 `if-none-match` / `if-modified-since` 去问，收到 `304` 就用缓存里的；
// - 请求的 `cache` 是 `no-store` / `reload` 时绕过缓存，`no-cache` 时一定要问；带 `range` 的请求不缓存。
//
// 存进去的是解压后的正文（先过 gzip 解压再存），不带 `set-cookie` / `x-frame-options` / CSP。
import { idbAll, idbDelete, idbPut, STORES } from '../../shared/idb'

export const CACHE_NAME = 'anycon-preview-http'
export const CACHE_MAX_BYTES = 200 * 1024 * 1024
/** 单个响应超过这么大就不存（也限制了收集期间的内存）。 */
export const ENTRY_MAX_BYTES = 16 * 1024 * 1024
/** 最近使用时间最多隔这么久写一次索引。 */
const TOUCH_PERSIST_MS = 60_000

export interface CacheLike {
  match(url: string): Promise<Response | undefined>
  put(url: string, response: Response): Promise<void>
  delete(url: string): Promise<boolean>
}
export interface CacheStorageLike {
  open(name: string): Promise<CacheLike>
}

export interface CacheIndexEntry {
  url: string
  size: number
  storedAt: number
  lastUsed: number
}

export interface IndexStore {
  load(): Promise<CacheIndexEntry[]>
  put(entry: CacheIndexEntry): Promise<void>
  delete(url: string): Promise<void>
}

/** 索引放在预览 origin 的 IndexedDB。 */
export const idbIndexStore: IndexStore = {
  load: async () => (await idbAll<CacheIndexEntry>(STORES.httpCache)).map((e) => e.value),
  put: (entry) => idbPut(STORES.httpCache, entry.url, entry),
  delete: (url) => idbDelete(STORES.httpCache, url),
}

export interface CacheControl {
  immutable: boolean
  noStore: boolean
  noCache: boolean
  maxAge?: number
}

export function parseCacheControl(value: string | null): CacheControl {
  const cc: CacheControl = { immutable: false, noStore: false, noCache: false }
  for (const part of (value ?? '').split(',')) {
    const [rawName = '', rawArg] = part.trim().split('=')
    const name = rawName.toLowerCase()
    if (name === 'immutable') cc.immutable = true
    else if (name === 'no-store') cc.noStore = true
    else if (name === 'no-cache') cc.noCache = true
    else if (name === 'max-age') {
      const n = Number.parseInt((rawArg ?? '').replace(/"/g, ''), 10)
      if (Number.isFinite(n) && n >= 0) cc.maxAge = n
    }
  }
  return cc
}

/** `vary` 里只有对预览不起作用的头（压缩方式、origin）才缓存；`*` 或别的（accept、cookie……）一律不缓存。 */
function varyAllowsCaching(vary: string | null): boolean {
  if (!vary) return true
  return vary
    .split(',')
    .map((v) => v.trim().toLowerCase())
    .filter((v) => v !== '')
    .every((v) => v === 'accept-encoding' || v === 'origin')
}

export type CacheLookup =
  | { kind: 'bypass' }
  | { kind: 'miss' }
  /** 缓存里的响应还新鲜，直接用。 */
  | { kind: 'fresh'; response: Response }
  /** 缓存里有，但要先问上游；`validators` 是要带的条件请求头。 */
  | { kind: 'stale'; validators: { etag?: string; lastModified?: string } }

export interface Collector {
  chunk(c: Uint8Array): void
  end(): Promise<void>
  abort(): void
}

export interface HttpCacheDeps {
  storage: CacheStorageLike
  index: IndexStore
  now?: () => number
  maxBytes?: number
  entryMaxBytes?: number
}

export class HttpCache {
  private cache: Promise<CacheLike> | undefined
  private entries: Promise<Map<string, CacheIndexEntry>> | undefined
  private readonly persisted = new Map<string, number>()
  private readonly now: () => number
  private readonly maxBytes: number
  private readonly entryMax: number

  constructor(private readonly deps: HttpCacheDeps) {
    this.now = deps.now ?? Date.now
    this.maxBytes = deps.maxBytes ?? CACHE_MAX_BYTES
    this.entryMax = deps.entryMaxBytes ?? ENTRY_MAX_BYTES
  }

  private open(): Promise<CacheLike> {
    this.cache ??= this.deps.storage.open(CACHE_NAME)
    return this.cache
  }

  private index(): Promise<Map<string, CacheIndexEntry>> {
    this.entries ??= this.deps.index.load().then((list) => new Map(list.map((e) => [e.url, e])))
    return this.entries
  }

  /** 这个请求能不能用缓存（读）。 */
  private static readable(req: Request): boolean {
    if (req.method !== 'GET' || req.mode === 'navigate') return false
    if (req.headers.has('range')) return false
    return req.cache !== 'no-store' && req.cache !== 'reload'
  }

  async lookup(req: Request): Promise<CacheLookup> {
    if (!HttpCache.readable(req)) return { kind: 'bypass' }
    const url = req.url
    const [cache, entries] = await Promise.all([this.open(), this.index()])
    const hit = await cache.match(url)
    const entry = entries.get(url)
    if (!hit || !entry) {
      if (hit || entry) await this.remove(url)
      return { kind: 'miss' }
    }
    const cc = parseCacheControl(hit.headers.get('cache-control'))
    const now = this.now()
    const fresh = req.cache !== 'no-cache' && !cc.noCache && (cc.immutable || (cc.maxAge !== undefined && now - entry.storedAt < cc.maxAge * 1000))
    if (fresh) {
      await this.touch(entry, now)
      return { kind: 'fresh', response: hit }
    }
    const etag = hit.headers.get('etag') ?? undefined
    const lastModified = hit.headers.get('last-modified') ?? undefined
    if (!etag && !lastModified) return { kind: 'miss' }
    return { kind: 'stale', validators: { etag, lastModified } }
  }

  /** 上游回了 `304`：取出缓存里的响应（并重新开始计新鲜期）。条目已经没了返回 `undefined`。 */
  async revalidated(url: string): Promise<Response | undefined> {
    const [cache, entries] = await Promise.all([this.open(), this.index()])
    const hit = await cache.match(url)
    const entry = entries.get(url)
    if (!hit || !entry) return undefined
    entry.storedAt = this.now()
    await this.touch(entry, entry.storedAt, true)
    return hit
  }

  /** 响应结束时把它存下来。不符合条件返回 `undefined`（响应照常转给页面）。 */
  collector(req: Request, status: number, headers: Headers): Collector | undefined {
    if (req.method !== 'GET' || req.mode === 'navigate' || req.cache === 'no-store' || req.headers.has('range')) return undefined
    if (status !== 200 || headers.has('content-range')) return undefined
    if (!headers.has('etag') && !headers.has('last-modified')) return undefined
    if (parseCacheControl(headers.get('cache-control')).noStore) return undefined
    if (!varyAllowsCaching(headers.get('vary'))) return undefined
    const url = req.url
    const stored = new Headers(headers)
    let chunks: Uint8Array[] | undefined = []
    let size = 0
    return {
      chunk: (c) => {
        if (!chunks) return
        size += c.length
        if (size > this.entryMax) chunks = undefined
        else chunks.push(c)
      },
      end: async () => {
        const parts = chunks
        chunks = undefined
        if (!parts) return
        const body = new Uint8Array(size)
        let at = 0
        for (const p of parts) {
          body.set(p, at)
          at += p.length
        }
        await this.store(url, new Response(body, { status: 200, headers: stored }), size).catch(() => {})
      },
      abort: () => {
        chunks = undefined
      },
    }
  }

  private async store(url: string, response: Response, size: number): Promise<void> {
    const [cache, entries] = await Promise.all([this.open(), this.index()])
    await cache.put(url, response)
    const now = this.now()
    const entry: CacheIndexEntry = { url, size, storedAt: now, lastUsed: now }
    entries.set(url, entry)
    this.persisted.set(url, now)
    await this.deps.index.put(entry)
    await this.evict(entries, cache)
  }

  /** 超过上限就按最近使用从旧到新删，直到放得下。 */
  private async evict(entries: Map<string, CacheIndexEntry>, cache: CacheLike): Promise<void> {
    let total = 0
    for (const e of entries.values()) total += e.size
    if (total <= this.maxBytes) return
    const byAge = [...entries.values()].sort((a, b) => a.lastUsed - b.lastUsed)
    for (const e of byAge) {
      if (total <= this.maxBytes) break
      total -= e.size
      entries.delete(e.url)
      this.persisted.delete(e.url)
      await Promise.all([cache.delete(e.url).catch(() => false), this.deps.index.delete(e.url).catch(() => {})])
    }
  }

  private async remove(url: string): Promise<void> {
    const [cache, entries] = await Promise.all([this.open(), this.index()])
    entries.delete(url)
    this.persisted.delete(url)
    await Promise.all([cache.delete(url).catch(() => false), this.deps.index.delete(url).catch(() => {})])
  }

  private async touch(entry: CacheIndexEntry, now: number, force = false): Promise<void> {
    entry.lastUsed = now
    const last = this.persisted.get(entry.url) ?? 0
    if (force || now - last >= TOUCH_PERSIST_MS) {
      this.persisted.set(entry.url, now)
      await this.deps.index.put({ ...entry }).catch(() => {})
    }
  }

  /** 缓存里现在有多少字节（诊断 / 测试）。 */
  async totalBytes(): Promise<number> {
    let total = 0
    for (const e of (await this.index()).values()) total += e.size
    return total
  }
}
