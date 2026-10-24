// 真路由下的 OAuth / 邀请入口：`/login?oauth_error=` 的查询串传到页面、登录重定向变成 OAuth 的 `next`、
// `/invite/:code` 在 single 带着邀请码落到登录页，在 multi 说明注册已开放。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider, createMemoryHistory } from '@tanstack/react-router'
import { render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('@/app/bootstrap', () => ({ startTransport: vi.fn(() => Promise.resolve()), stopTransport: vi.fn() }))
vi.mock('@/transport/instance', () => ({
  getClient: () => ({ pair: vi.fn(), deviceName: 'Chrome · macOS', snapshot: { paired: false, deviceId: null } }),
  rpc: () => ({ request: vi.fn(), isReady: () => false }),
}))

import { setUnauthorizedHandler } from '@/api/client'
import { createAppRouter, installUnauthorizedHandler } from '@/router'
import { useHub } from '@/stores/hub'

let config: Record<string, unknown>

function json(status: number, body?: unknown): Response {
  return new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
}

function mount(initial: string) {
  const history = createMemoryHistory({ initialEntries: [initial] })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const router = createAppRouter(queryClient)
  router.update({ history, context: { queryClient } })
  installUnauthorizedHandler(router, queryClient)
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  )
  return router
}

beforeEach(() => {
  sessionStorage.clear()
  useHub.setState({ started: true })
  config = { mode: 'multi', public_url: location.origin, login_methods: ['github', 'google', 'email'], protocol_version: 1, version: '0.0.0' }
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      if (url === '/api/config') return json(200, config)
      if (url === '/api/me') return json(401, { error: { code: 'unauthorized', message: 'no session' } })
      return json(404, { error: { code: 'not_found', message: url } })
    }),
  )
})
afterEach(() => {
  setUnauthorizedHandler(null)
  vi.unstubAllGlobals()
  sessionStorage.clear()
})

describe('/login with OAuth', () => {
  it('?oauth_error=&provider= reaches the page and shows as an inline error next to the buttons', async () => {
    mount('/login?oauth_error=cancelled&provider=github')
    expect(await screen.findByTestId('oauth-error')).toHaveTextContent('GitHub sign-in was cancelled.')
    expect(screen.getByTestId('oauth-github')).toHaveAttribute('href', '/auth/oauth/github/start')
  })

  it('the redirect to a protected page becomes the OAuth next= (path and query only)', async () => {
    const router = mount('/pair')
    const github = await screen.findByTestId('oauth-github')
    expect(router.state.location.pathname).toBe('/login')
    expect(new URL(github.getAttribute('href')!, 'http://h').searchParams.get('next')).toBe('/pair')
  })
})

describe('/invite/:code', () => {
  it('single: lands on /login holding the code; the OAuth buttons carry it as invite=', async () => {
    config = { ...config, mode: 'single', login_methods: ['github', 'token'] }
    const router = mount('/invite/inv_xyz')
    const github = await screen.findByTestId('oauth-github')
    expect(router.state.location.pathname).toBe('/login')
    expect(new URL(github.getAttribute('href')!, 'http://h').searchParams.get('invite')).toBe('inv_xyz')
    expect(screen.getByTestId('login-invite-accept')).toBeInTheDocument()
  })

  it('multi: stays on the invite URL and says registration is open', async () => {
    const router = mount('/invite/inv_xyz')
    expect(await screen.findByTestId('invite-open-registration')).toHaveTextContent('Registration is open')
    expect(router.state.location.pathname).toBe('/invite/inv_xyz')
    expect(sessionStorage.getItem('invite')).toBeNull()
  })
})
