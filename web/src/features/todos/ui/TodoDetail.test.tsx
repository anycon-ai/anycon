// 清单项详情（spec/web.md § 4.12）：编辑只发改了的字段、正文按消毒的 Markdown 渲染、删除要确认、agent 项的来源链接、
// 失败的操作有「重试 / 丢弃」、不出现「让 agent 处理」。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { handleTodoChanged, replayOutbox, resetTodosMemory, syncTodos } from '../service'
import { useDrafts } from '../drafts'
import { useTodoView } from '../store'
import { TodoDetail } from './TodoDetail'
import { summary } from '@/test/factories'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const A = 'a'.repeat(32)
const info = { capabilities: { todo: true } } as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}

const NODES = [A]
function Live({ id, onDeleted }: { id: string; onDeleted?: () => void }) {
  const item = useTodoView(NODES).find((v) => v.id === id)
  return item ? <TodoDetail item={item} online onDeleted={onDeleted} /> : <p data-testid="gone">gone</p>
}
async function show(over: Parameters<typeof todoItem>[0], onDeleted?: () => void): Promise<void> {
  daemon.seed(todoItem(over))
  await syncTodos(A)
  render(<Live id={over.id} onDeleted={onDeleted} />)
  await flush()
}

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info, conn: 'ready' }) }, order: [A], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  resetTodosMemory()
  useDrafts.setState({ drafts: {} })
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('rendering', () => {
  it('the body is sanitised Markdown: a script tag and a javascript: link do not become elements', async () => {
    await show({ id: 't', title: 'x', body: '**bold** <script>alert(1)</script> [click](javascript:alert(2)) <img src=x onerror=alert(3)>' })
    const body = screen.getByTestId('todo-body')
    expect(body.querySelector('strong')).toHaveTextContent('bold')
    expect(body.querySelector('script')).toBeNull()
    expect(body.querySelector('img')).toBeNull()
    expect(body.querySelector('a[href^="javascript:"]')).toBeNull()
  })

  it('the title is text with control and bidi characters removed', async () => {
    await show({ id: 't', title: 'a‮b\u0001c' })
    expect(screen.getByTestId('todo-title')).toHaveTextContent('a b c')
  })

  it('has no dispatch buttons', async () => {
    await show({ id: 't', title: 'x' })
    expect(screen.queryByText(/dispatch|hand off|let the agent|run with agent/i)).toBeNull()
  })

  it('an agent item links to its source session when it is known, and says so when it is gone', async () => {
    await show({ id: 't', title: 'x', source: { kind: 'agent', session_id: 's1' }, unreviewed: false })
    expect(screen.getByTestId('todo-source')).toHaveTextContent(/gone|no longer/i)
    useSessions.setState({ list: { s1: { ...summary({ id: 's1', title: 'the session' }), node_id: A, cached_seq: 0 } } })
    await flush()
    expect(screen.getByTestId('todo-source').querySelector('a')?.getAttribute('href')).toMatch(/\/n\/aaaaaaaa\/s\/s1$/)
  })
})

describe('editing', () => {
  it('sends only the fields that changed', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'old title', body: 'old body', project: 'api' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-title'))
    await user.type(screen.getByTestId('todo-edit-title'), 'new title')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    const update = daemon.log.filter((l) => l.method === 'todo.update').at(-1)!
    expect(update.params).toEqual({ id: 't', title: 'new title' })
    expect(daemon.todos.get('t')).toMatchObject({ title: 'new title', body: 'old body', project: 'api' })
  })

  it('clearing the project sends a clear, not an empty string', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'x', project: 'api' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-project'))
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(daemon.log.filter((l) => l.method === 'todo.update').at(-1)!.params).toEqual({ id: 't', clear: ['project'] })
  })

  it('an invalid title is explained and nothing is sent', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'x' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-title'))
    await user.click(screen.getByTestId('todo-save'))
    expect(await screen.findByTestId('todo-edit-error')).toBeInTheDocument()
    expect(daemon.methods().filter((m) => m === 'todo.update')).toEqual([])
  })

  it('state: done, reopen, and the state selector', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'x' })
    await user.click(screen.getByTestId('todo-done'))
    await flush()
    expect(daemon.todos.get('t')!.state).toBe('done')
    await user.click(screen.getByTestId('todo-reopen'))
    await flush()
    expect(daemon.todos.get('t')!.state).toBe('open')
    await user.selectOptions(screen.getByTestId('todo-state'), 'working')
    await flush()
    expect(daemon.todos.get('t')!.state).toBe('working')
    await user.selectOptions(screen.getByTestId('todo-state'), 'dropped')
    await flush()
    expect(daemon.todos.get('t')!.state).toBe('dropped')
  })
})

