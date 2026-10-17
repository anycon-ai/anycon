// 未登录时跳登录页：`redirect` 参数怎么构造（docs/protocol.md § 4.1：链接短码不进 query；docs/spec/web.md § 4.3）。
//
// 配对链接 `/pair#code=XXXX-XXXX` 的短码在 URL fragment 里，浏览器不会把它发给 hub / 反代。
// 如果把整个 `location.href` 塞进 `/login?redirect=…`，短码就进了 query：刷新、手机浏览器回收后重载标签页、
// 复制地址到新标签页都会发出带短码的整页请求，短码就到了 hub 和反代的访问日志里。
//
// 所以：redirect 只带路径（+ 不含 `code` 的 search），**绝不带 hash**；链接里的短码在跳转前放进 `sessionStorage`
// （只在这个标签页里、只有同源脚本读得到、几分钟后作废），登录后回到 `/pair` 的 `useLinkCode` 取出并立即删除。

const STORAGE_KEY = 'anycon.pending_pair_code'
/** 暂存的短码最多留这么久。配对码本身 5 分钟就过期，留太久没意义。 */
export const PENDING_CODE_TTL_MS = 10 * 60_000

/** 会带短码的路由。 */
type CodeRoute = '/pair' | '/setup'
const CODE_ROUTES: readonly string[] = ['/pair', '/setup']

export interface LocationParts {
  pathname: string
  /** 带前导 `?` 的 search 串（TanStack Router 的 `location.searchStr`），没有就是 ''。 */
  searchStr?: string
  /** 带前导 `#` 的 hash，没有就是 ''。 */
  hash?: string
}

function codeFrom(loc: LocationParts): string {
  const fromHash = new URLSearchParams((loc.hash ?? '').replace(/^#/, '')).get('code')
  // 旧版链接把短码放在 `?code=`
  const fromSearch = new URLSearchParams(loc.searchStr ?? '').get('code')
  return fromHash ?? fromSearch ?? ''
}

/**
 * 跳登录页之前调用：返回要放进 `?redirect=` 的值（路径 + 去掉 `code` 的 search，没有 hash），
 * 同时把链接里的短码暂存进 `sessionStorage`。
 */
export function loginRedirectTarget(loc: LocationParts): string {
  const onCodeRoute = CODE_ROUTES.includes(loc.pathname)
  const params = new URLSearchParams(loc.searchStr ?? '')
  if (onCodeRoute) {
    const code = codeFrom(loc)
    if (code) stashPairCode(loc.pathname as CodeRoute, code)
    params.delete('code')
  }
  const qs = params.toString()
  return loc.pathname + (qs ? `?${qs}` : '')
}

/** 暂存配对链接里的短码。`sessionStorage` 不可用（隐私模式 / 被禁）时短码就丢了，用户重新输入即可。 */
export function stashPairCode(to: CodeRoute, code: string): void {
  try {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ to, code, at: Date.now() }))
  } catch (e) {
    console.warn('anycon: could not keep the pairing code across the sign-in redirect', e)
  }
}

/** 读出暂存的短码（路由要对得上、没过期），不删。没有返回 ''。读和删分开：渲染期只读，挂载后的 effect 里再 `clearPendingPairCode()`。 */
export function peekPendingPairCode(to: CodeRoute): string {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY)
    if (!raw) return ''
    const v: unknown = JSON.parse(raw)
    if (typeof v !== 'object' || v === null) return ''
    const o = v as Record<string, unknown>
    if (o.to !== to || typeof o.code !== 'string' || typeof o.at !== 'number') return ''
    if (Date.now() - o.at > PENDING_CODE_TTL_MS) return ''
    return o.code
  } catch {
    return ''
  }
}

export function clearPendingPairCode(): void {
  try {
    sessionStorage.removeItem(STORAGE_KEY)
  } catch {
    /* ignore */
  }
}
