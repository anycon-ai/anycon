// 组件测试里不起真正的路由：把 `useNavigate` / `useParams` / `Link` 换成桩，断言「跳到了哪里」即可。
import type { AnchorHTMLAttributes, ReactNode } from 'react'

type LinkProps = { to: string; params?: Record<string, string>; search?: Record<string, string | undefined>; children?: ReactNode } & Omit<AnchorHTMLAttributes<HTMLAnchorElement>, 'href'>

export function StubLink({ to, params, search, children, ...rest }: LinkProps) {
  const path = Object.entries(params ?? {}).reduce((acc, [k, v]) => acc.replace(`$${k}`, v), to)
  const query = new URLSearchParams(Object.entries(search ?? {}).filter((e): e is [string, string] => e[1] !== undefined)).toString()
  const href = query ? `${path}?${query}` : path
  return (
    <a href={href} {...rest}>
      {children}
    </a>
  )
}

/** 用在 `vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig(), navigate, params))`。 */
export function mockRouter<T extends object>(actual: T, navigate: (opts: unknown) => unknown, params: Record<string, string> = {}): T {
  return { ...actual, useNavigate: () => navigate, useParams: () => params, Link: StubLink }
}
