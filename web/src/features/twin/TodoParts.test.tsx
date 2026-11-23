// 处理人、经过、回答和验收理由均用协议数据与假节点验证。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo, TodoEvent } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { FakeTwinDaemon, twinQuestion } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useTodoView } from '@/features/todos/store'
import { resetTodosMemory, syncTodos } from '@/features/todos/service'
import { handlerOf } from '@/features/todos/activity'
import { groupTodos } from '@/features/todos/list'
import { CaptureForm } from '@/features/todos/ui/CaptureForm'
import { useCapture } from '@/features/todos/capture'
import { HandlerChip } from '@/features/todos/ui/ActivityBits'
import { historyKey, useTodoHistory } from '@/features/todos/history'
import { TodoHistory } from '@/features/todos/ui/TodoHistory'
import { TodoDetail } from '@/features/todos/ui/TodoDetail'
import { FollowUpRuleHost } from '@/features/rules/ui/AlwaysDoThis'
import { useFollowUp } from '@/features/rules/followUp'
import { TwinHandlerButton, TwinReviewReason } from './TodoParts'
import { useTwin } from './store'
import { syncTwin } from './service'
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn()))
const A = 'a'.repeat(32)
const info = { capabilities: { twin: true, todo: true } } as unknown as NodeInfo
let rpc: FakeRpc
let daemon: FakeTodoDaemon
let twin: FakeTwinDaemon
beforeEach(async () => {
  await db.resetDb()
  resetTodosMemory()
  useTwin.getState().reset()
  useTodoHistory.getState().reset()
  localStorage.removeItem('anycon.todoHandler')
  usePrefs.setState({ sidebarNodes: null, sidebarSpace: null })
  useCapture.setState({ text: '', open: false })
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  twin = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info }) }, order: [A], loaded: true })
})
const ids = [A]
function LiveHandler({ detail = false }: { detail?: boolean }) {
  const item = useTodoView(ids)[0]
  return item ? detail ? <TodoDetail item={item} online /> : <TwinHandlerButton item={item} /> : null
}
describe('Twin in to-dos', () => {
  it('gives a reviewed item to Twin and takes it back', async () => {
    const id = '11111111-2222-4333-8444-555555555555'
    daemon.seed(todoItem({ id }))
    await syncTodos(A)
    render(<LiveHandler />)
    await userEvent.click(screen.getByRole('button', { name: 'Give to Twin' }))
    await waitFor(() => expect(daemon.todos.get(id)?.handler).toBe('twin'))
    const params = rpc.calls.find((c) => c.method === 'todo.update')?.params
    expect(params).toEqual({ id, handler: 'twin' })
    await userEvent.click(await screen.findByRole('button', { name: 'I’ll do it' }))
    await waitFor(() => expect(daemon.todos.get(id)?.handler).toBe('me'))
  })
  it('requires accepting an agent item before offering handover, as two separate actions', async () => {
    const id = '11111111-2222-4333-8444-555555555555'
    daemon.seed(todoItem({ id, handler: 'me', unreviewed: true, source: { kind: 'agent' }, body: 'Read these requirements before accepting.' }))
    rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
    await syncTodos(A)
    render(<LiveHandler detail />)
    expect(screen.getByText('Read these requirements before accepting.')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Give to Twin' })).not.toBeInTheDocument()
    expect(rpc.calls.filter((c) => c.method === 'todo.update')).toHaveLength(0)
    await userEvent.click(screen.getByTestId('todo-accept'))
    const handover = await screen.findByRole('button', { name: 'Give to Twin' })
    await waitFor(() => expect(daemon.todos.get(id)?.unreviewed).toBe(false))
    expect(rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)).toEqual([{ id, reviewed: true }])
    expect(daemon.todos.get(id)?.handler).toBe('me')
    await userEvent.click(handover)
    await waitFor(() => expect(daemon.todos.get(id)?.handler).toBe('twin'))
    expect(rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)).toEqual([{ id, reviewed: true }, { id, handler: 'twin' }])
  })
  it('shows Twin even before dispatch, and handler filters select its items', () => {
    const todo = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
    render(<HandlerChip item={todo} />)
    expect(screen.getByTestId('todo-handler')).toHaveTextContent('Twin')
    expect(groupTodos([todo], { source: 'all', project: null, handler: 'twin' }).open).toHaveLength(1)
    expect(groupTodos([todo], { source: 'all', project: null, handler: 'me' }).open).toHaveLength(0)
    expect(handlerOf({ ...todo, handler: 'future' as never })).toBe('me')
    expect(handlerOf({ ...todo, activity: 'working', handler: 'future' as never })).toBe('me')
  })
  it('records with a remembered handler and hides the choice on an unsupported node', async () => {
    localStorage.setItem('anycon.todoHandler', 'twin')
    useCapture.setState({ text: 'Build a thing' })
    const { unmount } = render(<CaptureForm autoFocus={false} />)
    expect(screen.getByTestId('capture-handler')).toHaveValue('twin')
    await userEvent.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect(rpc.calls.some((c) => c.method === 'todo.add')).toBe(true))
    expect(rpc.calls.find((c) => c.method === 'todo.add')?.params).toMatchObject({ handler: 'twin' })
    await userEvent.selectOptions(screen.getByTestId('capture-handler'), 'me')
    expect(localStorage.getItem('anycon.todoHandler')).toBe('me')
    unmount()
    useNodes.getState().patch(A, { info: { ...info, capabilities: { ...info.capabilities, twin: false } } })
    render(<CaptureForm autoFocus={false} />)
    expect(screen.queryByTestId('capture-handler')).not.toBeInTheDocument()
  })
  it('renders all new history kinds, links to a run and answers model text inline', async () => {
    twin.questions = [twinQuestion()]
    await syncTwin(A)
    const events: TodoEvent[] = [
      { seq: 5, ts_ms: 5, kind: 'twin_question', question_id: 'q1' },
      { seq: 4, ts_ms: 4, kind: 'handler', by: { kind: 'twin', id: 'local' }, from: 'me', to: 'twin' },
      { seq: 3, ts_ms: 3, kind: 'taken_over', by: { kind: 'device' } },
      { seq: 2, ts_ms: 2, kind: 'handed_back', reason: 'No capacity', run_id: 'r1' },
      { seq: 1, ts_ms: 1, kind: 'twin_decision', reason: 'Safe to dispatch', run_id: 'r1', action: 'dispatch' },
    ]
    rpc.handlers['todo.history'] = () => ({ events, has_more: false })
    const item = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
    render(<TodoHistory item={item} />)
    expect(await screen.findByText('You took over')).toBeInTheDocument()
    expect(screen.getByText('No capacity')).toBeInTheDocument()
    expect(screen.getByText('Handler changed: Me → Twin')).toBeInTheDocument()
    expect(screen.getAllByText('Open this run')[0]).toHaveAttribute('href', '/twin?node=aaaaaaaa&run=r1')
    expect(screen.getByTestId('twin-question').querySelector('script')).toBeNull()
    await userEvent.click(screen.getByRole('button', { name: 'Web' }))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'twin.answer')?.params).toEqual({ id: 'q1', answer: 'Web' }))
  })
  it('says on which node you took over, by name when paired and by short id when not', async () => {
    const B = 'b'.repeat(32)
    const F = 'f'.repeat(32)
    useNodes.setState((st) => ({ byId: { ...st.byId, [B]: nodeEntry({ id: B, name: 'beta' }) }, order: [A, B] }))
    const events: TodoEvent[] = [
      { seq: 3, ts_ms: 3, kind: 'taken_over', by: { kind: 'device' }, node_id: F },
      { seq: 2, ts_ms: 2, kind: 'taken_over', by: { kind: 'device' }, node_id: B },
      { seq: 1, ts_ms: 1, kind: 'taken_over', by: { kind: 'device' } },
    ]
    rpc.handlers['todo.history'] = () => ({ events, has_more: false })
    const item = { ...todoItem({ id: 't1', handler: 'me' }), node_id: A, unsynced: false }
    render(<TodoHistory item={item} />)
    expect(await screen.findByText('You took over')).toBeInTheDocument()
    const onNode = screen.getAllByTestId('taken-over-on').map((e) => e.textContent)
    expect(onNode).toEqual(['You took over on beta', 'You took over on another node (ffffffff)'])
  })
  it('shows the four PR events and only links GitHub addresses', async () => {
    const events: TodoEvent[] = [
      { seq: 4, ts_ms: 4, kind: 'pr_event', session_id: 's', event: 'closed', url: 'javascript:alert(1)' },
      { seq: 3, ts_ms: 3, kind: 'pr_event', session_id: 's', event: 'merged', url: 'https://github.com/o/r/pull/1' },
      { seq: 2, ts_ms: 2, kind: 'pr_event', session_id: 's', event: 'reviewed', url: 'https://evil.example/https://github.com/' },
      { seq: 1, ts_ms: 1, kind: 'pr_event', session_id: 's', event: 'checks_failed' },
    ]
    rpc.handlers['todo.history'] = () => ({ events, has_more: false })
    const item = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
    render(<TodoHistory item={item} />)
    expect(await screen.findByText('The checks on the pull request failed')).toBeInTheDocument()
    expect(screen.getByText('The pull request has new review activity')).toBeInTheDocument()
    expect(screen.getByText('The pull request was merged')).toBeInTheDocument()
    expect(screen.getByText('The pull request was closed without merging')).toBeInTheDocument()
    const links = screen.getAllByRole('link', { name: 'Open the pull request' })
    expect(links).toHaveLength(1)
    expect(links[0]).toHaveAttribute('href', 'https://github.com/o/r/pull/1')
  })

  it('answering inline with “Always do this” ticked opens the rule editor only after the answer succeeded', async () => {
    useFollowUp.getState().close()
    useNodes.getState().patch(A, { info: { ...info, capabilities: { ...info.capabilities, rules: true } } })
    twin.questions = [twinQuestion()]
    await syncTwin(A)
    rpc.handlers['todo.history'] = () => ({ events: [{ seq: 1, ts_ms: 1, kind: 'twin_question', question_id: 'q1' }], has_more: false })
    const item = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
    render(
      <>
        <TodoHistory item={item} />
        <FollowUpRuleHost />
      </>,
    )
    await screen.findByTestId('twin-question')
    // not ticked: no editor
    await userEvent.click(screen.getByRole('button', { name: 'Web' }))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'twin.answer')).toHaveLength(1))
    expect(screen.queryByTestId('rule-editor')).not.toBeInTheDocument()
    // ticked, and the answer is refused: no editor
    twin.questions = [twinQuestion()]
    await syncTwin(A)
    rpc.handlers['twin.answer'] = () => { throw new Error('offline') }
    await userEvent.click(await screen.findByTestId('always-do-this-box'))
    await userEvent.click(screen.getByRole('button', { name: 'API' }))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'twin.answer')).toHaveLength(2))
    expect(screen.queryByTestId('rule-editor')).not.toBeInTheDocument()
    // ticked, and it works: the editor opens with the model’s question apart from your answer
    rpc.handlers['twin.answer'] = (p) => ({ ...twinQuestion(), status: 'answered', answer: (p as { answer: string }).answer })
    await userEvent.click(screen.getByRole('button', { name: 'API' }))
    expect(await screen.findByTestId('rule-editor')).toBeInTheDocument()
    expect(screen.getByTestId('record-agent')).toHaveTextContent('Which project?')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
  })
  it('shows remote dispatch and remote result with the node name, linking to the other node only when this device is paired with it', async () => {
    const B = 'b'.repeat(32)
    const F = 'f'.repeat(32)
    useNodes.setState((st) => ({ byId: { ...st.byId, [B]: nodeEntry({ id: B, name: 'beta' }) }, order: [A, B] }))
    const events: TodoEvent[] = [
      { seq: 4, ts_ms: 4, kind: 'remote_result', node_id: B, session_id: 'sb', text: 'Fixed <script>alert(1)</script> **done**' },
      { seq: 3, ts_ms: 3, kind: 'remote_result', node_id: F, session_id: 'sf', text: 'From afar' },
      { seq: 2, ts_ms: 2, kind: 'remote_dispatched', node_id: F, todo_id: 'rf', session_id: 'sf' },
      { seq: 1, ts_ms: 1, kind: 'remote_dispatched', node_id: B, todo_id: 'rb', session_id: 'sb' },
    ]
    rpc.handlers['todo.history'] = () => ({ events, has_more: false })
    const item = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
    render(<TodoHistory item={item} />)
    const sent = await screen.findAllByTestId('remote-dispatched')
    expect(sent[0]).toHaveTextContent('Sent to beta')
    expect(screen.getByRole('link', { name: 'Open the item on beta' })).toHaveAttribute('href', '/todos/bbbbbbbb/rb')
    expect(screen.getAllByRole('link', { name: 'Open the session on beta' })).toHaveLength(2)
    // 没配过对的节点：只有名字（id 前 8 位），没有链接
    expect(sent[1]).toHaveTextContent('Sent to another node (ffffffff)')
    expect(sent[1]).toHaveTextContent('This device is not paired with that node.')
    expect(sent[1]!.querySelector('a')).toBeNull()
    const results = screen.getAllByTestId('remote-result')
    expect(results[1]).toHaveTextContent('Result from beta')
    expect(results[1]).toHaveTextContent('Fixed')
    expect(results[1]!.querySelector('script')).toBeNull()
    expect(results[0]).toHaveTextContent('Result from another node (ffffffff)')
    expect(results[0]!.querySelector('a')).toBeNull()
  })
  it('shows the Twin reason in acceptance, and emits no handler entry without capability', () => {
    const item = { ...todoItem({ id: 't1' }), node_id: A, unsynced: false }
    act(() =>
      useTodoHistory
        .getState()
        .patch(historyKey(A, 't1'), () => ({ events: [{ kind: 'twin_decision', action: 'done', reason: 'All criteria met', run_id: 'r1', seq: 1, ts_ms: 1 }] })),
    )
    const { unmount } = render(<TwinReviewReason item={item} />)
    expect(screen.getByText('All criteria met')).toBeInTheDocument()
    unmount()
    useNodes.getState().patch(A, { info: { ...info, capabilities: { ...info.capabilities, twin: false } } })
    render(<TwinHandlerButton item={item} />)
    expect(screen.queryByRole('button')).not.toBeInTheDocument()
  })
})

it('shows a retry when loading pre-dispatch Twin history fails', async () => {
  rpc.handlers['todo.history'] = () => { throw new Error('offline') }
  const item = { ...todoItem({ id: 't1', handler: 'twin' }), node_id: A, unsynced: false }
  render(<TodoHistory item={item} />)
  expect(await screen.findByTestId('history-failed')).toBeInTheDocument()
  rpc.handlers['todo.history'] = () => ({ events: [{ seq: 1, ts_ms: 1, kind: 'taken_over', by: { kind: 'device' } }], has_more: false })
  await userEvent.click(screen.getByTestId('history-retry'))
  expect(await screen.findByText('You took over')).toBeInTheDocument()
})
