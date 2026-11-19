// 组件测试里不起真正的路由：把 `useNavigate` / `useParams` / `Link` 换成桩，断言「跳到了哪里」即可。
import type { AnchorHTMLAttributes, ReactNode } from 'react'

type LinkProps = { to: string; params?: Record<string, string>; search?: Record<string, string | undefined>; children?: ReactNode } & Omit<AnchorHTMLAttributes<HTMLAnchorElement>, 'href'>

export function StubLink({ to, params, search, children, ...rest }: LinkProps) {
  // 通配段（`/files/$`）的参数名是 `_splat`，路径里就是结尾的 `$`；和路由器一样按段做百分号编码、保留 `/`
  const path = Object.entries(params ?? {}).reduce(
    (acc, [k, v]) => (k === '_splat' ? acc.replace(/\$$/, v.split('/').map(encodeURIComponent).join('/')) : acc.replace(`$${k}`, v)),
    to,
  )
  const query = new URLSearchParams(Object.entries(search ?? {}).filter((e): e is [string, string] => e[1] !== undefined)).toString()
  const href = query ? `${path}?${query}` : path
  return (
    <a href={href} {...rest}>
      {children}
    </a>
  )
}

/**
 * 用在 `vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig(), navigate, params))`。
 * `pathname`：`useRouterState` 里的当前路径（高亮导航项、判断是不是标签页根页面）；`search`：`useSearch`。
 * 返回按钮：`useCanGoBack` 恒为真，`router.history.back()` 记成 `navigate({ to: '-1' })`。
 */
export function mockRouter<T extends object>(actual: T, navigate: (opts: unknown) => unknown, params: Record<string, string> = {}, extra: { pathname?: string; search?: Record<string, unknown> } = {}): T {
  const state = { location: { pathname: extra.pathname ?? '/', search: extra.search ?? {} } }
  return {
    ...actual,
    useNavigate: () => navigate,
    useParams: () => params,
    useSearch: () => extra.search ?? {},
    useRouterState: (o?: { select?: (s: typeof state) => unknown }) => (o?.select ? o.select(state) : state),
    useCanGoBack: () => true,
    useRouter: () => ({ history: { back: () => navigate({ to: '-1' }) } }),
    Link: StubLink,
  }
}
