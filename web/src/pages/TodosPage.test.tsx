// 清单页（spec/web.md § 4.12）：分组、筛选、已关闭分页展开、没有节点支持的提示、缓存标注、键盘、滑动完成 + 撤销、多选。
// 业务层用真的 service + 假 daemon（和 service.test.ts 同一套），只替掉路由。
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { useCapture } from '@/features/todos/capture'
import { useDrafts } from '@/features/todos/drafts'
import { handleTodoChanged, resetTodosMemory, syncTodos } from '@/features/todos/service'
import { useTodos } from '@/features/todos/store'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'

const h = vi.hoisted(() => ({ navigate: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}, { pathname: '/todos' }))

vi.mock('@/api/queries', async (orig) => ({ ...(await orig<object>()), useDevices: () => ({ data: undefined, dataUpdatedAt: 0 }) }))

import { TodosPage } from './TodosPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const C = 'c'.repeat(32)
const info = (todo: boolean): NodeInfo => ({ capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const rows = (): HTMLElement[] => screen.queryAllByTestId('todo-row')
const titles = (): string[] => rows().map((r) => within(r).getByTestId('todo-main').textContent ?? '')
const row = (id: string): HTMLElement => rows().find((r) => r.getAttribute('data-id') === id)!

async function load(...items: Array<Parameters<typeof todoItem>[0]>): Promise<void> {
  daemon.seed(...items.map(todoItem))
  await syncTodos(A)
  render(<TodosPage />)
  await flush()
}

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  h.navigate.mockReset()
  resetViewport()
  usePrefs.setState({ sidebarNodes: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready' }), [C]: nodeEntry({ id: C, name: 'gamma', info: info(false), conn: 'ready' }) }, order: [A, C], loaded: true })
  resetTodosMemory()
  useDrafts.setState({ drafts: {} })
  useUi.setState({ toasts: [] })
  useCapture.setState({ open: false, text: '' })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('groups', () => {
  it('working first, then not started; closed ones are collapsed and not even asked for', async () => {
    await load({ id: 'w', title: 'in progress', state: 'working' }, { id: 'o', title: 'later' }, { id: 'd', title: 'finished', state: 'done' })
    expect(within(screen.getByTestId('todos-group-working')).getAllByTestId('todo-row')).toHaveLength(1)
    expect(within(screen.getByTestId('todos-group-open')).getAllByTestId('todo-row')).toHaveLength(1)
    expect(titles()).not.toContain('finished')
    expect(screen.getByTestId('todos-closed-toggle')).toHaveAttribute('aria-expanded', 'false')
    expect(daemon.log.some((l) => (l.params as { include_closed?: boolean }).include_closed)).toBe(false)
  })

  it('the five groups appear in order (waiting for you, to review, in progress, not started) and an unknown activity counts as none', async () => {
    await load(
      { id: 'o', title: 'not started' },
      { id: 'w', title: 'agent busy', activity: 'working' as never, sessions: ['s1'] as never },
      { id: 'r', title: 'review me', activity: 'review' as never, sessions: ['s2'] as never },
      { id: 'd', title: 'decide me', activity: 'waiting' as never, sessions: ['s3'] as never },
      { id: 'x', title: 'from the future', activity: 'something-new' as never },
    )
    const order = [...document.querySelectorAll('[data-testid^="todos-group-"]')].map((g) => g.getAttribute('data-testid')).filter((id) => id !== 'todos-group-closed')
    expect(order).toEqual(['todos-group-decide', 'todos-group-review', 'todos-group-working', 'todos-group-open'])
    expect(within(screen.getByTestId('todos-group-decide')).getAllByTestId('todo-row').map((r) => r.getAttribute('data-id'))).toEqual(['d'])
    expect(within(screen.getByTestId('todos-group-review')).getAllByTestId('todo-row').map((r) => r.getAttribute('data-id'))).toEqual(['r'])
    expect(within(screen.getByTestId('todos-group-open')).getAllByTestId('todo-row').map((r) => r.getAttribute('data-id')).sort()).toEqual(['o', 'x'])
    // 谁在处理：agent 在跑的是 agent，等你决定 / 待验收是我
    expect(within(row('w')).getByTestId('todo-handler')).toHaveAttribute('data-state', 'agent')
    expect(within(row('r')).getByTestId('todo-handler')).toHaveAttribute('data-state', 'me')
    expect(within(row('o')).queryByTestId('todo-handler')).toBeNull()
  })

  it('repeating items (templates) sit in a group of their own after "not started", collapsible, each with its rule and next time', async () => {
    const next = Date.UTC(2030, 0, 7, 9, 0)
    await load(
      { id: 'o', title: 'later' },
      { id: 'tpl', title: 'Weekly review', repeat: { kind: 'weekly', at: '09:00', weekday: 1, next_ms: next } as never },
      { id: 'copy', title: 'Weekly review', source: { kind: 'repeat', todo_id: 'tpl' } as never },
    )
    const order = [...document.querySelectorAll('[data-testid^="todos-group-"]')].map((g) => g.getAttribute('data-testid')).filter((id) => id !== 'todos-group-closed')
    expect(order).toEqual(['todos-group-open', 'todos-group-repeat'])
    expect(within(screen.getByTestId('todos-group-repeat')).getAllByTestId('todo-row').map((r) => r.getAttribute('data-id'))).toEqual(['tpl'])
    // 复制出来的项是一项普通的待办，在「还没开始」里，带「来自周期项」
    expect(within(screen.getByTestId('todos-group-open')).getAllByTestId('todo-row').map((r) => r.getAttribute('data-id')).sort()).toEqual(['copy', 'o'])
    expect(within(row('copy')).getByTestId('todo-repeat-from-badge')).toBeInTheDocument()
    expect(row('tpl')).toHaveTextContent(/Every Monday at 09:00 · next/)
    await userEvent.click(screen.getByTestId('todos-repeat-toggle'))
    expect(screen.getByTestId('todos-repeat-toggle')).toHaveAttribute('aria-expanded', 'false')
    expect(rows().map((r) => r.getAttribute('data-id')).sort()).toEqual(['copy', 'o'])
  })

  it('the “who is handling it” filter narrows the list', async () => {
    await load({ id: 'w', title: 'agent busy', activity: 'working' as never }, { id: 'r', title: 'review me', activity: 'review' as never }, { id: 'o', title: 'later' })
    await userEvent.selectOptions(screen.getByTestId('todos-filter-handler'), 'agent')
    expect(rows().map((r) => r.getAttribute('data-id'))).toEqual(['w'])
    await userEvent.selectOptions(screen.getByTestId('todos-filter-handler'), 'me')
    expect(rows().map((r) => r.getAttribute('data-id')).sort()).toEqual(['o', 'r'])
  })

  it('an activity-only change (the same updated_ms) moves the row to its new group', async () => {
    await load({ id: 'r', title: 'review me', activity: 'working' as never, updated_ms: 50 })
    expect(within(screen.getByTestId('todos-group-working')).getAllByTestId('todo-row')).toHaveLength(1)
    act(() => handleTodoChanged(A, { id: 'r', todo: todoItem({ id: 'r', title: 'review me', activity: 'review' as never, updated_ms: 50 }) }))
    await flush()
    expect(within(screen.getByTestId('todos-group-review')).getAllByTestId('todo-row')).toHaveLength(1)
    expect(screen.queryByTestId('todos-group-working')).toBeNull()
  })

  it('expanding “closed” fetches them page by page', async () => {
    daemon.pageSize = 2
    await load(...[1, 2, 3].map((n) => ({ id: `c${n}`, title: `closed ${n}`, state: 'done' as const, updated_ms: n * 10 })))
    await userEvent.click(screen.getByTestId('todos-closed-toggle'))
    await flush()
    expect(rows()).toHaveLength(2)
    await userEvent.click(screen.getByTestId('todos-closed-more'))
    await flush()
    expect(rows()).toHaveLength(3)
    expect(screen.queryByTestId('todos-closed-more')).toBeNull()
  })

  it('shows an empty state when there is nothing', async () => {
    await load()
    expect(screen.getByText('No to-dos')).toBeInTheDocument()
  })
})

describe('filters', () => {
  it('by project and by source', async () => {
    await load({ id: 'a', title: 'one', project: 'api' }, { id: 'b', title: 'two', project: 'web' }, { id: 'c', title: 'three', project: 'api', source: { kind: 'agent' }, unreviewed: false })
    await userEvent.selectOptions(screen.getByTestId('todos-filter-project'), 'api')
    expect(rows().map((r) => r.getAttribute('data-id')).sort()).toEqual(['a', 'c'])
    await userEvent.selectOptions(screen.getByTestId('todos-filter-source'), 'agent')
    expect(rows().map((r) => r.getAttribute('data-id'))).toEqual(['c'])
  })

  it('agent items that nobody has accepted are not on the list (they are in the inbox)', async () => {
    await load({ id: 'a', title: 'mine' }, { id: 'u', title: 'from agent', source: { kind: 'agent' }, unreviewed: true })
    expect(rows().map((r) => r.getAttribute('data-id'))).toEqual(['a'])
  })
})

describe('nodes', () => {
  it('says so when none of the selected nodes supports to-dos', async () => {
    usePrefs.setState({ sidebarNodes: [C] })
    render(<TodosPage />)
    expect(screen.getByText(/None of the selected nodes keeps a to-do list/)).toBeInTheDocument()
    expect(rows()).toHaveLength(0)
  })

  it('shows the node name only when several supporting nodes are selected, and labels cached data from an offline node', async () => {
    useNodes.setState({ byId: { ...useNodes.getState().byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true), conn: 'ready' }) }, order: [A, B, C] })
    daemon.seed(todoItem({ id: 'a', title: 'first item', project: 'p' }))
    await syncTodos(A)
    const { unmount } = render(<TodosPage />)
    await flush()
    expect(titles().join('|')).toMatch(/alpha/)
    unmount()
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    usePrefs.setState({ sidebarNodes: [A] })
    render(<TodosPage />)
    await flush()
    expect(screen.getByTestId('todos-cached')).toHaveTextContent('alpha')
    expect(rows()).toHaveLength(1)
    expect(titles().join('|')).not.toMatch(/alpha/) // 只勾了一个节点：不标节点名
  })

  it('the not-synced marker only shows while the node cannot be reached', async () => {
    await load({ id: 'a', title: 'x' })
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    rpc.ready = false
    const { updateTodo } = await import('@/features/todos/service')
    await act(async () => {
      await updateTodo(A, 'a', { title: 'edited' })
    })
    await flush()
    expect(within(row('a')).getByTestId('todo-unsynced')).toBeInTheDocument()
  })
})

describe('keyboard', () => {
  it('J / K move, E marks done (with an undo toast), Enter opens, X selects several', async () => {
    const user = userEvent.setup()
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 }, { id: 'c', title: 'third', updated_ms: 10 })
    await user.keyboard('j')
    expect(row('a')).toHaveAttribute('data-selected', 'true')
    await user.keyboard('jj')
    expect(row('c')).toHaveAttribute('data-selected', 'true')
    await user.keyboard('k')
    expect(row('b')).toHaveAttribute('data-selected', 'true')
    await user.keyboard('{Enter}')
    expect(h.navigate).toHaveBeenCalledWith(expect.objectContaining({ to: '/todos/$node/$todo', params: expect.objectContaining({ todo: 'b' }) }))
    await user.keyboard('e')
    await flush()
    expect(daemon.todos.get('b')!.state).toBe('done')
    expect(useUi.getState().toasts.some((t) => t.action)).toBe(true)
  })

  it('X marks several, E then applies to all of them', async () => {
    const user = userEvent.setup()
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 }, { id: 'c', title: 'third', updated_ms: 10 })
    await user.keyboard('jxjx')
    expect(screen.getByTestId('todos-bulk')).toHaveTextContent('2')
    await user.keyboard('e')
    await waitFor(() => expect([...daemon.todos.values()].filter((t) => t.state === 'done').map((t) => t.id).sort()).toEqual(['a', 'b']))
  })

  it('does nothing while typing in a field or when the page is hidden', async () => {
    const user = userEvent.setup()
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(
      <>
        <input data-testid="typing" />
        <TodosPage active={false} />
      </>,
    )
    await flush()
    await user.keyboard('je')
    expect(daemon.todos.get('a')!.state).toBe('open')
    await user.click(screen.getByTestId('typing'))
    await user.keyboard('e')
    expect(daemon.todos.get('a')!.state).toBe('open')
  })
})