describe('delete', () => {
  it('asks first; cancelling keeps the item, confirming deletes it', async () => {
    const user = userEvent.setup()
    const deleted = vi.fn()
    await show({ id: 't', title: 'x' }, deleted)
    await user.click(screen.getByTestId('todo-delete'))
    await user.click(await screen.findByText('Cancel'))
    await flush()
    expect(daemon.todos.has('t')).toBe(true)
    await user.click(screen.getByTestId('todo-delete'))
    await user.click(await screen.findByTestId('todo-delete-confirm'))
    await waitFor(() => expect(daemon.todos.has('t')).toBe(false))
    await waitFor(() => expect(deleted).toHaveBeenCalled())
  })
})

describe('a rejected operation', () => {
  it('shows what failed with Retry and Discard', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'orig' })
    daemon.failNext('todo.update', rpcError('invalid', 'title'))
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-title'))
    await user.type(screen.getByTestId('todo-edit-title'), 'edited')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    await act(async () => void (await replayOutbox(A)))
    expect(await screen.findByTestId('todo-failed-op')).toBeInTheDocument()
    await user.click(screen.getByTestId('todo-retry'))
    await flush()
    expect(daemon.todos.get('t')!.title).toBe('edited')
    await waitFor(() => expect(screen.queryByTestId('todo-failed-op')).toBeNull())
  })

  it('Discard puts the item back to what the node has', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'orig' })
    daemon.failNext('todo.update', rpcError('invalid', 'title'))
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-title'))
    await user.type(screen.getByTestId('todo-edit-title'), 'edited')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    await user.click(await screen.findByTestId('todo-discard'))
    await flush()
    expect(screen.getByTestId('todo-title')).toHaveTextContent('orig')
    expect(daemon.todos.get('t')!.title).toBe('orig')
  })
})

