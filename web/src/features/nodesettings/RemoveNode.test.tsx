// 节点设置「关于」里的「移除这台节点」：机器重装了、旧 daemon 再也不会上线时，离线也能从账号里移除并在这个浏览器里忘记它。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const h = vi.hoisted(() => ({
  navigate: vi.fn(),
  revokeDevice: vi.fn(async (_id: string, _nodes: string[]) => ({ offlineNodes: [] as string[] })),
  removeNode: vi.fn(async (_id: string) => {}),
}))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, { node: 'nnnnnnnn' }))
vi.mock('@/features/devices/revoke', () => ({ revokeDevice: h.revokeDevice }))
vi.mock('@/transport/instance', () => ({ getClient: () => ({ removeNode: h.removeNode }), rpc: () => ({}) }))

import { AboutTab } from './LimitsAndAbout'

const NODE = 'n'.repeat(32)
const OTHER = 'o'.repeat(32)

function mount() {
  return render(
    <QueryClientProvider client={new QueryClient()}>
      <AboutTab nodeId={NODE} />
    </QueryClientProvider>,
  )
}

beforeEach(() => {
  h.navigate.mockReset()
  h.revokeDevice.mockReset()
  h.revokeDevice.mockResolvedValue({ offlineNodes: [] })
  h.removeNode.mockClear()
  useUi.setState({ toasts: [] })
  // 节点离线（重装后旧的 daemon 不会再上线）
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'old-mini', conn: 'peer_offline' }), [OTHER]: nodeEntry({ id: OTHER }) }, order: [NODE, OTHER], loaded: true })
  useSessions.setState({
    list: {
      a: { ...summary({ id: 'a' }), node_id: NODE, cached_seq: 0 },
      b: { ...summary({ id: 'b' }), node_id: OTHER, cached_seq: 0 },
    },
    pending: {},
    open: {},
  })
})

describe('remove this node', () => {
  it('works while the node is offline: the hub revokes it, this browser forgets it and its cached sessions', async () => {
    const user = userEvent.setup()
    mount()
    await user.click(screen.getByTestId('node-remove-open'))
    expect(screen.getByRole('dialog')).toHaveTextContent('not deleted')
    await user.click(screen.getByTestId('node-remove-confirm'))
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/' }))
    // hub 那一步只吊销节点本身，不去问任何节点
    expect(h.revokeDevice).toHaveBeenCalledWith(NODE, [])
    expect(h.removeNode).toHaveBeenCalledWith(NODE)
    expect(useNodes.getState().order).toEqual([OTHER])
    expect(Object.keys(useSessions.getState().list)).toEqual(['b'])
  })

  it('keeps everything and says so when the hub step fails, so it can be retried', async () => {
    const user = userEvent.setup()
    h.revokeDevice.mockRejectedValue(new Error('boom'))
    mount()
    await user.click(screen.getByTestId('node-remove-open'))
    await user.click(screen.getByTestId('node-remove-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(h.removeNode).not.toHaveBeenCalled()
    expect(useNodes.getState().order).toEqual([NODE, OTHER])
    expect(screen.getByTestId('node-remove-confirm')).toBeInTheDocument()
    expect(h.navigate).not.toHaveBeenCalled()
  })
})