describe('phone', () => {
  beforeEach(() => setViewport('phone'))

  it('swiping a row left marks it done with a 5 second undo; undo puts it back', async () => {
    const timers = vi.spyOn(globalThis, 'setTimeout')
    await load({ id: 'a', title: 'swipe me' })
    const main = within(row('a')).getByTestId('todo-main').parentElement!
    fireEvent.pointerDown(main, { pointerId: 1, pointerType: 'touch', clientX: 300, clientY: 100 })
    fireEvent.pointerMove(main, { pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 102 })
    fireEvent.pointerUp(main, { pointerId: 1, pointerType: 'touch', clientX: 200, clientY: 102 })
    await flush()
    expect(daemon.todos.get('a')!.state).toBe('done')
    const toast = useUi.getState().toasts.find((t) => t.action)!
    expect(timers.mock.calls.some((c) => c[1] === 5000)).toBe(true)
    await act(async () => {
      toast.action!.run()
    })
    await flush()
    expect(daemon.todos.get('a')!.state).toBe('open')
  })

  it('a short or vertical drag does nothing; a mouse never swipes', async () => {
    await load({ id: 'a', title: 'stay' })
    const main = within(row('a')).getByTestId('todo-main').parentElement!
    for (const [type, x, y] of [['touch', 270, 100], ['touch', 150, 260], ['mouse', 100, 100]] as const) {
      fireEvent.pointerDown(main, { pointerId: 1, pointerType: type, clientX: 300, clientY: 100 })
      fireEvent.pointerMove(main, { pointerId: 1, pointerType: type, clientX: x, clientY: y })
      fireEvent.pointerUp(main, { pointerId: 1, pointerType: type, clientX: x, clientY: y })
    }
    await flush()
    expect(daemon.todos.get('a')!.state).toBe('open')
  })

  it('the menu (the same actions as the swipe) marks done and deletes after a confirmation', async () => {
    const user = userEvent.setup()
    await load({ id: 'a', title: 'menu me' }, { id: 'b', title: 'delete me', updated_ms: 5 })
    await user.click(within(row('a')).getByTestId('todo-menu'))
    await user.click(await screen.findByTestId('todo-act-done'))
    await flush()
    expect(daemon.todos.get('a')!.state).toBe('done')
    await user.click(within(row('b')).getByTestId('todo-menu'))
    await user.click(await screen.findByTestId('todo-act-delete'))
    expect(daemon.todos.has('b')).toBe(true) // 还没确认
    await user.click(await screen.findByTestId('todo-delete-confirm'))
    await waitFor(() => expect(daemon.todos.has('b')).toBe(false))
  })
})

