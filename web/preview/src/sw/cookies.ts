// service worker 一侧的 cookie jar：纯逻辑在 `shared/cookies.ts`，这里加 IndexedDB 持久化。
// service worker 随时可能被浏览器回收，所以 jar 在每次变化后写回，下次启动再读出来。
import { CookieJar, isCookieEntry, type CookieEntry } from '../shared/cookies'
import { STORES, idbGet, idbPut } from '../shared/idb'

export interface CookieStorage {
  load(): Promise<CookieEntry[]>
  save(entries: CookieEntry[]): Promise<void>
}

const KEY = 'jar'

export const idbCookieStorage: CookieStorage = {
  load: async () => {
    const raw = await idbGet<unknown>(STORES.cookies, KEY)
    return Array.isArray(raw) ? raw.filter(isCookieEntry) : []
  },
  save: (entries) => idbPut(STORES.cookies, KEY, entries),
}

export class CookieStore {
  private jar: Promise<CookieJar> | undefined

  constructor(
    private readonly storage: CookieStorage,
    private readonly now: () => number = Date.now,
  ) {}

  private load(): Promise<CookieJar> {
    this.jar ??= this.storage
      .load()
      .catch(() => [])
      .then((entries) => {
        const jar = new CookieJar(entries)
        jar.prune(this.now())
        return jar
      })
    return this.jar
  }

  /** 请求的 `cookie` 头（含 HttpOnly）。 */
  async header(requestPath: string): Promise<string> {
    const jar = await this.load()
    return jar.header(requestPath, this.now())
  }

  /** 页面在这个路径上读 `document.cookie` 会看到的值。 */
  async documentCookie(requestPath: string): Promise<string> {
    const jar = await this.load()
    return jar.documentCookie(requestPath, this.now())
  }

  /** 响应里的 `Set-Cookie`。页面看得到的 cookie 有变化返回 true（要通知页面）。 */
  async storeSetCookies(setCookies: readonly string[], requestPath: string): Promise<boolean> {
    if (setCookies.length === 0) return false
    return this.mutate((jar, now) => {
      for (const header of setCookies) jar.set(header, requestPath, now)
    })
  }

  /** 页面对 `document.cookie` 的赋值。 */
  async setFromDocument(value: string, documentPath: string): Promise<boolean> {
    return this.mutate((jar, now) => {
      jar.set(value, documentPath, now, true)
    })
  }

  private async mutate(change: (jar: CookieJar, now: number) => void): Promise<boolean> {
    const jar = await this.load()
    const now = this.now()
    const before = jar.visibleSignature(now)
    jar.prune(now)
    change(jar, now)
    const visibleChanged = jar.visibleSignature(now) !== before
    // HttpOnly 的变化页面看不到，但仍要持久化
    await this.storage.save(jar.all(now)).catch(() => {})
    return visibleChanged
  }

  /** 页面看得到的 cookie（非 HttpOnly）的全量快照。 */
  async visible(): Promise<CookieEntry[]> {
    const jar = await this.load()
    return jar.visible(this.now())
  }
}