describe('editing against a stale draft', () => {
  const live = (over: Parameters<typeof todoItem>[0]) => handleTodoChanged(A, { id: over.id, todo: todoItem(over) })
  const lastUpdate = () => daemon.log.filter((l) => l.method === 'todo.update').at(-1)?.params

  it('another device changing the body while you edit the title: only the title is sent, the other change survives, and you are told', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'old', body: 'orig body', updated_ms: 10 })
    await user.click(screen.getByTestId('todo-edit'))
    daemon.seed(todoItem({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    await act(async () => live({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    expect(screen.getByTestId('todo-changed-elsewhere')).toHaveTextContent(/Notes|Body/i)
    await user.clear(screen.getByTestId('todo-edit-title'))
    await user.type(screen.getByTestId('todo-edit-title'), 'new')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(lastUpdate()).toEqual({ id: 't', title: 'new' })
    expect(daemon.todos.get('t')).toMatchObject({ title: 'new', body: 'theirs' })
  })

  it('both sides changed the same field: you see theirs, choose, and Save is blocked until you do (no silent overwrite)', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'old', body: 'orig', updated_ms: 10 })
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-body'))
    await user.type(screen.getByTestId('todo-edit-body'), 'mine')
    daemon.seed(todoItem({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    await act(async () => live({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    expect(screen.getByTestId('todo-conflict')).toHaveAttribute('data-field', 'body')
    expect(screen.getByTestId('todo-conflict-theirs')).toHaveTextContent('theirs')
    expect(screen.getByTestId('todo-save')).toBeDisabled()
    await user.click(screen.getByTestId('todo-keep-mine'))
    expect(screen.getByTestId('todo-save')).toBeEnabled()
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(daemon.todos.get('t')!.body).toBe('mine')
  })

  it('“Use theirs” takes their version into the draft and nothing is sent for that field', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'old', body: 'orig', updated_ms: 10 })
    await user.click(screen.getByTestId('todo-edit'))
    await user.type(screen.getByTestId('todo-edit-body'), ' plus mine')
    daemon.seed(todoItem({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    await act(async () => live({ id: 't', title: 'old', body: 'theirs', updated_ms: 50 }))
    await user.click(screen.getByTestId('todo-use-theirs'))
    expect(screen.getByTestId('todo-edit-body')).toHaveValue('theirs')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(daemon.log.filter((l) => l.method === 'todo.update')).toHaveLength(0)
    expect(daemon.todos.get('t')!.body).toBe('theirs')
  })

  it('the draft survives the detail being remounted (resizing across the breakpoint, switching rows, leaving the page and coming back)', async () => {
    const user = userEvent.setup()
    daemon.seed(todoItem({ id: 't', title: 'old' }))
    await syncTodos(A)
    const first = render(<Live id="t" />)
    await flush()
    await user.click(screen.getByTestId('todo-edit'))
    await user.type(screen.getByTestId('todo-edit-title'), ' typed')
    first.unmount()
    render(<Live id="t" />)
    await flush()
    expect(screen.getByTestId('todo-edit-title')).toHaveValue('old typed')
    expect(Object.keys(useDrafts.getState().drafts)).toHaveLength(1)
  })

  it('Cancel with unsaved text asks first: Keep editing keeps the draft, Discard drops it; Cancel on an untouched form just closes', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'old' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.click(screen.getByTestId('todo-cancel-edit'))
    expect(screen.queryByTestId('todo-edit-title')).toBeNull()
    await user.click(screen.getByTestId('todo-edit'))
    await user.type(screen.getByTestId('todo-edit-title'), '!')
    await user.click(screen.getByTestId('todo-cancel-edit'))
    await user.click(await screen.findByText('Keep editing'))
    expect(screen.getByTestId('todo-edit-title')).toHaveValue('old!')
    await user.click(screen.getByTestId('todo-cancel-edit'))
    await user.click(await screen.findByTestId('todo-discard-edit-confirm'))
    expect(screen.queryByTestId('todo-edit-title')).toBeNull()
    expect(screen.getByTestId('todo-title')).toHaveTextContent('old')
  })

  it('saving without touching the due time never sends due_ms (seconds and milliseconds set elsewhere survive); changing it does', async () => {
    const user = userEvent.setup()
    const due = new Date('2031-05-06T07:08:09.123').getTime()
    await show({ id: 't', title: 'old', due_ms: due })
    await user.click(screen.getByTestId('todo-edit'))
    await user.type(screen.getByTestId('todo-edit-title'), '!')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(lastUpdate()).toEqual({ id: 't', title: 'old!' })
    expect(daemon.todos.get('t')!.due_ms).toBe(due)
    await user.click(screen.getByTestId('todo-edit'))
    await user.clear(screen.getByTestId('todo-edit-due'))
    await user.type(screen.getByTestId('todo-edit-due'), '2032-01-02T03:04')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(lastUpdate()).toEqual({ id: 't', due_ms: new Date('2032-01-02T03:04').getTime() })
  })
})

describe('an unreviewed agent item', () => {
  it('shows Accept / Dismiss instead of the edit and state controls', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'agent thing', source: { kind: 'agent' }, unreviewed: true })
    expect(screen.queryByTestId('todo-edit')).toBeNull()
    expect(screen.queryByTestId('todo-state')).toBeNull()
    await user.click(screen.getByTestId('todo-accept'))
    await flush()
    expect(daemon.todos.get('t')!.unreviewed).toBe(false)
  })
})