describe('agent text is untrusted', () => {
  it('control and bidi characters never reach the row; markup stays text', async () => {
    await load({ id: 'a', title: `evil‮txt.exe\u0007<img src=x onerror=alert(1)>`, source: { kind: 'agent' }, unreviewed: false })
    const text = within(row('a')).getByTestId('todo-main').textContent ?? ''
    expect(text).not.toContain('\u202E')
    expect(text).not.toContain('\u0007')
    expect(text).toContain('<img src=x onerror=alert(1)>')
    expect(row('a').querySelector('img')).toBeNull()
  })
})

describe('keyboard safety', () => {
  const press = (key: string, init: KeyboardEventInit = {}): void => void fireEvent.keyDown(window, { key, ...init })

  it('E, X and Enter do nothing when no row has the cursor', async () => {
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 })
    press('e')
    press('x')
    press('Enter')
    await flush()
    expect([...daemon.todos.values()].every((t) => t.state === 'open')).toBe(true)
    expect(screen.queryByTestId('todos-bulk')).toBeNull()
    expect(h.navigate).not.toHaveBeenCalled()
  })

  it('a held key (repeat) is ignored: holding E completes one item, not three', async () => {
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 }, { id: 'c', title: 'third', updated_ms: 10 })
    press('j')
    press('e')
    press('e', { repeat: true })
    press('e', { repeat: true })
    await flush()
    expect([...daemon.todos.values()].filter((t) => t.state === 'done').map((t) => t.id)).toEqual(['a'])
  })

  it('after E the cursor moves to the next row, so pressing E again completes the next one', async () => {
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 }, { id: 'c', title: 'third', updated_ms: 10 })
    press('j')
    press('e')
    await waitFor(() => expect(daemon.todos.get('a')!.state).toBe('done'))
    await waitFor(() => expect(row('b')).toHaveAttribute('data-selected', 'true'))
    press('e')
    await waitFor(() => expect(daemon.todos.get('b')!.state).toBe('done'))
  })

  it('a cursor whose row has gone from the list is not silently replaced by the first row', async () => {
    await load({ id: 'a', title: 'first', updated_ms: 30 }, { id: 'b', title: 'second', updated_ms: 20 })
    press('j')
    press('j')
    expect(row('b')).toHaveAttribute('data-selected', 'true')
    await act(async () => handleTodoChanged(A, { id: 'b', deleted: true }))
    press('e')
    press('Enter')
    await flush()
    expect(daemon.todos.get('a')!.state).toBe('open')
    expect(h.navigate).not.toHaveBeenCalled()
  })

  it('the row with the cursor is marked without adding anything that shifts its text', async () => {
    await load({ id: 'a', title: 'first' })
    press('j')
    expect(row('a')).toHaveAttribute('data-selected', 'true')
    expect(within(row('a')).getByTestId('todo-main')).toHaveAttribute('aria-current', 'true')
    expect(within(row('a')).queryByTestId('todo-cursor')).toBeNull()
  })
})

