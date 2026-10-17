import type { QueryClient } from '@tanstack/react-query'
import { createRouter } from '@tanstack/react-router'
import { setUnauthorizedHandler } from './api/client'
import { loginRedirectTarget } from './lib/loginRedirect'
import { routeTree } from './routeTree.gen'
import { NotFoundPage } from './pages/NotFoundPage'

export interface RouterContext {
  queryClient: QueryClient
}

export function createAppRouter(queryClient: QueryClient) {
  return createRouter({
    routeTree,
    context: { queryClient },
    defaultPreload: 'intent',
    defaultNotFoundComponent: () => <NotFoundPage />,
    scrollRestoration: true,
  })
}

/**
 * 任何接口返回 401（登录态失效）时回登录页。用 replace：登录页不该在历史栈里压一条；
 * `redirect` 只带路径、不带 hash（配对链接的 `#code=` 在 `loginRedirectTarget` 里被拿走，见 docs/spec/web.md § 4.3）。
 */
export function installUnauthorizedHandler(router: ReturnType<typeof createAppRouter>, queryClient: QueryClient): void {
  setUnauthorizedHandler(() => {
    const here = router.state.location
    if (here.pathname === '/login' || here.pathname.startsWith('/invite/')) return
    queryClient.removeQueries({ queryKey: ['me'] })
    void router.navigate({ to: '/login', search: { redirect: loginRedirectTarget(here) }, replace: true })
  })
}

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof createAppRouter>
  }
}
