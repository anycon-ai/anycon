// `/invite/:code`：single 暂存邀请码并转 /login；multi 没有邀请，说一声「注册已开放」并引到 /login。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { installFakeApi } from '@/test/fakeApi'
import { mockRouter } from '@/test/routerMock'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))

import { InvitePage } from './InvitePage'

function mount(config: Record<string, unknown> | 'error') {
  installFakeApi({ 'GET /api/config': () => (config === 'error' ? { status: 500, body: { error: { code: 'internal', message: 'x' } } } : { protocol_version: 1, version: '0.0.0', login_methods: ['token'], ...config }) })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <InvitePage code="inv_abc" />
    </QueryClientProvider>,
  )
}

beforeEach(() => {
  navigate.mockReset()
  sessionStorage.clear()
})
afterEach(() => vi.unstubAllGlobals())

describe('InvitePage', () => {
  it('single: keeps the code for the login page (which offers it to OAuth too) and goes there', async () => {
    mount({ mode: 'single' })
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/login', replace: true }))
    expect(sessionStorage.getItem('invite')).toBe('inv_abc')
    expect(screen.queryByTestId('invite-open-registration')).toBeNull()
  })

  it('multi: says registration is open, links to /login, and keeps no code', async () => {
    mount({ mode: 'multi', login_methods: ['github', 'email'] })
    const card = await screen.findByTestId('invite-open-registration')
    expect(card).toHaveTextContent(/Registration is open/)
    expect(screen.getByTestId('invite-go-login').closest('a')).toHaveAttribute('href', '/login')
    expect(navigate).not.toHaveBeenCalled()
    expect(sessionStorage.getItem('invite')).toBeNull()
  })

  it('a hub whose config cannot be read still lands on /login (it shows the error there)', async () => {
    mount('error')
    // useConfig 失败后会重试一次（默认间隔 1 秒）才算完
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/login', replace: true }), { timeout: 4000 })
  })
})
