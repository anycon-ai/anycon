// 测试用的内存假货：Cache Storage、缓存索引、cookie 存储。
import type { CacheIndexEntry, CacheLike, CacheStorageLike, IndexStore } from '../sw/http/cache'
import type { CookieStorage } from '../sw/cookies'
import type { CookieEntry } from '../shared/cookies'

interface Stored {
  status: number
  headers: [string, string][]
  body: Uint8Array
}

export class MemoryCache implements CacheLike {
  readonly items = new Map<string, Stored>()
  async match(url: string): Promise<Response | undefined> {
    const s = this.items.get(url)
    return s && new Response(s.body.slice(), { status: s.status, headers: s.headers })
  }
  async put(url: string, response: Response): Promise<void> {
    this.items.set(url, { status: response.status, headers: [...response.headers], body: new Uint8Array(await response.arrayBuffer()) })
  }
  async delete(url: string): Promise<boolean> {
    return this.items.delete(url)
  }
}

export class MemoryCacheStorage implements CacheStorageLike {
  readonly caches = new Map<string, MemoryCache>()
  async open(name: string): Promise<CacheLike> {
    let c = this.caches.get(name)
    if (!c) this.caches.set(name, (c = new MemoryCache()))
    return c
  }
}

export class MemoryIndex implements IndexStore {
  readonly entries = new Map<string, CacheIndexEntry>()
  async load(): Promise<CacheIndexEntry[]> {
    return [...this.entries.values()].map((e) => ({ ...e }))
  }
  async put(entry: CacheIndexEntry): Promise<void> {
    this.entries.set(entry.url, { ...entry })
  }
  async delete(url: string): Promise<void> {
    this.entries.delete(url)
  }
}

export class MemoryCookies implements CookieStorage {
  saved: CookieEntry[] = []
  saves = 0
  async load(): Promise<CookieEntry[]> {
    return this.saved.map((e) => ({ ...e }))
  }
  async save(entries: CookieEntry[]): Promise<void> {
    this.saved = entries.map((e) => ({ ...e }))
    this.saves += 1
  }
}
