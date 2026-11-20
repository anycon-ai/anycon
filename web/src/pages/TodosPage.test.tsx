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

  it('E, X and Enter do nothing when no row has the cursor (they used to act on the first row)', async () => {
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

  it('the row with the cursor has a non-colour marker', async () => {
    await load({ id: 'a', title: 'first' })
    press('j')
    expect(within(row('a')).getByTestId('todo-cursor')).toBeInTheDocument()
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
    await act(async () => handleTodoChanged(A, { id: 'a', deleted: true }))
    expect(await screen.findByTestId('todo-orphan')).toBeInTheDocument()
    expect(screen.getByTestId('todo-orphan-text')).toHaveTextContent('my long note plus more')
    await userEvent.click(screen.getByTestId('todo-orphan-save'))
    await waitFor(() => expect([...daemon.todos.values()].some((t) => t.title === 'first' && t.body === 'my long note plus more')).toBe(true))
    await waitFor(() => expect(useDrafts.getState().drafts).toEqual({}))
  })

  it('discarding the orphaned text clears the draft, so nothing keeps asking before the tab closes', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await userEvent.type(screen.getByTestId('todo-edit-title'), '!')
    await act(async () => handleTodoChanged(A, { id: 'a', deleted: true }))
    await userEvent.click(await screen.findByTestId('todo-orphan-discard'))
    expect(useDrafts.getState().drafts).toEqual({})
    expect(await screen.findByTestId('todo-gone')).toBeInTheDocument()
  })

  it('an untouched draft for an item that is gone is dropped silently', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'first' }))
    await syncTodos(A)
    render(<TodosPage selected={{ node: A, todo: 'a' }} />)
    await userEvent.click(await screen.findByTestId('todo-edit'))
    await act(async () => handleTodoChanged(A, { id: 'a', deleted: true }))
    expect(await screen.findByTestId('todo-gone')).toBeInTheDocument()
    await waitFor(() => expect(useDrafts.getState().drafts).toEqual({}))
  })
})
