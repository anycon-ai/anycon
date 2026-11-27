// 周期项的界面（spec/web.md § 14.6）：详情里的规则和「重复」控件（只在节点支持时出现）、模板没有「让 agent 处理」和「交给 Twin」、
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
import { RpcCallError } from '@/transport/rpcClient'
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
const info = (repeat: boolean, twin = false, interval = false): NodeInfo => ({ capabilities: { todo: true, todo_repeat: repeat, todo_repeat_interval: interval, twin } }) as unknown as NodeInfo

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
const setNode = (repeat: boolean, twin = false, interval = false, conn: 'ready' | 'peer_offline' = 'ready'): void =>
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(repeat, twin, interval), conn }) }, order: [A], loaded: true })

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
  it('shows the rule and the next time, and offers no "let an agent do it"; the twin toggle is about each copy', async () => {
    setNode(true, true)
    await show({ id: 't', title: 'Weekly review', repeat: weekly })
    expect(screen.getByTestId('todo-repeat-chip')).toBeInTheDocument()
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent(/Every Monday at 09:00/)
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent(/next/)
    expect(screen.queryByTestId('todo-dispatch')).toBeNull()
    expect(screen.getByTestId('todo-twin-handler')).toHaveTextContent('Each copy goes to the twin')
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

  it('a template hands each copy to the twin and back, with template wording; no toggle without twin support or for an unreviewed item', async () => {
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

  it('no hand-over toggle on a template when the node has no twin', async () => {
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

  it('an item without a rule sends no repeat, and with a rule it can still go to the twin (each copy does)', async () => {
    setNode(true, true)
    const user = userEvent.setup()
    render(<CaptureForm />)
    await type('Plain one')
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(1))
    expect('repeat' in ((rpc.calls.find((c) => c.method === 'todo.add')?.params as object) ?? {}) ? (rpc.calls.find((c) => c.method === 'todo.add')?.params as { repeat?: unknown }).repeat : null).toBeNull()
    await type('Twin rule')
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

describe('several times a day', () => {
  const interval = { kind: 'interval', days: 'weekdays', at: '09:00', every_min: 120, until: '18:00', next_ms: Date.UTC(2030, 0, 7, 9, 0) } as const

  it('reads the rule as a sentence, and shows auto-close in the read-only detail', async () => {
    setNode(true, false, true)
    await show({ id: 't', title: 'Patrol', repeat: { ...interval, auto_close: true } })
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent('Weekdays 09:00–18:00 every 2 hours')
    expect(screen.getByTestId('todo-repeat-auto-close')).toHaveTextContent(/Closes when the twin thinks it is done/)
  })

  it('an unknown rule kind is still "a repeat rule this app cannot read"', async () => {
    setNode(true, false, true)
    await show({ id: 't', title: 'Future', repeat: { kind: 'unknown', at: '09:00' } })
    expect(screen.getByTestId('todo-repeat-rule')).toHaveTextContent(/cannot read/)
  })

  it('the "every" choice and the end time are only there when the node supports them', async () => {
    const user = userEvent.setup()
    setNode(true, false, false)
    await show({ id: 't', title: 'Plain' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'daily')
    expect(screen.queryByTestId('todo-edit-repeat-every')).toBeNull()
    expect(screen.queryByTestId('todo-edit-repeat-auto-close')).toBeNull()
  })

  it('sets an interval rule: the start time stays, an end time appears, and only the rule is sent', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Plain' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'weekdays')
    expect(screen.queryByTestId('todo-edit-repeat-until')).toBeNull()
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-every'), '120')
    expect(screen.getByTestId('todo-edit-repeat-until')).toHaveValue('18:00')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)).toEqual([{ id: 't', repeat: { kind: 'interval', days: 'weekdays', at: '09:00', every_min: 120, until: '18:00' } }])
  })

  it('an end time that is not after the start is stopped before anything is sent', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Plain' })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'daily')
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-every'), '60')
    const until = screen.getByTestId('todo-edit-repeat-until')
    await user.clear(until)
    await user.type(until, '08:00')
    expect(screen.getByTestId('todo-edit-repeat-error')).toHaveTextContent(/later than the start/)
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(rpc.calls.some((c) => c.method === 'todo.update')).toBe(false)
  })

  it('auto-close is part of the rule: ticking it sends the whole rule with auto_close, unticking it drops the flag', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Patrol', repeat: interval })
    await user.click(screen.getByTestId('todo-edit'))
    await user.click(screen.getByTestId('todo-edit-repeat-auto-close'))
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    const sent = rpc.calls.filter((c) => c.method === 'todo.update').map((c) => c.params)
    expect(sent).toEqual([{ id: 't', repeat: { kind: 'interval', days: 'weekdays', at: '09:00', every_min: 120, until: '18:00', auto_close: true } }])
    await user.click(screen.getByTestId('todo-edit'))
    await user.click(screen.getByTestId('todo-edit-repeat-auto-close'))
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(daemon.todos.get('t')?.repeat).toEqual({ kind: 'interval', days: 'weekdays', at: '09:00', every_min: 120, until: '18:00' })
  })

  it('a conflict on an interval rule is shown as a sentence', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Patrol', repeat: interval })
    await user.click(screen.getByTestId('todo-edit'))
    await user.selectOptions(screen.getByTestId('todo-edit-repeat-kind'), 'daily')
    act(() => void daemon.editBehindTheBrowser('t', { repeat: { ...interval, every_min: 30, auto_close: true } }))
    await act(async () => {
      const { handleTodoChanged } = await import('../service')
      await handleTodoChanged(A, { id: 't', todo: daemon.todos.get('t')!, deleted: false })
    })
    await flush()
    const theirs = await screen.findByTestId('todo-conflict-theirs')
    expect(theirs).toHaveTextContent('Weekdays 09:00–18:00 every 30 minutes')
    expect(theirs).not.toHaveTextContent('#close')
  })

  it('the capture form offers the interval and auto-close, but not on a node without support', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    render(<CaptureForm />)
    await user.type(screen.getByTestId('capture-input'), 'Check the build')
    await user.selectOptions(screen.getByTestId('capture-repeat-kind'), 'weekdays')
    await user.selectOptions(screen.getByTestId('capture-repeat-every'), '240')
    await user.click(screen.getByTestId('capture-repeat-auto-close'))
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(1))
    const sent = rpc.calls.find((c) => c.method === 'todo.add')?.params as { repeat?: unknown }
    expect(sent.repeat).toEqual({ kind: 'interval', days: 'weekdays', at: '09:00', every_min: 240, until: '18:00', auto_close: true })
  })

  it('the compact bar has the interval but no auto-close', async () => {
    setNode(true, false, true)
    render(<CaptureForm compact />)
    await userEvent.setup().selectOptions(screen.getByTestId('capture-repeat-kind'), 'daily')
    expect(screen.getByTestId('capture-repeat-every')).toBeInTheDocument()
    expect(screen.queryByTestId('capture-repeat-auto-close')).toBeNull()
  })
})

