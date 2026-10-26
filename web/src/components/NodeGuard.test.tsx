// /n/:node 的守卫：本账号的节点、只是这个浏览器还没配对 → /unlock/:node；不是本账号的 → 404（spec/web.md § 2）。
import { cleanup, render, screen } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { mockRouter } from '@/test/routerMock'
import { nodeEntry } from '@/test/factories'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({
  navigate: vi.fn(),
  params: { node: 'aaaaaaaa' } as Record<string, string>,
  devices: { data: undefined as unknown, dataUpdatedAt: 0, isPending: false, fetchStatus: 'idle', isError: false },
  removeNode: vi.fn(async (_id: string) => {}),
  enabled: [] as Array<boolean | undefined>,
  onSettings: false,
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
    useMatchRoute: () => (o: { to: string }) => (o.to === '/n/$node/settings' && h.onSettings ? {} : false),
  }
})
vi.mock('@/api/queries', () => ({
  useDevices: (o?: { enabled?: boolean }) => {
    h.enabled.push(o?.enabled)
    return h.devices
  },
}))
vi.mock('@/transport/instance', () => ({ getClient: () => ({ removeNode: h.removeNode }) }))

import { NodeGuard } from './NodeGuard'

const A = 'a'.repeat(32)
const daemon = (id: string): ApiDevice => ({ id, name: 'mac-mini', kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] })

beforeEach(() => {
  h.navigate.mockReset()
  h.enabled = []
  h.params = { node: 'aaaaaaaa' }
  h.devices.data = undefined
  h.devices.dataUpdatedAt = 0
  h.removeNode.mockClear()
  h.devices.isPending = false
  h.devices.fetchStatus = 'idle'
  h.onSettings = false
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

  it('a paired node the account no longer has (removed on another device) gets a page offering to forget it', async () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, pairedAt: 100 }) }, order: [A], loaded: true })
    h.devices.data = [daemon('b'.repeat(32))]
    h.devices.dataUpdatedAt = 200
    render(<NodeGuard />)
    expect(screen.getByTestId('node-removed')).toBeInTheDocument()
    expect(screen.queryByTestId('outlet')).toBeNull()
    screen.getByTestId('node-removed-forget').click()
    await vi.waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/' }))
    expect(h.removeNode).toHaveBeenCalledWith(A)
    expect(useNodes.getState().order).toEqual([])
  })

  it('a device list fetched before the pairing does not make a node look removed', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, pairedAt: 300 }) }, order: [A], loaded: true })
    h.devices.data = []
    h.devices.dataUpdatedAt = 200
    render(<NodeGuard />)
    expect(screen.getByTestId('outlet')).toBeInTheDocument()
  })

  it('a node the daemon no longer recognises offers "unlock again" next to "pair again"', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, conn: 'noise_failed' }) }, order: [A], loaded: true })
    render(<NodeGuard />)
    screen.getByTestId('reunlock').click()
    expect(h.navigate).toHaveBeenCalledWith({ to: '/unlock/$node', params: { node: 'aaaaaaaa' } })
    screen.getByTestId('repair').click()
    expect(h.navigate).toHaveBeenCalledWith({ to: '/pair' })
  })

  it('incompatible versions and an unrecognised device do not block node settings, where the node can be removed from the account', () => {
    for (const patch of [{ versionMismatch: true, conn: 'peer_offline' as const }, { conn: 'noise_failed' as const }]) {
      useNodes.setState({ byId: { [A]: nodeEntry({ id: A, ...patch }) }, order: [A], loaded: true })
      h.onSettings = false
      const { unmount } = render(<NodeGuard />)
      expect(screen.queryByTestId('outlet')).toBeNull()
      screen.getByTestId('guard-node-settings').click()
      expect(h.navigate).toHaveBeenCalledWith({ to: '/n/$node/settings', params: { node: 'aaaaaaaa' } })
      unmount()
      h.onSettings = true
      render(<NodeGuard />)
      expect(screen.getByTestId('outlet')).toBeInTheDocument()
      cleanup()
    }
  })
})
