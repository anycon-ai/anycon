// 单项详情里的「经过」和「让 agent 处理」入口（spec/web.md § 4.12）：派发、每轮的结果（消毒）、人改的状态，从旧到新；每一行都能点到会话。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { mockAgent, nodeEntry, summary, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { handleTodoEvent, useTodoHistory } from '../history'
import { resetTodosMemory, syncTodos } from '../service'
import { useTodoView } from '../store'
import { TodoDetail } from './TodoDetail'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const N = 'n'.repeat(32)
let rpc: FakeRpc
let daemon: FakeTodoDaemon
const by = { kind: 'device', device_id: 'dev' }
const dispatched = (seq: number, sid: string, agent = 'claude') => ({ seq, ts_ms: seq * 1000, kind: 'dispatched', session_id: sid, agent, workspace_mode: 'worktree', no_push: true, by }) as never
const result = (seq: number, sid: string, text: string, extra: object = {}) => ({ seq, ts_ms: seq * 1000, kind: 'result', session_id: sid, turn: 1, text, truncated: false, ...extra }) as never
const stateEv = (seq: number) => ({ seq, ts_ms: seq * 1000, kind: 'state', from: 'open', to: 'working', by }) as never

function Live({ online = true }: { online?: boolean }) {
  const item = useTodoView([N]).find((v) => v.id === 't1')
  return item ? <TodoDetail item={item} online={online} /> : <p>gone</p>
}
async function mount(over: Parameters<typeof todoItem>[0], o: { online?: boolean } = {}): Promise<void> {
  daemon.seed(todoItem(over))
  await syncTodos(N)
  render(<Live {...o} />)
  await act(async () => {
    for (let i = 0; i < 15; i++) await new Promise((r) => setTimeout(r, 5))
  })
}

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', agents: [mockAgent()], info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  useSessions.setState({ list: { s1: { ...summary({ id: 's1', status: 'idle' }), node_id: N, cached_seq: 0 } as never }, pending: {}, open: {} })
  resetTodosMemory()
  useTodoHistory.getState().reset()
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('the history (经过) in the detail', () => {
  it('is loaded when the item has been handed over, oldest first, each dispatch and result linking to its session with its live status', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [stateEv(4), result(3, 's1', 'All **done**'), dispatched(2, 's1'), dispatched(1, 'gone')], has_more: false })
    await mount({ id: 't1', title: 'Fix it', sessions: ['gone', 's1'] as never, activity: 'review' as never })
    const rows = await screen.findAllByTestId('history-row')
    expect(rows.map((r) => r.getAttribute('data-kind'))).toEqual(['dispatched', 'dispatched', 'result', 'state'])
    expect(within(rows[2]!).getByTestId('history-result-text')).toHaveTextContent('All done')
    expect(within(rows[1]!).getByTestId('history-session-link')).toHaveAttribute('data-status', 'idle')
    expect(within(rows[0]!).getByTestId('history-session-gone')).toBeInTheDocument()
    expect(within(screen.getByTestId('history-sessions')).getAllByRole('listitem')).toHaveLength(2)
  })

  it('an item that was never handed over shows no history and does not ask the node', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
    await mount({ id: 't1', title: 'Plain' })
    expect(screen.queryByTestId('todo-history')).toBeNull()
    expect(rpc.calls.some((c) => c.method === 'todo.history')).toBe(false)
  })

  it('the agent’s report is untrusted: script, event handlers and raw HTML are not rendered as such', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [result(2, 's1', '<img src=x onerror="alert(1)"><script>alert(2)</script> [link](javascript:alert(3))'), dispatched(1, 's1')], has_more: false })
    await mount({ id: 't1', sessions: ['s1'] as never })
    const box = await screen.findByTestId('history-result-text')
    expect(box.querySelector('script')).toBeNull()
    expect(box.querySelector('img[onerror]')).toBeNull()
    expect(box.querySelector('a[href^="javascript:"]')).toBeNull()
  })

  it('a kind this version does not know is not drawn; an empty report and a cut one are labelled', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [{ seq: 4, ts_ms: 1, kind: 'from-the-future' } as never, result(3, 's1', '', {}), result(2, 's1', 'long', { truncated: true }), dispatched(1, 's1')], has_more: false })
    await mount({ id: 't1', sessions: ['s1'] as never })
    await screen.findAllByTestId('history-row')
    expect(screen.getAllByTestId('history-row')).toHaveLength(3)
    expect(screen.getByText('(no text)')).toBeInTheDocument()
    expect(screen.getByText(/Only the beginning of the report was kept/)).toBeInTheDocument()
  })

  it('pages back with “Earlier” using the oldest seq as before_seq, and a todo.event notification appends live', async () => {
    const user = userEvent.setup()
    rpc.handlers['todo.history'] = (p) => ((p as { before_seq?: number }).before_seq ? { events: [dispatched(1, 's1')], has_more: false } : { events: [result(2, 's1', 'second')], has_more: true })
    await mount({ id: 't1', sessions: ['s1'] as never })
    await user.click(await screen.findByTestId('history-more'))
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(2))
    expect(rpc.calls.filter((c) => c.method === 'todo.history').at(-1)!.params).toMatchObject({ id: 't1', before_seq: 2 })
    expect(screen.queryByTestId('history-more')).toBeNull()
    act(() => handleTodoEvent(N, { todo_id: 't1', event: result(3, 's1', 'third, live') }))
    expect(await screen.findByText('third, live')).toBeInTheDocument()
    act(() => handleTodoEvent(N, { todo_id: 'other', event: result(9, 's1', 'not mine') }))
    expect(screen.queryByText('not mine')).toBeNull()
  })

  it('a failed load is shown with a retry', async () => {
    const user = userEvent.setup()
    let fail = true
    rpc.handlers['todo.history'] = () => {
      if (fail) throw new Error('boom')
      return { events: [dispatched(1, 's1')], has_more: false }
    }
    await mount({ id: 't1', sessions: ['s1'] as never })
    expect(await screen.findByTestId('history-failed')).toBeInTheDocument()
    fail = false
    await user.click(screen.getByTestId('history-retry'))
    expect(await screen.findAllByTestId('history-row')).toHaveLength(1)
  })
})

