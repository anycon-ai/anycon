// 账号设置的侧边导航里有「通知」（ADR-37）。
import { render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'

vi.mock('@tanstack/react-router', async (orig) => ({ ...mockRouter(await orig<object>(), vi.fn(), {}), Outlet: () => null }))
vi.mock('./PageHeader', () => ({ PageHeader: () => null }))

import { SettingsLayout } from './SettingsLayout'

describe('SettingsLayout', () => {
  it('lists Notifications between Devices and Appearance, linking to /settings/notifications', () => {
    render(<SettingsLayout />)
    const items = screen.getAllByTestId(/^settings-nav-/)
    expect(items.map((i) => i.getAttribute('data-testid'))).toEqual(['profile', 'login', 'sessions', 'devices', 'notifications', 'appearance'].map((id) => `settings-nav-${id}`))
    expect(screen.getByTestId('settings-nav-notifications')).toHaveAttribute('href', '/settings/notifications')
  })
})
