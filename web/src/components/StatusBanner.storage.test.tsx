// 本地写入失败（空间满 / 被禁用）：横幅说明后果，「管理存储」去 /settings/data，「关闭」只收起横幅。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'
import { useDrafts } from '@/stores/drafts'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

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
  useHub.setState({ started: true, snapshot: { ...useHub.getState().snapshot, phase: 'hub_authed', halt: null, attempt: 0 } })
  useUi.getState().setStorageError(true)
})

describe('storage banner', () => {
  it('links to the storage page', async () => {
    const user = userEvent.setup()
    render(<StatusBanner nodeId={null} nodeConn={null} />)
    expect(screen.getByTestId('status-banner')).toHaveAttribute('data-kind', 'storage')
    await user.click(screen.getByTestId('banner-storage-manage'))
    expect(navigate).toHaveBeenCalledWith({ to: '/settings/data' })
  })

  it('dismiss only hides the banner', async () => {
    const user = userEvent.setup()
    render(<StatusBanner nodeId={null} nodeConn={null} />)
    await user.click(screen.getByTestId('banner-storage-dismiss'))
    expect(useUi.getState().storageError).toBe(false)
    expect(navigate).not.toHaveBeenCalled()
    expect(screen.queryByTestId('status-banner')).toBeNull()
  })
})
