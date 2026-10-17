// 关联项目的目录在 turn 进行中被换掉时，`permission.respond` 返回 `not_found`（docs/protocol.md § 5.5）：
// 请求已被 daemon 消耗并按拒绝答复；随后是 permission_resolved（by_device: "daemon"）、error { not_found }、turn_done { error }。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, PermissionRequest } from '@/protocol'
import { handleNotification } from '@/features/sync/notifications'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
// 不真的去 daemon 拉会话：事件直接放进 store
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

const request: PermissionRequest = {
  session_id: SID,
  request_id: 'r1',
  kind: 'tool',
  title: 'Run command',
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'ls' }, detail: { command: 'ls' } } },
  options: [
    { id: 'allow', label: 'Allow', kind: 'allow_once' },
    { id: 'deny', label: 'Deny', kind: 'reject_once' },
  ],
  created_at_ms: 1,
}

function seed(): void {
  const events = [
    ev({ type: 'user_message', message_id: 'm1', content: 'list files', by_device: 'me' }),
    ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }),
    ev({ type: 'permission', request_id: 'r1', kind: 'tool', title: request.title, detail: request.detail, options: request.options }),
  ]
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true, })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, status: 'waiting_permission', pending_permissions: 1 }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events)
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
  useSessions.getState().addPending(NODE, request)
}

const push = (...events: Event[]): void => {
  act(() => {
    useSessions.getState().ingest(SID, events.map((e) => ev(e)))
  })
}

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  seed()
})
afterEach(() => setRpc(null))

describe('SessionPage: permission.respond → not_found (the project folder changed during the turn)', () => {
  it('does not retry or show an error: the card turns into “denied because the project folder changed”, with no buttons left, and the pending request is cleared', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found', undefined, 'project_dir_changed')
    }
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('permission-option-allow'))

    const card = await screen.findByTestId('permission-settled')
    expect(card).toHaveTextContent('This request was denied because the project folder changed, and the turn was stopped.')
    const settled = screen.getByTestId('permission-card')
    expect(settled).toHaveAttribute('data-state', 'settled')
    expect(screen.queryByTestId('permission-option-allow')).toBeNull()
    expect(screen.queryByTestId('permission-option-deny')).toBeNull()
    // 只发了一次，没有重试
    expect(rpc.calls.filter((c) => c.method === 'permission.respond')).toHaveLength(1)
    // 不是错误：没有 error toast，只有一条说明
    const toasts = useUi.getState().toasts
    expect(toasts.filter((t) => t.kind === 'error')).toEqual([])
    expect(toasts.map((t) => t.text)).toContain('This request was denied because the project folder changed, and the turn was stopped.')
    expect(useSessions.getState().pending).toEqual({})
  })

  it('then shows the daemon’s own answer as “Denied automatically by anycon”, not as a device called “daemon”, and the turn ending in an error', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found', undefined, 'project_dir_changed')
    }
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('permission-option-allow'))
    await screen.findByTestId('permission-settled')

    push({ type: 'permission_resolved', request_id: 'r1', option_id: 'deny', by_device: 'daemon' }, { type: 'error', code: 'not_found', message: 'the project directory changed', recoverable: false }, { type: 'turn_done', turn_id: 1, reason: 'error' })
    handleNotification(NODE, 'permission.resolved', { session_id: SID, request_id: 'r1', option_id: 'deny', by_device: 'daemon' })

    const card = await screen.findByTestId('permission-card')
    await waitFor(() => expect(card).toHaveAttribute('data-state', 'resolved'))
    expect(card).toHaveAttribute('data-by', 'daemon')
    expect(card).toHaveTextContent('Denied automatically by anycon')
    expect(card.textContent).not.toMatch(/daemon/i)
    expect(screen.queryByTestId('permission-settled')).toBeNull()
    expect(screen.getByTestId('error-block')).toHaveTextContent('the project directory changed')
    expect(rpc.methods().filter((m) => m === 'node.devices')).toEqual([]) // 没去查名叫 "daemon" 的设备
  })

  it('any other not_found (no data.reason: unknown request id, session not found) only says the request no longer exists — not that the folder changed or the turn was stopped — and does not retry', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found')
    }
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('permission-option-allow'))

    const note = await screen.findByTestId('permission-settled')
    expect(note).toHaveTextContent('This request no longer exists, so it can’t be answered.')
    const card = screen.getByTestId('permission-card')
    expect(card).toHaveAttribute('data-state', 'settled')
    expect(card).toHaveAttribute('data-settled', 'gone')
    expect(card.textContent).not.toMatch(/folder|turn was stopped/i)
    expect(screen.queryByTestId('permission-option-allow')).toBeNull()
    expect(screen.queryByTestId('permission-option-deny')).toBeNull()
    expect(rpc.calls.filter((c) => c.method === 'permission.respond')).toHaveLength(1)
    const toasts = useUi.getState().toasts
    expect(toasts.filter((t) => t.kind === 'error')).toEqual([])
    expect(toasts.map((t) => t.text)).toEqual(['This request no longer exists, so it can’t be answered.'])
    expect(useSessions.getState().pending).toEqual({})
  })

  it('still reports any other failure as an error and leaves the card answerable', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('permission-option-allow'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(screen.queryByTestId('permission-settled')).toBeNull()
    expect(await screen.findByTestId('permission-option-allow')).toBeEnabled()
  })
})
