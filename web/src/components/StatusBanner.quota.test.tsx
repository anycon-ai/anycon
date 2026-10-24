// `auth_fail { quota }`（连接数用完）：横幅说明原因并给「查看用量」，指向 设置 → 配额。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'
import { useDrafts } from '@/stores/drafts'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))
vi.mock('@/transport/instance', () => ({ getClient: () => ({ forceReconnect: vi.fn(), takeOver: vi.fn() }) }))
vi.mock('@/features/push/usePrompt', () => ({ useNotificationsPrompt: () => false, dismissNotificationsPrompt: vi.fn() }))

import { StatusBanner } from './StatusBanner'

beforeEach(() => {
  navigate.mockReset()
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useHub.setState({ started: true, snapshot: { ...useHub.getState().snapshot, phase: 'hub_disconnected', halt: 'quota', attempt: 1 } })
})

describe('hub_quota banner', () => {
  it('names the limit and points to Account settings → Devices, with a button to the quota page', async () => {
    const user = userEvent.setup()
    render(<StatusBanner nodeId={null} nodeConn={null} />)
    const banner = screen.getByTestId('status-banner')
    expect(banner).toHaveAttribute('data-kind', 'hub_quota')
    expect(banner).toHaveTextContent(/limit on simultaneous connections/)
    expect(banner).toHaveTextContent(/Account settings → Devices/)
    await user.click(screen.getByTestId('banner-quota-view'))
    expect(navigate).toHaveBeenCalledWith({ to: '/settings/quota' })
  })
})
