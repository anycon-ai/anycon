// /admin：multi 里没有邀请（接口是 404，页面也不显示、不请求）；账号管理两种模式都有。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { installFakeApi } from '@/test/fakeApi'

vi.mock('@/components/PageHeader', () => ({ PageHeader: () => null }))

import { AdminPage } from './AdminPage'

function mount(mode: 'single' | 'multi', isAdmin = true) {
  const api = installFakeApi({
    'GET /api/config': () => ({ mode, protocol_version: 1, version: '0.0.0', login_methods: mode === 'multi' ? ['email'] : ['token'] }),
    'GET /api/me': () => ({ id: 'me', display_name: 'Root', is_admin: isAdmin, login_methods: ['token'] }),
    'GET /api/invites': () => [{ id: 'i1', note: 'for Carol', expires_at_ms: Date.now() + 86_400_000 }],
    'GET /api/admin/status': () => ({ mode, accounts: 3 }),
    'GET /api/admin/accounts': () => [{ id: 'me', display_name: 'Root', is_admin: true, admin_by_config: false, created_at_ms: 1 }],
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <AdminPage />
    </QueryClientProvider>,
  )
  return api
}

afterEach(() => vi.unstubAllGlobals())

describe('AdminPage', () => {
  it('single: invitations, accounts and status', async () => {
    const api = mount('single')
    expect(await screen.findByTestId('admin-invites')).toBeInTheDocument()
    expect(await screen.findByText('for Carol')).toBeInTheDocument()
    expect(await screen.findByTestId('admin-account-me')).toBeInTheDocument()
    expect(api.callsTo('GET /api/invites')).toHaveLength(1)
  })

  it('multi: no invitations section and no request for them; accounts and status stay', async () => {
    const api = mount('multi')
    expect(await screen.findByTestId('admin-account-me')).toBeInTheDocument()
    expect(await screen.findByTestId('admin-status')).toBeInTheDocument()
    expect(screen.queryByTestId('admin-invites')).toBeNull()
    expect(screen.queryByTestId('invite-new')).toBeNull()
    expect(api.callsTo('GET /api/invites')).toHaveLength(0)
  })

  it('a non-admin sees the "administrators only" page and the hub is not asked for accounts', async () => {
    const api = mount('multi', false)
    expect(await screen.findByText('Administrators only')).toBeInTheDocument()
    expect(api.callsTo('GET /api/admin/accounts')).toHaveLength(0)
  })
})