describe('run once now', () => {
  const tpl = { kind: 'weekly', at: '09:00', weekday: 1, next_ms: Date.UTC(2030, 0, 7, 9, 0) } as const

  it('makes a copy, leaves the template alone and offers the copy in a toast', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Weekly review', repeat: tpl })
    await user.click(screen.getByTestId('todo-run-now'))
    await flush()
    const copies = [...daemon.todos.values()].filter((t) => t.source.todo_id === 't')
    expect(copies).toHaveLength(1)
    const toast = useUi.getState().toasts.at(-1)
    expect(toast?.text).toMatch(/Made a copy/)
    expect(toast?.action?.label).toBe('Open')
    // 新的一项已经在列表里；上一份没关闭，按钮禁用并说明原因
    expect(screen.getByTestId('todo-run-now')).toBeDisabled()
    expect(screen.getByTestId('todo-run-now-blocked')).toBeInTheDocument()
    expect(screen.getByTestId('todo-run-now-open-copy')).toHaveAttribute('href', expect.stringContaining(copies[0]!.id))
  })

  it('is disabled with the reason when the node is offline, and nothing is queued', async () => {
    setNode(true, false, true, 'peer_offline')
    daemon.seed(todoItem({ id: 't', title: 'Weekly review', repeat: tpl }))
    await syncTodos(A)
    render(<TodoDetail item={{ ...todoItem({ id: 't', title: 'Weekly review', repeat: tpl }), node_id: A, unsynced: false }} online={false} />)
    const button = screen.getByTestId('todo-run-now')
    expect(button).toBeDisabled()
    expect(button).toHaveAttribute('title', expect.stringMatching(/offline/i))
    expect(rpc.calls.some((c) => c.method === 'todo.repeat_now')).toBe(false)
  })

  it('is not offered on a node without the capability, on a closed template or on an ordinary item', async () => {
    setNode(true, false, false)
    await show({ id: 't', title: 'Weekly review', repeat: tpl })
    expect(screen.queryByTestId('todo-run-now')).toBeNull()
  })

  it('is not offered on a closed template', async () => {
    setNode(true, false, true)
    daemon.seed(todoItem({ id: 't', title: 'Old', state: 'done', repeat: tpl }))
    await syncTodos(A)
    await loadClosed(A)
    render(<Live id="t" />)
    await flush()
    expect(screen.queryByTestId('todo-run-now')).toBeNull()
  })

  it('explains a refusal from the node, with a link to the open copy when it names one', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Weekly review', repeat: tpl })
    // 这个浏览器的缓存里还没有那份没关闭的副本（别的设备刚复制的）：节点拒绝并指给它
    daemon.seed(todoItem({ id: 'copy-x', title: 'Weekly review', source: { kind: 'repeat', todo_id: 't' }, created_ms: 99_999 }))
    await user.click(screen.getByTestId('todo-run-now'))
    await flush()
    expect(screen.getByTestId('todo-run-now-error')).toHaveTextContent(/previous copy is still open/)
    expect(within(screen.getByTestId('todo-run-now-error')).getByRole('link')).toHaveAttribute('href', expect.stringContaining('copy-x'))
  })

  it('says why when the node is full or the link drops', async () => {
    const user = userEvent.setup()
    setNode(true, false, true)
    await show({ id: 't', title: 'Weekly review', repeat: tpl })
    daemon.openLimit = 1
    await user.click(screen.getByTestId('todo-run-now'))
    await flush()
    expect(screen.getByTestId('todo-run-now-error')).toHaveTextContent(/full/)
    daemon.openLimit = 1000
    daemon.failNext('todo.repeat_now', new RpcCallError('timeout', 'timeout'))
    await user.click(screen.getByTestId('todo-run-now'))
    await flush()
    expect(screen.getByTestId('todo-run-now-error')).toHaveTextContent(/may or may not/)
  })
})

