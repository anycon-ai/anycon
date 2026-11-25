// 周期项的界面（spec/web.md § 14.6）：详情里的规则和「重复」控件（只在节点支持时出现）、模板没有「让 agent 处理」和「交给 Bot」、
// 复制出来的项的「来自周期项」、「经过」里的两种新事件、记一项里的「重复」。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { useCapture } from '../capture'
import { useDrafts } from '../drafts'
import { useTodoHistory } from '../history'
import { loadClosed, resetTodosMemory, syncTodos } from '../service'
import { useTodoView } from '../store'
import { CaptureForm } from './CaptureForm'
import { TodoDetail } from './TodoDetail'
import { TodoRow } from './TodoRow'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const A = 'a'.repeat(32)
const info = (repeat: boolean, twin = false): NodeInfo => ({ capabilities: { todo: true, todo_repeat: repeat, twin } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
function Live({ id }: { id: string }) {
  const item = useTodoView([A]).find((v) => v.id === id)
  return item ? <TodoDetail item={item} online /> : <p data-testid="gone">gone</p>
}
async function show(over: Parameters<typeof todoItem>[0]): Promise<void> {
  daemon.seed(todoItem(over))
  await syncTodos(A)
  render(<Live id={over.id} />)
  await flush()
}
const setNode = (repeat: boolean, twin = false): void =>
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(repeat, twin), conn: 'ready' }) }, order: [A], loaded: true })

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
  setRpc(rpc)
  setNode(true)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  resetTodosMemory()
  useDrafts.setState({ drafts: {} })
  useTodoHistory.getState().reset()
  useUi.setState({ toasts: [] })
  useCapture.setState({ open: false, text: '', focusTick: 0 })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

const weekly = { kind: 'weekly', at: '09:00', weekday: 1, next_ms: Date.UTC(2030, 0, 7, 9, 0) } as const

