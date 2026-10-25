// 账号设置的侧边导航：通知在设备和外观之间（ADR-37）；配额只有 `multi` 才有。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { installFakeApi } from '@/test/fakeApi'
import { mockRouter } from '@/test/routerMock'

vi.mock('@tanstack/react-router', async (orig) => ({ ...mockRouter(await orig<object>(), vi.fn(), {}), Outlet: () => null }))
vi.mock('./PageHeader', () => ({ PageHeader: () => null }))

import { SettingsLayout } from './SettingsLayout'

afterEach(() => vi.unstubAllGlobals())

function mount(mode: 'single' | 'multi') {
  installFakeApi({ 'GET /api/config': () => ({ mode, protocol_version: 1, version: '0.0.0', login_methods: mode === 'multi' ? ['email'] : ['token'] }) })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <SettingsLayout />
    </QueryClientProvider>,
  )
}
const ids = () => screen.getAllByTestId(/^settings-nav-/).map((i) => i.getAttribute('data-testid'))

describe('SettingsLayout', () => {
  it('single: lists Notifications between Devices and Appearance, linking to /settings/notifications, and has no Quota tab', async () => {
    mount('single')
    expect(await screen.findByTestId('settings-nav-notifications')).toHaveAttribute('href', '/settings/notifications')
    expect(ids()).toEqual(['profile', 'login', 'sessions', 'devices', 'notifications', 'appearance'].map((id) => `settings-nav-${id}`))
    expect(screen.queryByTestId('settings-nav-quota')).toBeNull()
  })

  it('multi: adds a Quota tab right after Devices', async () => {
    mount('multi')
    expect(await screen.findByTestId('settings-nav-quota')).toHaveAttribute('href', '/settings/quota')
    expect(ids()).toEqual(['profile', 'login', 'sessions', 'devices', 'quota', 'notifications', 'appearance'].map((id) => `settings-nav-${id}`))
  })
})