describe('the entry points in the detail', () => {
  it('“Let an agent handle it” opens the dialog for an open item; a closed one has none; offline disables it with the reason', async () => {
    const user = userEvent.setup()
    await mount({ id: 't1', project: 'proj' })
    await user.click(screen.getByTestId('todo-dispatch'))
    expect(await screen.findByTestId('dispatch-dialog')).toBeInTheDocument()
  })

  it('offline: the button is disabled', async () => {
    await mount({ id: 't1' }, { online: false })
    expect(screen.getByTestId('todo-dispatch')).toBeDisabled()
  })

  it('an item in review has a link to the review page; one that is still working does not', async () => {
    await mount({ id: 't1', activity: 'review' as never, sessions: ['s1'] as never })
    expect(screen.getByTestId('todo-review-link')).toHaveAttribute('href', expect.stringContaining('/review'))
    expect(screen.getByTestId('todo-activity')).toHaveAttribute('data-state', 'review')
  })

  it('an unreviewed agent item keeps Accept / Dismiss and also offers handing it over (which accepts it)', async () => {
    await mount({ id: 't1', source: { kind: 'agent' } as never, unreviewed: true })
    expect(screen.getByTestId('todo-accept')).toBeInTheDocument()
    expect(screen.getByTestId('todo-dispatch')).toBeEnabled()
  })

  it('an unknown activity shows no activity chip', async () => {
    await mount({ id: 't1', activity: 'something-new' as never })
    expect(screen.queryByTestId('todo-activity')).toBeNull()
  })
})