describe('the detail from the address', () => {
  it('opens from the full node id as well as the short one, and shows a loading state until the cache is read', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'from url' }))
    useTodos.setState({ nodes: {} })
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    expect(await screen.findByTestId('todo-loading')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-gone')).toBeNull()
    await act(async () => {
      await syncTodos(A)
    })
    expect(await screen.findByTestId('todo-title')).toHaveTextContent('from url')
    expect(screen.queryByTestId('todo-gone')).toBeNull()
  })

  it('says why there is nothing: unknown node, a node without to-do support, an item that does not exist', async () => {
    daemon.seed(todoItem({ id: 'a' }))
    await syncTodos(A)
    const { rerender } = render(<TodosPage selected={{ node: 'zzzzzzzz', todo: 'a' }} />)
    expect(await screen.findByTestId('todo-unknown-node')).toBeInTheDocument()
    rerender(<TodosPage selected={{ node: C, todo: 'a' }} />)
    expect(await screen.findByTestId('todo-unsupported-node')).toBeInTheDocument()
    rerender(<TodosPage selected={{ node: A, todo: 'missing' }} />)
    expect(await screen.findByTestId('todo-gone')).toBeInTheDocument()
  })

  it('an item on a node that is not ticked in the filter is shown with the “not in your node filter” note and a button to show the node', async () => {
    useNodes.setState({ byId: { ...useNodes.getState().byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true), conn: 'ready' }) }, order: [A, B, C] })
    daemon.seed(todoItem({ id: 'a', title: 'on alpha' }))
    await syncTodos(A)
    usePrefs.setState({ sidebarNodes: [B] })
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    expect(await screen.findByTestId('todo-outside-note')).toHaveTextContent('alpha')
    expect(screen.getByTestId('todo-title')).toHaveTextContent('on alpha')
    await userEvent.click(screen.getByTestId('todo-show-node'))
    expect(usePrefs.getState().sidebarNodes).toEqual(expect.arrayContaining([A, B]))
  })

  it('an unreviewed agent item opened by address offers Accept / Dismiss, not the edit controls', async () => {
    daemon.seed(todoItem({ id: 'a', source: { kind: 'agent' }, unreviewed: true }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    expect(await screen.findByTestId('todo-accept')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-edit')).toBeNull()
  })
})

describe('unsaved edits', () => {
  it('a row with a half-written edit says so', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-title'), ' more')
    expect(within(row('a')).getByTestId('todo-draft')).toBeInTheDocument()
  })

  it('an item deleted elsewhere while you edit it does not take your text with it: it is shown with Copy, Save as a new to-do and Discard', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first', body: 'my long note' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-body'), ' plus more')
    await act(async () => {
      daemon.todos.delete('a')
      handleTodoChanged(A, { id: 'a', deleted: true })
    })
    expect(await screen.findByTestId('todo-orphan')).toBeInTheDocument()
    expect(screen.getByTestId('todo-orphan-text')).toHaveTextContent('my long note plus more')
    await userEvent.click(screen.getByTestId('todo-orphan-save'))
    await waitFor(() => expect([...daemon.todos.values()].some((t) => t.title === 'first' && t.body === 'my long note plus more')).toBe(true))
    await waitFor(() => expect(useDrafts.getState().drafts).toEqual({}))
  })

  it('saving an orphaned edit as a new to-do keeps the repeat rule that was being edited', async () => {
    useNodes.getState().patch(A, { info: { capabilities: { todo: true, todo_repeat: true } } as unknown as NodeInfo })
    daemon.seed(todoItem({ id: 'a', title: 'weekly', repeat: { kind: 'weekdays', at: '08:00' } }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-title'), ' plus')
    await act(async () => {
      daemon.todos.delete('a')
      handleTodoChanged(A, { id: 'a', deleted: true })
    })
    await userEvent.click(await screen.findByTestId('todo-orphan-save'))
    await waitFor(() => expect([...daemon.todos.values()].find((t) => t.title === 'weekly plus')?.repeat).toEqual({ kind: 'weekdays', at: '08:00' }))
  })

  it('discarding the orphaned text clears the draft, so nothing keeps asking before the tab closes', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-title'), '!')
    await act(async () => {
      daemon.todos.delete('a')
      handleTodoChanged(A, { id: 'a', deleted: true })
    })
    await userEvent.click(await screen.findByTestId('todo-orphan-discard'))
    expect(useDrafts.getState().drafts).toEqual({})
    expect(await screen.findByTestId('todo-gone')).toBeInTheDocument()
  })

  it('an untouched draft for an item that is gone is dropped silently', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await act(async () => {
      daemon.todos.delete('a')
      handleTodoChanged(A, { id: 'a', deleted: true })
    })
    expect(await screen.findByTestId('todo-gone')).toBeInTheDocument()
    await waitFor(() => expect(useDrafts.getState().drafts).toEqual({}))
  })
})

