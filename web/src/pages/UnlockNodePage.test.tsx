// /unlock/:node（spec/web.md § 4.3.1）：解析节点、在线状态、已经配对的不用解锁、hub 说不是你的 → 404。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { FakeEnrollClient } from '@/test/fakeEnrollClient'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { nodeEntry } from '@/test/factories'
import { EnrollError } from '@/transport/enroll'

const h = vi.hoisted(() => ({
  navigate: vi.fn(),
  client: null as unknown as FakeEnrollClient,
  devices: { data: undefined as unknown, isPending: false, isError: false, refetch: vi.fn() },
}))
vi.mock('@tanstack/react-router', async (orig) => ({
  ...mockRouter(await orig<object>(), h.navigate, {}),
  Navigate: (props: unknown) => {
    h.navigate(props)
    return null
  },
  useRouter: () => ({ history: { back: vi.fn() } }),
}))
vi.mock('@/api/queries', () => ({ useDevices: () => h.devices }))
vi.mock('@/transport/instance', () => ({ getClient: () => h.client }))
vi.mock('@/ui/QrCode', () => ({ QrCode: ({ text }: { text: string }) => <div data-testid="qr" data-text={text} /> }))

import { UnlockNodePage } from './UnlockNodePage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const dev = (id: string, over: Partial<ApiDevice> = {}): ApiDevice => ({ id, name: id === A ? 'mac-mini' : 'nas', kind: 'daemon', platform: 'linux-x86_64', created_at_ms: 1, online: true, paired_with: [], ...over })

beforeEach(() => {
  h.navigate.mockReset()
  h.client = new FakeEnrollClient()
  h.devices.data = [dev(A), dev(B)]
  h.devices.isPending = false
  h.devices.isError = false
  useNodes.setState({ byId: {}, order: [], loaded: true })
})

describe('UnlockNodePage', () => {
  it('titles the page with the node name (from the hub, unauthenticated) and offers the four entries', async () => {
    render(<UnlockNodePage node="aaaaaaaa" />)
    expect(screen.getByTestId('unlock-title')).toHaveTextContent('mac-mini')
    for (const e of ['scan', 'approve', 'code', 'password']) expect(screen.getByTestId(`unlock-trigger-${e}`)).toBeEnabled()
    // 打开页面不开始接入；点「显示二维码」才开始
    await new Promise((r) => setTimeout(r, 40))
    expect((h.client as FakeEnrollClient).calls).toHaveLength(0)
    await userEvent.setup().click(screen.getByTestId('unlock-show-qr'))
    await waitFor(() => expect((h.client as FakeEnrollClient).calls).toHaveLength(1))
    expect((h.client as FakeEnrollClient).last.req.node).toBe(A) // 短 id 解析回完整 id
  })

  it('accepts the full id as well', () => {
    render(<UnlockNodePage node={B} />)
    expect(screen.getByTestId('unlock-title')).toHaveTextContent('nas')
  })

  it('an offline node disables every entry and says so', () => {
    h.devices.data = [dev(A, { online: false })]
    render(<UnlockNodePage node="aaaaaaaa" />)
    expect(screen.getByTestId('unlock-offline')).toBeInTheDocument()
    expect(screen.getByTestId('unlock-trigger-password')).toBeDisabled()
  })

  it('a node that is not on the account is a 404', () => {
    render(<UnlockNodePage node="cccccccc" />)
    expect(screen.getByTestId('not-found')).toBeInTheDocument()
    expect(screen.queryByTestId('unlock-panel')).toBeNull()
  })

  it('an ambiguous short id is not guessed', () => {
    h.devices.data = [dev('abcd1234' + '0'.repeat(24)), dev('abcd1234' + '1'.repeat(24))]
    render(<UnlockNodePage node="abcd1234" />)
    expect(screen.getByTestId('not-found')).toBeInTheDocument()
  })

  it('the hub saying "not found" (not your node) also ends in the 404 page', async () => {
    render(<UnlockNodePage node="aaaaaaaa" />)
    const client = h.client as FakeEnrollClient
    await userEvent.setup().click(screen.getByTestId('unlock-show-qr'))
    await waitFor(() => expect(client.calls).toHaveLength(1))
    act(() => client.last.reject(new EnrollError('not_found')))
    expect(await screen.findByTestId('not-found')).toBeInTheDocument()
  })

  it('shows a spinner while the device list is loading and an error with retry when it cannot be loaded', async () => {
    h.devices.data = undefined
    h.devices.isPending = true
    const { rerender } = render(<UnlockNodePage node="aaaaaaaa" />)
    expect(document.querySelector('[aria-busy="true"]')).not.toBeNull()
    h.devices.isPending = false
    h.devices.isError = true
    rerender(<UnlockNodePage node="aaaaaaaa" />)
    expect(screen.getByTestId('unlock-load-failed')).toBeInTheDocument()
  })

  it('a node this browser is already paired with (and can reach) needs no unlock: go to it', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, conn: 'ready' }) }, order: [A], loaded: true })
    render(<UnlockNodePage node="aaaaaaaa" />)
    expect(h.navigate).toHaveBeenCalledWith(expect.objectContaining({ to: '/n/$node', params: { node: 'aaaaaaaa' } }))
    expect(screen.queryByTestId('unlock-panel')).toBeNull()
  })

  it('...but one the daemon no longer recognises (noise_failed) can be unlocked again', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, conn: 'noise_failed' }) }, order: [A], loaded: true })
    render(<UnlockNodePage node="aaaaaaaa" />)
    expect(screen.getByTestId('unlock-panel')).toBeInTheDocument()
  })

  it('when the node is unlocked it goes to the node page', async () => {
    render(<UnlockNodePage node="aaaaaaaa" />)
    const client = h.client as FakeEnrollClient
    await userEvent.setup().click(screen.getByTestId('unlock-show-qr'))
    await waitFor(() => expect(client.calls).toHaveLength(1))
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
    act(() => client.last.resolve(A))
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'aaaaaaaa' }, replace: true }))
  })

  it('offers the short pairing code as the way out', () => {
    render(<UnlockNodePage node="aaaaaaaa" />)
    screen.getByTestId('unlock-use-code').click()
    expect(h.navigate).toHaveBeenCalledWith({ to: '/pair' })
  })
})