describe('a template in the detail', () => {
  it('shows the rule and the next time, and offers no "let an agent do it"; the bot toggle is about each copy', async () => {
    setNode(true, true)
    await show({ id: 't', title: 'Weekly review', repeat: weekly })
    expect(screen.getByTestId('todo-repeat-chip')).toBeInTheDocument()
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent(/Every Monday at 09:00/)
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent(/next/)
    expect(screen.queryByTestId('todo-dispatch')).toBeNull()
    expect(screen.getByTestId('todo-twin-handler')).toHaveTextContent('Each copy goes to the bot')
    expect(screen.queryByTestId('todo-move')).toBeNull()
    expect(screen.getByTestId('todo-repeat-note')).toBeInTheDocument()
  })

  it('a closed template says it no longer repeats and shows no next time', async () => {
    daemon.seed(todoItem({ id: 't', title: 'Old', state: 'done', repeat: weekly }))
    await syncTodos(A)
    await loadClosed(A)
    render(<Live id="t" />)
    await flush()
    expect(screen.getByTestId('todo-repeat-rule')).not.toHaveTextContent(/next/)
    expect(screen.getByTestId('todo-repeat-note')).toHaveTextContent(/no more copies/)
  })

  it('an ordinary item still has the dispatch button and no repeat row', async () => {
    await show({ id: 't', title: 'Plain' })
    expect(screen.getByTestId('todo-dispatch')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-repeat-rule')).toBeNull()
  })
})

describe('editing the rule', () => {
  it('the control is only there when the node supports repeating items', async () => {
    setNode(false)
    await show({ id: 't', title: 'Plain' })
    await userEvent.setup().click(screen.getByTestId('todo-edit'))
    expect(screen.queryByTestId('todo-edit-repeat')).toBeNull()
  })

  it('sets a weekly rule: only the changed field is sent, with the chosen day and time', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'Plain' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'weekly')
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-weekday'), '3')
    const at = screen.getByTestId('todo-edit-repeat-at')
    await user.clear(at)
    await user.type(at, '08:15')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    const sent = rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)
    expect(sent).toEqual([{ id: 't', repeat: { kind: 'weekly', at: '08:15', weekday: 3 } }])
    expect(daemon.todos.get('t')?.repeat).toEqual({ kind: 'weekly', at: '08:15', weekday: 3 })
  })

  it('"does not repeat" clears the rule', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'Tpl', repeat: weekly })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'none')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)).toEqual([{ id: 't', clear: ['repeat'] }])
    expect(daemon.todos.get('t')?.repeat).toBeUndefined()
  })

  it('a template hands each copy to the bot and back, with template wording; no toggle without bot support or for an unreviewed item', async () => {
    setNode(true, true)
    const user = userEvent.setup()
    await show({ id: 't', title: 'Weekly', repeat: weekly })
    await user.click(screen.getByTestId('todo-twin-handler'))
    await flush()
    expect(daemon.todos.get('t')?.handler).toBe('twin')
    expect(screen.getByTestId('todo-twin-handler')).toHaveTextContent("I’ll handle each copy")
    await user.click(screen.getByTestId('todo-twin-handler'))
    await flush()
    expect(daemon.todos.get('t')?.handler).toBe('me')
  })

  it('no hand-over toggle on a template when the node has no bot', async () => {
    setNode(true, false)
    await show({ id: 't', title: 'Weekly', repeat: weekly })
    expect(screen.queryByTestId('todo-twin-handler')).toBeNull()
  })

  it('a conflict on the rule shows it as text, not as the draft string', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'Weekly', repeat: weekly })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'daily')
    // 别处把规则改成了每周三 07:30
    act(() => void daemon.editBehindTheBrowser('t', { repeat: { kind: 'weekly', at: '07:30', weekday: 3 } }))
    await act(async () => {
      const { handleTodoChanged } = await import('../service')
      await handleTodoChanged(A, { id: 't', todo: daemon.todos.get('t')!, deleted: false })
    })
    await flush()
    const theirs = await screen.findByTestId('todo-conflict-theirs')
    expect(theirs).toHaveTextContent('Every Wednesday at 07:30')
    expect(theirs).not.toHaveTextContent('weekly:')
  })

  it('a time that is not HH:MM is stopped before anything is sent', async () => {
    const user = userEvent.setup()
    await show({ id: 't', title: 'Plain' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'daily')
    await user.clear(screen.getByTestId('todo-edit-repeat-at'))
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(screen.getByTestId('todo-edit-error')).toBeInTheDocument()
    expect(rpc.calls.some((c) => c.method === 'todo.update')).toBe(false)
  })
})

describe('a copy', () => {
  it('says "from a repeating item" and links to the template', async () => {
    daemon.seed(todoItem({ id: 'tpl', title: 'Weekly review', repeat: weekly }))
    await show({ id: 'copy', title: 'Weekly review', source: { kind: 'repeat', todo_id: 'tpl' } })
    const chip = await screen.findByTestId('todo-repeat-from')
    expect(chip).toHaveAttribute('href', expect.stringContaining('tpl'))
    // 它不是「自动记的」
    expect(screen.queryByTestId('todo-auto')).toBeNull()
  })

  it('when the template is gone the chip stays and says so', async () => {
    await show({ id: 'copy', title: 'Orphan copy', source: { kind: 'repeat', todo_id: 'deleted-tpl' } })
    expect(await screen.findByTestId('todo-repeat-from-deleted')).toBeInTheDocument()
  })

  it('in a list row it is a plain badge, not a link inside the row button', async () => {
    const copy = { ...todoItem({ id: 'copy', title: 'c', source: { kind: 'repeat', todo_id: 'tpl' } }), node_id: A, unsynced: false }
    const h = { onOpen: vi.fn(), onToggleCheck: vi.fn(), onDone: vi.fn(), onReopen: vi.fn(), onDrop: vi.fn(), onDelete: vi.fn() }
    render(<TodoRow item={copy} selected={false} multi={false} checked={false} {...h} />)
    expect(screen.getByTestId('todo-repeat-from-badge')).toBeInTheDocument()
    expect(screen.getByTestId('todo-main').querySelector('a')).toBeNull()
  })

  it('a template row shows the rule and the next time instead of a handler', () => {
    const tpl = { ...todoItem({ id: 'tpl', title: 'Weekly review', repeat: weekly }), node_id: A, unsynced: false }
    const h = { onOpen: vi.fn(), onToggleCheck: vi.fn(), onDone: vi.fn(), onReopen: vi.fn(), onDrop: vi.fn(), onDelete: vi.fn() }
    render(<TodoRow item={tpl} selected={false} multi={false} checked={false} {...h} />)
    expect(screen.getByTestId('todo-main')).toHaveTextContent(/Every Monday at 09:00 · next/)
    expect(screen.queryByTestId('todo-handler')).toBeNull()
  })
})