describe('orphaned edits are findable from the list', () => {
  const vals = { title: 'my unsaved note', body: 'details', project: '', due: '' }

  it('a dirty draft for an item that is gone shows up above the list with an Open button, even when you are not on its detail', async () => {
    await load({ id: 'a', title: 'still here' })
    act(() => {
      useDrafts.getState().start(`${A}:gone`, { ...vals, title: 'x' })
      useDrafts.getState().set(`${A}:gone`, 'title', vals.title)
    })
    await waitFor(() => expect(screen.getByTestId('todos-orphans')).toHaveTextContent('1 unsaved edit'))
    await userEvent.click(screen.getByTestId('todos-orphan-open'))
    expect(h.navigate).toHaveBeenCalledWith(expect.objectContaining({ to: '/todos/$node/$todo', params: expect.objectContaining({ todo: 'gone' }) }))
  })

  it('drafts of items that exist, untouched drafts, and a not-yet-sent new item are not listed', async () => {
    await load({ id: 'a', title: 'still here' })
    act(() => {
      useDrafts.getState().start(`${A}:a`, vals)
      useDrafts.getState().set(`${A}:a`, 'title', 'edited')
      useDrafts.getState().start(`${A}:untouched`, vals)
    })
    expect(screen.queryByTestId('todos-orphans')).toBeNull()
  })

  it('“Save as a new to-do” works while the node is offline: the add is queued, the draft is cleared and nothing is sent yet', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first', body: 'my note' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-body'), ' more')
    await act(async () => {
      daemon.todos.delete('a')
      handleTodoChanged(A, { id: 'a', deleted: true })
    })
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    rpc.ready = false
    daemon.log = []
    await userEvent.click(await screen.findByTestId('todo-orphan-save'))
    await waitFor(() => expect(useDrafts.getState().drafts).toEqual({}))
    expect(daemon.log).toEqual([])
    const queued = await db.listOutbox(A)
    expect(queued).toHaveLength(1)
    expect(queued[0]!.op).toMatchObject({ kind: 'add', params: { title: 'first', body: 'my note more' } })
  })
})


