// 节点设置 →「关于」→「查看日志」（web.md § 4.8）：node.logs 的尾部，按级别过滤、选行数、刷新、复制；节点离线时按钮禁用。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeLogsParams, NodeLogsResult } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'

const h = vi.hoisted(() => ({ navigate: vi.fn(), copy: vi.fn(async (_text: string) => {}) }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, { node: 'nnnnnnnn' }))
vi.mock('@/lib/clipboard', () => ({ copyText: h.copy }))

import { AboutTab } from './LimitsAndAbout'
import { LogsDialog } from './LogsDialog'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const LINES = [
  '2026-10-30T10:00:00.000000Z  INFO anycon_daemon::daemon: daemon started',
  '2026-10-30T10:00:01.000000Z  WARN anycon_daemon::peers: peer stalled peer=abc',
  '2026-10-30T10:00:02.000000Z ERROR anycon_daemon::store: cannot write error=disk full',
]
const logCalls = () => rpc.calls.filter((c) => c.method === 'node.logs').map((c) => c.params as NodeLogsParams)

function serve(result: NodeLogsResult = { lines: LINES, truncated: false }): void {
  rpc.handlers['node.logs'] = () => result
}

function node(conn: 'ready' | 'peer_offline' = 'ready'): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn }) }, order: [NODE], loaded: true })
}

beforeEach(() => {
  rpc = new FakeRpc()
  rpc.handlers['node.stats'] = () => ({ uptime_ms: 1, hub_connected: true, peers_online: 1, sessions: {}, agent_processes: 0, db_bytes: 1, disk_free_bytes: 1 })
  setRpc(rpc)
  h.copy.mockClear()
  useUi.setState({ toasts: [] })
  node()
  serve()
})
afterEach(() => setRpc(null))

describe('node logs dialog', () => {
  it('asks for the latest lines at info and above, and shows them oldest first', async () => {
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    const out = await screen.findByTestId('logs-output')
    expect(out.textContent).toBe(LINES.join('\n'))
    expect(logCalls()).toEqual([{ lines: 500, level: 'info' }])
    expect(screen.getByTestId('logs-count')).toHaveTextContent('Showing the latest 3 lines.')
    expect(screen.getByTestId('logs-count')).not.toHaveTextContent('Older lines')
  })

  it('says when older lines are not shown', async () => {
    serve({ lines: LINES, truncated: true })
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    expect(await screen.findByTestId('logs-count')).toHaveTextContent('Older lines are not shown.')
  })

  it('filters by level: changing the level asks again', async () => {
    const user = userEvent.setup()
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    await screen.findByTestId('logs-output')
    await user.selectOptions(screen.getByTestId('logs-level'), 'error')
    await waitFor(() => expect(logCalls()).toHaveLength(2))
    expect(logCalls()[1]).toEqual({ lines: 500, level: 'error' })
    await user.selectOptions(screen.getByTestId('logs-level'), 'trace')
    await waitFor(() => expect(logCalls()).toHaveLength(3))
    expect(logCalls()[2]).toEqual({ lines: 500, level: 'trace' })
  })

  it('the number of lines can be changed', async () => {
    const user = userEvent.setup()
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    await screen.findByTestId('logs-output')
    await user.selectOptions(screen.getByTestId('logs-lines'), '1000')
    await waitFor(() => expect(logCalls()).toHaveLength(2))
    expect(logCalls()[1]).toEqual({ lines: 1000, level: 'info' })
  })

  it('refresh reads the node again and shows the new lines', async () => {
    const user = userEvent.setup()
    let n = 0
    rpc.handlers['node.logs'] = () => ({ lines: n++ === 0 ? [LINES[0]!] : LINES, truncated: false })
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    await waitFor(() => expect(screen.getByTestId('logs-output').textContent).toBe(LINES[0]))
    await user.click(screen.getByTestId('logs-refresh'))
    await waitFor(() => expect(screen.getByTestId('logs-output').textContent).toBe(LINES.join('\n')))
    expect(logCalls()).toHaveLength(2)
  })

  it('copies the lines that are shown', async () => {
    const user = userEvent.setup()
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    await screen.findByTestId('logs-output')
    await user.click(screen.getByTestId('logs-copy'))
    expect(h.copy).toHaveBeenCalledWith(LINES.join('\n'))
  })

  it('says so when there is nothing at this level, and offers no copy', async () => {
    serve({ lines: [], truncated: false })
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    expect(await screen.findByTestId('logs-empty')).toHaveTextContent('No log lines at this level.')
    expect(screen.queryByTestId('logs-copy')).toBeNull()
    expect(screen.queryByTestId('logs-output')).toBeNull()
  })

  it('shows the error when the logs cannot be read', async () => {
    rpc.handlers['node.logs'] = () => {
      throw rpcError('internal')
    }
    render(<LogsDialog nodeId={NODE} open onOpenChange={() => {}} />)
    expect(await screen.findByTestId('logs-error')).toHaveTextContent('Something went wrong on the node.')
  })

  it('does not ask while it is closed', () => {
    render(<LogsDialog nodeId={NODE} open={false} onOpenChange={() => {}} />)
    expect(rpc.methods()).toEqual([])
  })
})

describe('node settings: View logs in the About tab', () => {
  const mount = () =>
    render(
      <QueryClientProvider client={new QueryClient()}>
        <AboutTab nodeId={NODE} />
      </QueryClientProvider>,
    )

  it('opens the logs dialog', async () => {
    const user = userEvent.setup()
    mount()
    expect(logCalls()).toEqual([])
    await user.click(screen.getByTestId('logs-open'))
    expect(await screen.findByTestId('logs-dialog')).toBeInTheDocument()
    expect((await screen.findByTestId('logs-output')).textContent).toBe(LINES.join('\n'))
  })

  it('is disabled while the node is offline', () => {
    node('peer_offline')
    mount()
    expect(screen.getByTestId('logs-open')).toBeDisabled()
    expect(rpc.methods()).toEqual([])
  })
})