describe('the history of a template', () => {
  it('shows each copy and each skipped time', async () => {
    rpc.handlers['todo.history'] = () => ({
      events: [
        { seq: 2, ts_ms: 2000, kind: 'repeat_skipped', at_ms: Date.UTC(2030, 0, 14, 9) },
        { seq: 1, ts_ms: 1000, kind: 'repeat_created', todo_id: 'copy-1', at_ms: Date.UTC(2030, 0, 7, 9) },
      ],
      has_more: false,
    })
    await show({ id: 't', title: 'Weekly', repeat: weekly })
    const rows = await screen.findAllByTestId('history-row')
    expect(rows.map((r) => r.getAttribute('data-kind'))).toEqual(['repeat_created', 'repeat_skipped'])
    expect(within(rows[0]!).getByTestId('history-repeat-copy')).toHaveAttribute('href', expect.stringContaining('copy-1'))
    expect(rows[1]).toHaveTextContent(/still open/)
  })
})

describe('the capture form', () => {
  const type = async (text: string): Promise<void> => {
    await userEvent.setup().type(screen.getByTestId('capture-input'), text)
  }

  it('has no repeat control on a node that does not support it', () => {
    setNode(false)
    render(<CaptureForm />)
    expect(screen.queryByTestId('capture-repeat')).toBeNull()
  })

  it('sends the rule with the new item', async () => {
    const user = userEvent.setup()
    render(<CaptureForm />)
    await type('Pay rent')
    await user.selectOptions(screen.getByTestId('capture-repeat-kind'), 'weekdays')
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(1))
    const sent = rpc.calls.find((c) => c.method === 'todo.add')?.params as { repeat?: unknown }
    expect(sent.repeat).toEqual({ kind: 'weekdays', at: '09:00' })
    // 发出之后表单回到「不重复」
    expect(screen.getByTestId('capture-repeat-kind')).toHaveValue('none')
  })

  it('an item without a rule sends no repeat, and with a rule it can still go to the bot (each copy does)', async () => {
    setNode(true, true)
    const user = userEvent.setup()
    render(<CaptureForm />)
    await type('Plain one')
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(1))
    expect('repeat' in ((rpc.calls.find((c) => c.method === 'todo.add')?.params as object) ?? {}) ? (rpc.calls.find((c) => c.method === 'todo.add')?.params as { repeat?: unknown }).repeat : null).toBeNull()
    await type('Bot rule')
    await user.selectOptions(screen.getByTestId('capture-handler'), 'twin')
    await user.selectOptions(screen.getByTestId('capture-repeat-kind'), 'daily')
    expect(screen.getByTestId('capture-handler')).not.toBeDisabled()
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(2))
    const second = rpc.calls.filter((c) => c.method === 'todo.add')[1]?.params as { handler?: string; repeat?: unknown }
    expect(second.handler).toBe('twin')
    expect(second.repeat).toEqual({ kind: 'daily', at: '09:00' })
  })
})
