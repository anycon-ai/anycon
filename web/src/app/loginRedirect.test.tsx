// 未登录时打开 `/pair#code=…`，短码不能进 `/login?redirect=` 的 query，也不能留在历史栈里；
// 登录后回到 /pair，确认步骤里仍然有这个短码，且 sessionStorage 里的暂存已被删掉。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider } from '@tanstack/react-router'
import { createMemoryHistory } from '@tanstack/react-router'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('@/app/bootstrap', () => ({ startTransport: vi.fn(() => Promise.resolve()), stopTransport: vi.fn() }))
vi.mock('@/transport/instance', () => ({
  getClient: () => ({ pair: vi.fn(), deviceName: 'Chrome · macOS', snapshot: { paired: false, deviceId: null } }),
  rpc: () => ({ request: vi.fn(), isReady: () => false }),
}))

import { createAppRouter, installUnauthorizedHandler } from '@/router'
import { setUnauthorizedHandler } from '@/api/client'
import { useHub } from '@/stores/hub'

const CODE = 'ABCD-2345'
const config = { mode: 'self_hosted', login_methods: ['token'], protocol_version: 1, version: '0.0.0' }
const me = { id: 'u1', display_name: 'U', is_admin: false, login_methods: ['token'] }

let loggedIn = false

function json(status: number, body?: unknown): Response {
  return new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
}

function mount(initial: string) {
  const entries = [initial]
  const history = createMemoryHistory({ initialEntries: entries })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const router = createAppRouter(queryClient)
  router.update({ history, context: { queryClient } })
  installUnauthorizedHandler(router, queryClient)
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  )
  return { entries, router, queryClient }
}

beforeEach(() => {
  loggedIn = false
  sessionStorage.clear()
  useHub.setState({ started: true })
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      if (url === '/api/config') return json(200, config)
      if (url === '/api/me') return loggedIn ? json(200, me) : json(401, { error: { code: 'unauthorized', message: 'no session' } })
      if (url === '/auth/token') {
        loggedIn = true
        return new Response(null, { status: 204 })
      }
      return json(404, { error: { code: 'not_found', message: url } })
    }),
  )
})
afterEach(() => {
  setUnauthorizedHandler(null)
  vi.unstubAllGlobals()
  sessionStorage.clear()
})

describe('sign-in redirect keeps the pairing code out of the URL', () => {
  it('/pair#code=… → /login has no code in search, hash or history; after sign-in the confirm step still shows the code', async () => {
    const user = userEvent.setup()
    const { entries, router } = mount(`/pair#code=${CODE}`)

    // 落在登录页
    await screen.findByTestId('login-token-input')
    const loc = router.state.location
    expect(loc.pathname).toBe('/login')
    expect(loc.search).toEqual({ redirect: '/pair' })
    expect(loc.hash).toBe('')
    expect(loc.href).not.toContain('ABCD')
    expect(loc.href).not.toContain('2345')
    expect(decodeURIComponent(loc.href)).not.toContain('code')
    // 历史栈（replace 而不是 push）：没有任何一条带短码，也没有留下 /pair#code= 那一条
    expect(entries).toHaveLength(1)
    expect(entries.join('\n')).not.toContain('ABCD')
    expect(entries.join('\n')).not.toContain('2345')
    // 短码在 sessionStorage 里等着，不在 URL 里
    expect(sessionStorage.getItem('anycon.pending_pair_code')).toContain(CODE)

    // 登录
    await user.type(screen.getByTestId('login-token-input'), 'anycon_test_token')
    await user.click(screen.getByTestId('login-submit'))

    // 回到 /pair，停在确认步骤，短码取出来了
    expect(await screen.findByTestId('pair-link-confirm')).toBeInTheDocument()
    expect(screen.getByTestId('pair-link-code')).toHaveTextContent(CODE)
    // 取出就删
    await waitFor(() => expect(sessionStorage.getItem('anycon.pending_pair_code')).toBeNull())
    // 地址栏 / 历史栈里始终没有短码
    expect(router.state.location.pathname).toBe('/pair')
    expect(router.state.location.href).not.toContain('ABCD')
    expect(entries.join('\n')).not.toContain('ABCD')
    expect(entries.join('\n')).not.toContain('2345')
  })

  it('the legacy ?code= form is stripped from the redirect too, and a session that expires later (401) uses replace', async () => {
    loggedIn = true
    const { entries, router } = mount('/pair')
    await screen.findByTestId('pair-code-input')
    // 登录态失效：某个接口 401 → 全局处理回登录页；此时用户正好在带 ?code= 的页面上
    await router.navigate({ to: '/setup', search: { code: 'ABCD-2345' }, hash: '' })
    loggedIn = false
    const before = entries.length
    const { apiFetch } = await import('@/api/client')
    await apiFetch('/api/me').catch(() => {})
    await screen.findByTestId('login-token-input')
    expect(router.state.location.search).toEqual({ redirect: '/setup' })
    expect(router.state.location.href).not.toContain('ABCD')
    // replace：历史栈没有变长，也没有任何一条带短码
    expect(entries.length).toBe(before)
    expect(entries.join('\n')).not.toContain('ABCD')
  })

  it('does not stash anything for pages that carry no code', async () => {
    mount('/n/abc/settings?tab=devices')
    await screen.findByTestId('login-token-input')
    expect(sessionStorage.getItem('anycon.pending_pair_code')).toBeNull()
  })
})
