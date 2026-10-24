// 预览 origin 的 cookie jar（纯逻辑，没有 IO）。service worker 用它代管服务端设的 cookie（含 HttpOnly），
// shim 用它实现 `document.cookie`（只含非 HttpOnly）。
//
// 预览 origin 不用浏览器自己的 cookie：浏览器会把它们随每个到 hub 的请求（`/v1/ws`、`/__anycon/*`）一起发出去，
// 而 dev app 的 cookie 是预览内容，hub 不该看到（ADR-39、ADR-32）。
//
// 规则取自 RFC 6265 的子集（spec/web.md § 13.2）：认名、值、`Expires`、`Max-Age`、`Path`、`HttpOnly`；
// `Domain` 忽略（一律只属于本主机），`Secure` / `SameSite` 忽略。

export interface CookieEntry {
  name: string
  value: string
  path: string
  /** 毫秒时间戳；`null` 是会话 cookie。 */
  expiresAt: number | null
  createdAt: number
  httpOnly: boolean
}

export function isCookieEntry(v: unknown): v is CookieEntry {
  if (typeof v !== 'object' || v === null) return false
  const e = v as Record<string, unknown>
  return (
    typeof e.name === 'string' &&
    typeof e.value === 'string' &&
    typeof e.path === 'string' &&
    (e.expiresAt === null || typeof e.expiresAt === 'number') &&
    typeof e.createdAt === 'number' &&
    typeof e.httpOnly === 'boolean'
  )
}

/** jar 里最多留多少个 cookie，超了删最早建立的。 */
export const MAX_COOKIES = 180
/** 名字加值的字节数上限（RFC 6265bis：4096）。 */
export const MAX_COOKIE_BYTES = 4096
/** 会话 cookie 没有「浏览器关闭」可言，留 24 小时（和预览票据的默认有效期一致）。 */
export const SESSION_COOKIE_TTL_MS = 24 * 60 * 60 * 1000

/** RFC 6265 § 5.1.4：请求路径的目录部分。 */
export function defaultPath(requestPath: string): string {
  if (!requestPath.startsWith('/')) return '/'
  const last = requestPath.lastIndexOf('/')
  return last <= 0 ? '/' : requestPath.slice(0, last)
}

/** RFC 6265 § 5.1.4 的 path-match。`requestPath` 不含查询串。 */
export function pathMatches(requestPath: string, cookiePath: string): boolean {
  if (requestPath === cookiePath) return true
  if (!requestPath.startsWith(cookiePath)) return false
  return cookiePath.endsWith('/') || requestPath[cookiePath.length] === '/'
}

function byteLength(text: string): number {
  return new TextEncoder().encode(text).length
}

// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u0008\u000a-\u001f\u007f]/

export interface ParsedCookie {
  entry: CookieEntry
  /** `Max-Age <= 0` 或 `Expires` 已过：这条 `Set-Cookie` 是删除指令。 */
  remove: boolean
}

export function parseSetCookie(header: string, requestPath: string, now: number): ParsedCookie | undefined {
  const [pair = '', ...attrs] = header.split(';')
  const eq = pair.indexOf('=')
  if (eq < 0) return undefined
  const name = pair.slice(0, eq).trim()
  const value = pair.slice(eq + 1).trim()
  if (name === '' || CONTROL.test(name) || CONTROL.test(value)) return undefined
  if (byteLength(name) + byteLength(value) > MAX_COOKIE_BYTES) return undefined

  let path: string | undefined
  let httpOnly = false
  let maxAge: number | undefined
  let expires: number | undefined
  for (const attr of attrs) {
    const at = attr.indexOf('=')
    const key = (at < 0 ? attr : attr.slice(0, at)).trim().toLowerCase()
    const val = at < 0 ? '' : attr.slice(at + 1).trim()
    if (key === 'httponly') httpOnly = true
    else if (key === 'path') path = val.startsWith('/') ? val : undefined
    else if (key === 'max-age' && /^-?\d+$/.test(val)) maxAge = Number.parseInt(val, 10)
    else if (key === 'expires') {
      const t = Date.parse(val)
      if (!Number.isNaN(t)) expires = t
    }
  }
  // Max-Age 优先于 Expires
  let expiresAt: number | null = null
  if (maxAge !== undefined) expiresAt = maxAge <= 0 ? 0 : now + maxAge * 1000
  else if (expires !== undefined) expiresAt = expires
  return {
    entry: { name, value, path: path ?? defaultPath(requestPath), expiresAt, createdAt: now, httpOnly },
    remove: expiresAt !== null && expiresAt <= now,
  }
}

function live(entry: CookieEntry, now: number): boolean {
  return entry.expiresAt === null ? now - entry.createdAt < SESSION_COOKIE_TTL_MS : entry.expiresAt > now
}

