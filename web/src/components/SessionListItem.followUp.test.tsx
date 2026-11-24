// 会话列表行上的「待处理」和未读（web.md § 4.6.1）：chip、未读小红点、右边的开关。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { SessionListItem } from './SessionListItem'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const item = (over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ id: 'a', title: 'Analyse jira', ...over }), node_id: NODE, cached_seq: 0 })
const seed = (over: Parameters<typeof summary>[0]): SessionItem => {
  const it = item(over)
  useSessions.setState({ list: { a: it }, pending: {}, open: {} })
  return it
}

beforeEach(async () => {
  resetSeq()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  rpc.handlers['session.set_marks'] = () => ({})
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
})
afterEach(() => setRpc(null))

describe('SessionListItem: follow-up and unread', () => {
  it('a plain session has no chip and no dot, and offers an unmarked flag', () => {
    render(<SessionListItem session={seed({})} />)
    expect(screen.queryByTestId('follow-up-chip-row')).toBeNull()
    expect(screen.queryByTestId('unread-dot')).toBeNull()
    expect(screen.getByTestId('session-follow-up-toggle')).toHaveAttribute('aria-pressed', 'false')
  })

  it('a marked session reads “Notify when done” whatever its status, with the same look', () => {
    const { rerender } = render(<SessionListItem session={seed({ follow_up: true, status: 'idle' })} />)
    expect(screen.getByTestId('follow-up-chip-row')).toHaveTextContent('Notify when done')
    expect(screen.getByTestId('session-follow-up-toggle')).toHaveAttribute('aria-pressed', 'true')
    const idleClass = screen.getByTestId('follow-up-chip-row').className
    rerender(<SessionListItem session={item({ follow_up: true, status: 'running' })} />)
    expect(screen.getByTestId('follow-up-chip-row')).toHaveTextContent('Notify when done')
    expect(screen.getByTestId('follow-up-chip-row').className).toBe(idleClass)
  })

  it('shows the unread dot only once the session stopped with output past the read watermark', () => {
    const { rerender } = render(<SessionListItem session={seed({ status: 'idle', last_seq: 9, read_seq: 4 })} />)
    expect(screen.getByTestId('unread-dot')).toHaveAccessibleName('Unread output')
    rerender(<SessionListItem session={item({ status: 'running', last_seq: 9, read_seq: 4 })} />)
    expect(screen.queryByTestId('unread-dot')).toBeNull()
    rerender(<SessionListItem session={item({ status: 'idle', last_seq: 9, read_seq: 9 })} />)
    expect(screen.queryByTestId('unread-dot')).toBeNull()
  })

  it('the flag marks the session through session.set_marks and flips right away', async () => {
    const user = userEvent.setup()
    const it = seed({})
    const { rerender } = render(<SessionListItem session={it} />)
    await user.click(screen.getByTestId('session-follow-up-toggle'))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'session.set_marks').map((c) => c.params)).toEqual([{ session_id: 'a', follow_up: true }]))
    expect(useSessions.getState().list['a']!.follow_up).toBe(true)
    rerender(<SessionListItem session={useSessions.getState().list['a']!} />)
    await user.click(screen.getByTestId('session-follow-up-toggle'))
    await waitFor(() => expect(useSessions.getState().list['a']!.follow_up).toBe(false))
  })

  it('puts the mark back and shows the error when the node refuses', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.set_marks'] = () => {
      throw rpcError('not_found')
    }
    render(<SessionListItem session={seed({})} />)
    await user.click(screen.getByTestId('session-follow-up-toggle'))
    await waitFor(() => expect(useUi.getState().toasts).toHaveLength(1))
    expect(useSessions.getState().list['a']!.follow_up).toBeFalsy()
  })

  it('the flag is disabled while the node is offline', () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], conn: 'peer_offline' }) } })
    render(<SessionListItem session={seed({})} />)
    expect(screen.getByTestId('session-follow-up-toggle')).toBeDisabled()
  })
})
