// 节点设置 →「预览」（web.md § 4.8，ADR-39）：preview.list 的票据列表、关闭（preview.close）、端口策略说明。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PreviewCloseParams, PreviewInfo } from '@/protocol'
import { clearTickets } from '@/features/preview/ticket'
import { nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { HOUR_MS, LABEL, previewInfo, PREVIEW_NODE, previewNodeInfo } from '@/test/preview'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { PreviewTab } from './PreviewTab'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = PREVIEW_NODE
let rpc: FakeRpc

function node(conn: 'ready' | 'peer_offline' = 'ready'): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn, info: previewNodeInfo() }) }, order: [NODE], loaded: true })
}
const closeCalls = () => rpc.calls.filter((c) => c.method === 'preview.close').map((c) => c.params as PreviewCloseParams)

beforeEach(() => {
  resetSeq()
  clearTickets()
  rpc = new FakeRpc()
  setRpc(rpc)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
  node()
})
afterEach(() => setRpc(null))

describe('node settings: the Preview tab', () => {
  it('lists each ticket: port, session link, who opened it, when it expires, how many tunnels', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_a', title: 'Fix the login' })])
    rpc.handlers['preview.list'] = () => ({
      previews: [
        previewInfo({ port: 5173, tunnels: 2, created_by_name: 'Safari on iPhone', expires_at_ms: Date.now() + 5 * HOUR_MS }),
        previewInfo({ port: 3000, ticket_id: 'e'.repeat(32), label: 'bcdefghijklmnopqrstuvwxyz2', session_id: null, created_by_name: 'Chrome on Mac', tunnels: 0 }),
      ] satisfies PreviewInfo[],
    })
    render(<PreviewTab nodeId={NODE} />)
    const rows = await screen.findAllByTestId('preview-row')
    expect(rows).toHaveLength(2)
    // 按端口排序
    expect(rows.map((r) => r.getAttribute('data-port'))).toEqual(['3000', '5173'])

    const first = within(rows[0]!)
    expect(first.getByTestId('preview-row-port')).toHaveTextContent('3000')
    expect(first.getByTestId('preview-row-no-session')).toHaveTextContent('No session')
    expect(first.getByTestId('preview-row-device')).toHaveTextContent('Opened by Chrome on Mac')
    expect(first.getByTestId('preview-row-tunnels')).toHaveTextContent('0 tunnels')

    const second = within(rows[1]!)
    expect(second.getByTestId('preview-row-port')).toHaveTextContent('5173')
    expect(second.getByTestId('preview-row-session')).toHaveTextContent('Fix the login')
    expect(second.getByTestId('preview-row-session')).toHaveAttribute('href', `/n/${NODE.slice(0, 8)}/s/sess_a`)
    expect(second.getByTestId('preview-row-device')).toHaveTextContent('Opened by Safari on iPhone')
    expect(second.getByTestId('preview-row-expires')).toHaveTextContent(/Expires/)
    expect(second.getByTestId('preview-row-expires')).toHaveAttribute('title')
    expect(second.getByTestId('preview-row-tunnels')).toHaveTextContent('2 tunnels')
    expect(rpc.calls.find((c) => c.method === 'preview.list')!.params).toEqual({})
  })

  it('explains the port policy, read-only: sessions’ own servers by default, the rest in preview.allow_ports', async () => {
    rpc.handlers['preview.list'] = () => ({ previews: [] })
    render(<PreviewTab nodeId={NODE} />)
    const policy = await screen.findByTestId('preview-policy')
    expect(policy).toHaveTextContent('servers started by the session itself')
    expect(policy).toHaveTextContent('preview.allow_ports')
    expect(within(policy).queryByRole('textbox')).toBeNull()
    expect(within(policy).queryByRole('button')).toBeNull()
  })

  it('says so when nothing is open, and shows the error when the list cannot be loaded', async () => {
    rpc.handlers['preview.list'] = () => ({ previews: [] })
    const a = render(<PreviewTab nodeId={NODE} />)
    expect(await screen.findByTestId('preview-list-empty')).toHaveTextContent('No open previews.')
    a.unmount()
    rpc.handlers['preview.list'] = () => {
      throw rpcError('internal')
    }
    render(<PreviewTab nodeId={NODE} />)
    expect(await screen.findByTestId('preview-list-error')).toHaveTextContent('Something went wrong on the node.')
  })

  it('does not ask an offline node, and its Close buttons are off', async () => {
    node('peer_offline')
    render(<PreviewTab nodeId={NODE} />)
    expect(await screen.findByTestId('preview-list-offline')).toBeInTheDocument()
    expect(rpc.methods()).not.toContain('preview.list')
    expect(screen.getByTestId('preview-list-refresh')).toBeDisabled()
  })

  it('refresh asks the node again', async () => {
    let n = 0
    rpc.handlers['preview.list'] = () => ({ previews: n++ === 0 ? [] : [previewInfo()] })
    const user = userEvent.setup()
    render(<PreviewTab nodeId={NODE} />)
    await screen.findByTestId('preview-list-empty')
    await user.click(screen.getByTestId('preview-list-refresh'))
    expect(await screen.findByTestId('preview-row')).toBeInTheDocument()
  })
})

describe('node settings: closing a preview', () => {
  function serve(list: PreviewInfo[] = [previewInfo()]): void {
    let items = list
    rpc.handlers['preview.list'] = () => ({ previews: items })
    rpc.handlers['preview.close'] = (p) => {
      items = items.filter((x) => x.label !== (p as PreviewCloseParams).label)
      return {}
    }
  }

  it('confirms first (it disconnects every device), then sends the label; every ticket of that label goes away', async () => {
    serve([previewInfo({ ticket_id: '1'.repeat(32) }), previewInfo({ ticket_id: '2'.repeat(32), created_by_name: 'Safari' })])
    const user = userEvent.setup()
    render(<PreviewTab nodeId={NODE} />)
    const rows = await screen.findAllByTestId('preview-row')
    expect(rows).toHaveLength(2)
    await user.click(within(rows[0]!).getByTestId('preview-row-close'))
    const dialog = await screen.findByTestId('preview-close-dialog')
    expect(dialog).toHaveTextContent('Close the preview of port 5173?')
    expect(dialog).toHaveTextContent('all your devices')
    expect(closeCalls()).toEqual([])

    await user.click(screen.getByTestId('preview-close-confirm'))
    await waitFor(() => expect(closeCalls()).toEqual([{ label: LABEL }]))
    await waitFor(() => expect(screen.queryAllByTestId('preview-row')).toHaveLength(0))
    expect(screen.getByTestId('preview-list-empty')).toBeInTheDocument()
    expect(useUi.getState().toasts.map((x) => x.text)).toContain('Stopped the preview of port 5173')
  })

  it('cancelling sends nothing', async () => {
    serve()
    const user = userEvent.setup()
    render(<PreviewTab nodeId={NODE} />)
    await user.click(await screen.findByTestId('preview-row-close'))
    await user.click(within(await screen.findByTestId('preview-close-dialog')).getByRole('button', { name: 'Cancel' }))
    expect(closeCalls()).toEqual([])
    expect(screen.getByTestId('preview-row')).toBeInTheDocument()
  })

  it('a refusal is shown and the row stays', async () => {
    serve()
    rpc.handlers['preview.close'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(<PreviewTab nodeId={NODE} />)
    await user.click(await screen.findByTestId('preview-row-close'))
    await user.click(await screen.findByTestId('preview-close-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.some((x) => x.kind === 'error' && x.text === 'Something went wrong on the node.')).toBe(true))
    expect(screen.getByTestId('preview-row')).toBeInTheDocument()
  })
})