const keyOf = (name: string, path: string): string => `${path}\0${name}`

export class CookieJar {
  private readonly cookies = new Map<string, CookieEntry>()

  constructor(entries: Iterable<CookieEntry> = []) {
    for (const e of entries) this.cookies.set(keyOf(e.name, e.path), { ...e })
  }

  /**
   * 处理一条 `Set-Cookie`（`fromScript = false`），或者页面对 `document.cookie` 的一次赋值（`fromScript = true`：
   * 脚本不能设 HttpOnly，也不能覆盖 HttpOnly 的 cookie）。jar 有变化返回 true。
   */
  set(header: string, requestPath: string, now: number, fromScript = false): boolean {
    const parsed = parseSetCookie(header, requestPath, now)
    if (!parsed) return false
    const { entry, remove } = parsed
    if (fromScript && entry.httpOnly) return false
    const key = keyOf(entry.name, entry.path)
    const old = this.cookies.get(key)
    if (fromScript && old?.httpOnly) return false
    if (remove) return this.cookies.delete(key)
    // 同名同路径的覆盖保留最初的建立时间（影响发送顺序）
    this.cookies.set(key, { ...entry, createdAt: old?.createdAt ?? entry.createdAt })
    this.evict()
    return true
  }

  private evict(): void {
    while (this.cookies.size > MAX_COOKIES) {
      let oldest: [string, CookieEntry] | undefined
      for (const item of this.cookies) if (!oldest || item[1].createdAt < oldest[1].createdAt) oldest = item
      if (!oldest) return
      this.cookies.delete(oldest[0])
    }
  }

  /** 删掉过期的。有删除返回 true。 */
  prune(now: number): boolean {
    let changed = false
    for (const [key, e] of this.cookies) {
      if (!live(e, now)) {
        this.cookies.delete(key)
        changed = true
      }
    }
    return changed
  }

  private matching(requestPath: string, now: number, includeHttpOnly: boolean): CookieEntry[] {
    const out: CookieEntry[] = []
    for (const e of this.cookies.values()) {
      if (live(e, now) && (includeHttpOnly || !e.httpOnly) && pathMatches(requestPath, e.path)) out.push(e)
    }
    // RFC 6265 § 5.4：路径长的在前，同长度按建立时间
    return out.sort((a, b) => b.path.length - a.path.length || a.createdAt - b.createdAt)
  }

  /** 请求的 `cookie` 头：路径匹配的全部 cookie（含 HttpOnly）。空串表示不带。 */
  header(requestPath: string, now: number): string {
    return this.matching(requestPath, now, true)
      .map((e) => `${e.name}=${e.value}`)
      .join('; ')
  }

  /** `document.cookie` 读到的值：路径匹配的非 HttpOnly cookie。 */
  documentCookie(requestPath: string, now: number): string {
    return this.matching(requestPath, now, false)
      .map((e) => `${e.name}=${e.value}`)
      .join('; ')
  }

  /** 全部未过期的非 HttpOnly cookie（发给页面的快照）。 */
  visible(now: number): CookieEntry[] {
    return [...this.cookies.values()].filter((e) => live(e, now) && !e.httpOnly).map((e) => ({ ...e }))
  }

  /** 全部未过期的 cookie（持久化用）。 */
  all(now: number): CookieEntry[] {
    return [...this.cookies.values()].filter((e) => live(e, now)).map((e) => ({ ...e }))
  }

  /** shim 收到 service worker 的快照：整个换掉。 */
  replace(entries: Iterable<CookieEntry>): void {
    this.cookies.clear()
    for (const e of entries) this.cookies.set(keyOf(e.name, e.path), { ...e })
  }

  /** 用来判断「页面看得到的 cookie 有没有变」。 */
  visibleSignature(now: number): string {
    return JSON.stringify(
      this.visible(now)
        .map((e) => [e.path, e.name, e.value, e.expiresAt])
        .sort(),
    )
  }

  get size(): number {
    return this.cookies.size
  }
}

/**
 * 把 `document.cookie` 形式的 `a=1; b=2` 变成 cookie 条目（路径 `/`，会话 cookie）：service worker 渲染进 HTML 的
 * `data-cookies` 是这个形式，shim 拿它做第一份快照，直到收到 worker 发来的完整快照。
 */
export function parseCookiePairs(text: string, now: number): CookieEntry[] {
  const out: CookieEntry[] = []
  for (const part of text.split(';')) {
    const eq = part.indexOf('=')
    if (eq < 0) continue
    const name = part.slice(0, eq).trim()
    if (name === '') continue
    out.push({ name, value: part.slice(eq + 1).trim(), path: '/', expiresAt: null, createdAt: now, httpOnly: false })
  }
  return out
}