describe('an item that left the open list is not necessarily deleted', () => {
  async function editThenLeaveOpenList(change: () => void): Promise<void> {
    daemon.seed(todoItem({ id: 'a', title: 'first', body: 'note' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-body'), ' mine')
    // 断线期间在别处改的：没有 todo.changed，重新连上之后同步才发现它不在没关闭的列表里了
    change()
    await act(async () => {
      await syncTodos(A)
    })
  }

  it('closed elsewhere while away: the draft stays attached to the closed item, there is no orphan entry and no “save as a new to-do”', async () => {
    await editThenLeaveOpenList(() => daemon.todos.set('a', { ...daemon.todos.get('a')!, state: 'done', closed_ms: 5, updated_ms: 50 }))
    await waitFor(() => expect(screen.getByTestId('todo-edit-body')).toHaveValue('note mine'))
    expect(screen.queryByTestId('todo-orphan')).toBeNull()
    expect(screen.queryByTestId('todo-orphan-save')).toBeNull()
    expect(screen.queryByTestId('todos-orphans')).toBeNull()
    // 项是关闭的：可以重新打开、保存
    expect(screen.getByTestId('todo-state')).toHaveValue('done')
    expect(useDrafts.getState().drafts[`${A}:a`]).toBeDefined()
  })

  it('while that is being checked the page says so and offers no way to create a duplicate', async () => {
    const original = rpc.handlers['todo.list']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    daemon.seed(todoItem({ id: 'a', title: 'first', body: 'note' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-body'), ' mine')
    rpc.handlers['todo.list'] = async (p, n) => {
      if ((p as { include_closed?: boolean }).include_closed) await gate
      return original(p, n)
    }
    daemon.todos.set('a', { ...daemon.todos.get('a')!, state: 'done', closed_ms: 5, updated_ms: 50 })
    await act(async () => {
      await syncTodos(A)
    })
    expect(await screen.findByTestId('todo-checking')).toBeInTheDocument()
    expect(await screen.findByTestId('todos-orphans-checking')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-orphan-save')).toBeNull()
    release()
    await waitFor(() => expect(screen.queryByTestId('todo-checking')).toBeNull())
    expect(screen.getByTestId('todo-edit-body')).toHaveValue('note mine')
  })

  it('really deleted: after the closed walk completes without finding it, the orphan panel and the list entry appear', async () => {
    await editThenLeaveOpenList(() => daemon.todos.delete('a'))
    expect(await screen.findByTestId('todo-orphan')).toBeInTheDocument()
    expect(screen.getByTestId('todo-orphan-text')).toHaveTextContent('note mine')
    expect(await screen.findByTestId('todos-orphans')).toBeInTheDocument()
    expect(screen.queryByTestId('todos-orphans-checking')).toBeNull()
  })

  it('deleting from the list row drops that item’s draft, so it never shows up later as deleted elsewhere', async () => {
    await load({ id: 'a', title: 'first' }, { id: 'b', title: 'second', updated_ms: 5 })
    act(() => {
      useDrafts.getState().start(`${A}:a`, { title: 'first', body: '', project: '', due: '' })
      useDrafts.getState().set(`${A}:a`, 'title', 'first!')
    })
    await userEvent.click(within(row('a')).getByTestId('todo-menu'))
    await userEvent.click(await screen.findByTestId('todo-act-delete'))
    await userEvent.click(await screen.findByTestId('todo-delete-confirm'))
    await waitFor(() => expect(useDrafts.getState().drafts[`${A}:a`]).toBeUndefined())
    expect(screen.queryByTestId('todos-orphans')).toBeNull()
  })
})

describe('spaces', () => {
  const H = 'h'.repeat(32)
  const W = 'w'.repeat(32)
  const grouped = (group: string): NodeInfo => ({ group, capabilities: { todo: true } }) as unknown as NodeInfo

  beforeEach(() => {
    useNodes.setState({ byId: { [H]: nodeEntry({ id: H, name: 'house', info: grouped('Home'), conn: 'ready' }), [W]: nodeEntry({ id: W, name: 'office', info: grouped('Work'), conn: 'ready' }) }, order: [H, W], loaded: true })
    useTodos.getState().setCache(H, [todoItem({ id: 'ha', title: 'at home' })])
    useTodos.getState().setCache(W, [todoItem({ id: 'wa', title: 'at work' }), todoItem({ id: 'wb', title: 'also work', updated_ms: 5 })])
    usePrefs.setState({ sidebarNodes: null, sidebarSpace: null })
  })

  it('the list shows only the selected space’s nodes; with “All” it shows everything and names the node on each row', () => {
    usePrefs.setState({ sidebarSpace: 'Work' })
    const { unmount } = render(<TodosPage />)
    expect(titles().sort()).toEqual(['also work', 'at work'])
    unmount()
    usePrefs.setState({ sidebarSpace: null })
    render(<TodosPage />)
    expect(titles().join('|')).toMatch(/at home.*house|house.*at home/)
    expect(rows()).toHaveLength(3)
  })

  it('the open-to-do count (sidebar, tab bar) counts only the selected space', async () => {
    const { renderHook } = await import('@testing-library/react')
    const { useOpenTodoCount } = await import('@/features/todos/useOpenTodoCount')
    usePrefs.setState({ sidebarSpace: 'Work' })
    expect(renderHook(() => useOpenTodoCount()).result.current).toBe(2)
    usePrefs.setState({ sidebarSpace: 'Home' })
    expect(renderHook(() => useOpenTodoCount()).result.current).toBe(1)
  })

  it('a to-do opened by a link from outside the selected space is shown, with a note naming its space and a button to switch (never switching by itself)', async () => {
    usePrefs.setState({ sidebarSpace: 'Home' })
    render(<TodosPage selected={{ node: W, todo: 'wa' }} />)
    expect(await screen.findByTestId('todo-title')).toHaveTextContent('at work')
    expect(screen.getByTestId('todo-outside-note')).toHaveTextContent('belongs to the space “Work”, which is not selected')
    expect(usePrefs.getState().sidebarSpace).toBe('Home')
    await userEvent.click(screen.getByTestId('todo-switch-space'))
    expect(usePrefs.getState().sidebarSpace).toBe('Work')
  })

  it('a node with no group keeps the old note and button (“show this node”)', async () => {
    useNodes.getState().setInfo(W, grouped(''))
    useNodes.setState({ byId: { ...useNodes.getState().byId, [W]: { ...useNodes.getState().byId[W]!, info: { capabilities: { todo: true } } as unknown as NodeInfo } } })
    usePrefs.setState({ sidebarNodes: [H], sidebarSpace: null })
    render(<TodosPage selected={{ node: W, todo: 'wa' }} />)
    await screen.findByTestId('todo-outside-note')
    expect(screen.getByTestId('todo-show-node')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-switch-space')).toBeNull()
  })

  it('the row menu offers “Move to another node…” when another node supports to-dos', async () => {
    usePrefs.setState({ sidebarSpace: 'Work' })
    render(<TodosPage />)
    await userEvent.click(within(row('wa')).getByTestId('todo-menu'))
    expect(await screen.findByTestId('todo-act-move')).toHaveTextContent('Move to another node…')
  })
})