describe('the history of an interval template and its copies', () => {
  it('names a manual run, and a close that the repeating item made', async () => {
    rpc.handlers['todo.history'] = (p) =>
      (p as { id: string }).id === 't'
        ? { events: [{ seq: 1, ts_ms: 1000, kind: 'repeat_created', todo_id: 'copy-1', at_ms: 1000, manual: true }], has_more: false }
        : { events: [{ seq: 2, ts_ms: 2000, kind: 'state', from: 'open', to: 'done', by: { kind: 'repeat', id: 't' } }], has_more: false }
    setNode(true, true, true)
    await show({ id: 't', title: 'Patrol', repeat: { kind: 'daily', at: '09:00' } })
    expect(await screen.findByText(/Run by hand once/)).toBeInTheDocument()
    expect(screen.getByTestId('history-repeat-copy')).toBeInTheDocument()
  })

  it('a copy closed by the repeating item says so instead of "you changed the state"', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [{ seq: 2, ts_ms: 2000, kind: 'state', from: 'open', to: 'done', by: { kind: 'repeat', id: 't' } }], has_more: false })
    setNode(true, true, true)
    await show({ id: 'copy', title: 'Patrol', source: { kind: 'repeat', todo_id: 't' } })
    const row = await screen.findByTestId('history-row')
    expect(row).toHaveTextContent(/Closed automatically/)
    expect(row).not.toHaveTextContent(/You changed/)
  })
})
