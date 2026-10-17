// /unlock（spec/web.md § 4.3.1）：账号下、这个浏览器还没配对的节点。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { mockRouter } from '@/test/routerMock'
import { nodeEntry } from '@/test/factories'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({ navigate: vi.fn(), devices: { data: undefined as unknown, isPending: false, isError: false, refetch: vi.fn() } }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}))
vi.mock('@/api/queries', () => ({ useDevices: () => h.devices }))

import { UnlockPage } from './UnlockPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const dev = (id: string, over: Partial<ApiDevice> = {}): ApiDevice => ({ id, name: id === A ? 'mac-mini' : 'nas', kind: 'daemon', platform: 'linux-x86_64', created_at_ms: 1, online: true, paired_with: [], ...over })

beforeEach(() => {
  h.navigate.mockReset()
  h.devices.data = [dev(A), dev(B)]
  h.devices.isPending = false
  h.devices.isError = false
  useNodes.setState({ byId: {}, order: [], loaded: true })
})

describe('UnlockPage', () => {
  it('lists the account nodes this browser has no pairing with (name, platform, online state), and opens one', async () => {
    h.devices.data = [dev(A), dev(B, { online: false, last_seen_ms: Date.now() - 3600_000 }), dev('w'.repeat(32), { kind: 'web' })]
    const user = userEvent.setup()
    render(<UnlockPage />)
    const rows = screen.getAllByTestId('unlock-node-row')
    expect(rows.map((r) => r.getAttribute('data-node'))).toEqual([A, B])
    expect(rows[0]).toHaveTextContent('mac-mini')
    expect(rows[0]).toHaveTextContent(/online|在线/)
    expect(rows[1]).toHaveTextContent(/ago|前|Offline|离线/)
    await user.click(rows[1]!)
    expect(h.navigate).toHaveBeenCalledWith({ to: '/unlock/$node', params: { node: 'bbbbbbbb' } })
  })

  it('does not list a node this browser is already paired with', () => {
    h.devices.data = [dev(A), dev(B), dev('c'.repeat(32), { name: 'third' })]
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
    render(<UnlockPage />)
    expect(screen.getAllByTestId('unlock-node-row').map((r) => r.getAttribute('data-node'))).toEqual([B, 'c'.repeat(32)])
  })

  it('with only one such node it goes straight to its unlock page', async () => {
    h.devices.data = [dev(A)]
    render(<UnlockPage />)
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/unlock/$node', params: { node: 'aaaaaaaa' }, replace: true }))
  })

  it('no node on the account → the setup wizard; all of them already paired → home', async () => {
    h.devices.data = [dev('w'.repeat(32), { kind: 'web' })]
    const { unmount } = render(<UnlockPage />)
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/setup', replace: true }))
    unmount()
    h.navigate.mockReset()
    h.devices.data = [dev(A)]
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
    render(<UnlockPage />)
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/', replace: true }))
  })

  it('offers pairing with a short code at the bottom', async () => {
    const user = userEvent.setup()
    render(<UnlockPage />)
    await user.click(screen.getByTestId('unlock-use-code'))
    expect(h.navigate).toHaveBeenCalledWith({ to: '/pair' })
  })

  it('says so when the hub cannot be asked', () => {
    h.devices.data = undefined
    h.devices.isError = true
    render(<UnlockPage />)
    expect(screen.getByTestId('unlock-load-failed')).toBeInTheDocument()
  })
})
