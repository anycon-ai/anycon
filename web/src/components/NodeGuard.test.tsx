// /n/:node 的守卫：本账号的节点、只是这个浏览器还没配对 → /unlock/:node；不是本账号的 → 404（spec/web.md § 2）。
import { render, screen } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { mockRouter } from '@/test/routerMock'
import { nodeEntry } from '@/test/factories'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({
  navigate: vi.fn(),
  params: { node: 'aaaaaaaa' } as Record<string, string>,
  devices: { data: undefined as unknown, isPending: false, fetchStatus: 'idle', isError: false },
  enabled: [] as Array<boolean | undefined>,
}))
vi.mock('@tanstack/react-router', async (orig) => {
  const actual = await orig<object>()
  return {
    ...mockRouter(actual, h.navigate, {}),
    useParams: () => h.params,
    Navigate: (props: unknown) => {
      h.navigate(props)
      return null
    },
    Outlet: () => <div data-testid="outlet" />,
    useRouter: () => ({ history: { back: vi.fn() } }),
  }
})
vi.mock('@/api/queries', () => ({
  useDevices: (o?: { enabled?: boolean }) => {
    h.enabled.push(o?.enabled)
    return h.devices
  },
}))
vi.mock('@/transport/instance', () => ({ getClient: () => ({}) }))

import { NodeGuard } from './NodeGuard'

const A = 'a'.repeat(32)
const daemon = (id: string): ApiDevice => ({ id, name: 'mac-mini', kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] })

beforeEach(() => {
  h.navigate.mockReset()
  h.enabled = []
  h.params = { node: 'aaaaaaaa' }
  h.devices.data = undefined
  h.devices.isPending = false
  h.devices.fetchStatus = 'idle'
  useNodes.setState({ byId: {}, order: [], loaded: true })
})

describe('NodeGuard', () => {
  it('a paired node renders the page (and the account is not even asked)', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
    render(<NodeGuard />)
    expect(screen.getByTestId('outlet')).toBeInTheDocument()
    expect(h.enabled.every((e) => e === false)).toBe(true)
  })

  it('the account\'s node that this browser has not paired with → /unlock/:node', () => {
    h.devices.data = [daemon(A)]
    render(<NodeGuard />)
    expect(h.navigate).toHaveBeenCalledWith(expect.objectContaining({ to: '/unlock/$node', params: { node: 'aaaaaaaa' }, replace: true }))
    expect(screen.queryByTestId('not-found')).toBeNull()
  })

  it('a node that is not on the account (or was revoked) is a 404', () => {
    h.params = { node: 'zzzzzzzz' }
    h.devices.data = [daemon(A)]
    render(<NodeGuard />)
    expect(screen.getByTestId('not-found')).toBeInTheDocument()
    expect(h.navigate).not.toHaveBeenCalled()
  })

  it('waits for the device list before deciding', () => {
    h.devices.isPending = true
    h.devices.fetchStatus = 'fetching'
    render(<NodeGuard />)
    expect(screen.queryByTestId('not-found')).toBeNull()
    expect(document.querySelector('[aria-busy="true"]')).not.toBeNull()
  })

  it('a node the daemon no longer recognises offers "unlock again" next to "pair again"', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, conn: 'noise_failed' }) }, order: [A], loaded: true })
    render(<NodeGuard />)
    screen.getByTestId('reunlock').click()
    expect(h.navigate).toHaveBeenCalledWith({ to: '/unlock/$node', params: { node: 'aaaaaaaa' } })
    screen.getByTestId('repair').click()
    expect(h.navigate).toHaveBeenCalledWith({ to: '/pair' })
  })
})
