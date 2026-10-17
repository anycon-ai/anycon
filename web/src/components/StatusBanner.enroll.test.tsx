// § 3.4 第 3 条：有设备请求接入某台节点 → 蓝色横幅，带「查看」；请求结束就消失。
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useDrafts } from '@/stores/drafts'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))
vi.mock('@/transport/instance', () => ({ getClient: () => ({ forceReconnect: vi.fn(), takeOver: vi.fn() }) }))
// 这里只测接入请求横幅；「通知」横幅（要 react-query 和路由）在 StatusBanner.notifications.test.tsx
vi.mock('@/features/push/usePrompt', () => ({ useNotificationsPrompt: () => false, dismissNotificationsPrompt: vi.fn() }))

import { StatusBanner } from './StatusBanner'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)

beforeEach(() => {
  navigate.mockReset()
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'mac-mini' }), [B]: nodeEntry({ id: B, name: 'nas' }) }, order: [A, B], loaded: true })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
  useHub.setState({ started: true, snapshot: { ...useHub.getState().snapshot, phase: 'hub_authed', halt: null, attempt: 0 } })
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

const banner = () => screen.queryByTestId('status-banner')

describe('enroll request banner', () => {
  it('appears for a request on any node, names the node, and "Review" opens the approval dialog', async () => {
    const user = userEvent.setup()
    render(<StatusBanner nodeId={A} nodeConn="ready" />)
    expect(banner()).toBeNull()
    act(() => useEnroll.getState().upsert(B, enrollInfo({ enroll_id: 'x' })))
    expect(banner()).toHaveAttribute('data-kind', 'enroll_request')
    expect(banner()).toHaveTextContent('nas')
    await user.click(screen.getByTestId('banner-enroll-review'))
    expect(useEnroll.getState().dialog).toEqual({ nodeId: B, enrollId: 'x' })
  })

  it('prefers the node you are looking at, and counts when there are several', () => {
    act(() => {
      useEnroll.getState().upsert(B, enrollInfo({ enroll_id: 'x' }))
      useEnroll.getState().upsert(A, enrollInfo({ enroll_id: 'y' }))
    })
    render(<StatusBanner nodeId={A} nodeConn="ready" />)
    expect(banner()).toHaveTextContent(/2/)
    screen.getByTestId('banner-enroll-review').click()
    expect(useEnroll.getState().dialog).toEqual({ nodeId: A, enrollId: 'y' })
  })

  it('disappears when the request is resolved or expires', () => {
    render(<StatusBanner nodeId={A} nodeConn="ready" />)
    act(() => useEnroll.getState().upsert(A, enrollInfo({ enroll_id: 'x' })))
    expect(banner()).not.toBeNull()
    act(() => useEnroll.getState().resolve(A, 'x', 'approved', 'phone'))
    expect(banner()).toBeNull()
  })

  it('gives way to "node offline" and does not show for a scan request (those never push)', () => {
    act(() => useEnroll.getState().upsert(A, enrollInfo({ enroll_id: 'x' })))
    const { unmount } = render(<StatusBanner nodeId={A} nodeConn="peer_offline" />)
    expect(banner()).toHaveAttribute('data-kind', 'node_offline')
    unmount()
    useEnroll.setState({ items: {}, resolved: {} })
    act(() => useEnroll.getState().upsert(A, enrollInfo({ enroll_id: 's', method: 'scan', approval: undefined })))
    render(<StatusBanner nodeId={A} nodeConn="ready" />)
    expect(banner()).toBeNull()
  })
})
