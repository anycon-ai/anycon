// 把一项移到另一个节点（docs/spec/daemon.md § 21.7）：先在目标 `todo.add`（新 id、项目清掉、不带来源会话），目标确认了才在原节点 `todo.delete`；
// 中间失败宁可留两份；两个节点都要在线、这一项没有没发出去的改动、没收下的 agent 项和有没存编辑的项不能移；跨空间先确认。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon, linkError } from '@/test/fakeTodoDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { useDrafts } from './drafts'
import { abandonMove, crossesSpace, moveBlock, moveKey, moveTodo, removeCopy, resetMoves, resumeMove, useMoves } from './move'
import { resetTodosMemory, syncTodos } from './service'
import { useTodos, viewOf } from './store'
import { MoveTodoDialog } from './ui/MoveTodoDialog'

const SRC = 's'.repeat(32)
const SAME = 'm'.repeat(32)
const OTHER = 'o'.repeat(32)
const info = (group?: string, todo = true): NodeInfo => ({ group, capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
const daemons: Record<string, FakeTodoDaemon> = {}
/** 两个节点共用一条总的请求日志（带节点），断言「先 add 后 delete」。 */
let log: string[] = []

function install(): void {
  for (const id of [SRC, SAME, OTHER]) {
    const inner = new FakeRpc()
    const d = new FakeTodoDaemon()
    d.install(inner)
    daemons[id] = d
  }
  const route = (method: string) => (p: unknown, nodeId: string) => {
    log.push(`${nodeId[0]}:${method}`)
    return daemons[nodeId]!.routeFor(method)(p, nodeId)
  }
  for (const m of ['todo.list', 'todo.add', 'todo.update', 'todo.delete']) rpc.handlers[m] = route(m)
}

const view = (node: string) => viewOf(useTodos.getState(), [node])
const itemOnSrc = (): ReturnType<typeof view>[number] => view(SRC).find((v) => v.id === 'it')!

beforeEach(async () => {
  await db.resetDb()
  log = []
  rpc = new FakeRpc()
  setRpc(rpc)
  install()
  useNodes.setState({
    byId: {
      [SRC]: nodeEntry({ id: SRC, name: 'house', info: info('Home'), conn: 'ready' }),
      [SAME]: nodeEntry({ id: SAME, name: 'garage', info: info('Home'), conn: 'ready' }),
      [OTHER]: nodeEntry({ id: OTHER, name: 'office', info: info('Work'), conn: 'ready' }),
    },
    order: [SRC, SAME, OTHER],
    loaded: true,
  })
  resetTodosMemory()
  resetMoves()
  useDrafts.setState({ drafts: {} })
  useUi.setState({ toasts: [] })
  daemons[SRC]!.seed(todoItem({ id: 'it', title: 'move me', body: 'some **body**', project: 'api', due_ms: 1_800_000_000_000, source: { kind: 'device', device_id: 'd', session_id: 'sess-1' } as never, updated_ms: 5 }))
  await syncTodos(SRC)
  log = []
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('moving', () => {
  it('adds on the target first (new id, same title / body / due, no project, no source session) and deletes on the source only after the target confirmed', async () => {
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r.ok).toBe(true)
    expect(log).toEqual(['m:todo.add', 's:todo.delete'])
    const moved = [...daemons[SAME]!.todos.values()][0]!
    expect(moved.id).not.toBe('it')
    expect(r.ok && moved.id).toBe(r.ok ? r.newId : '')
    expect(moved).toMatchObject({ title: 'move me', body: 'some **body**', due_ms: 1_800_000_000_000, state: 'open', unreviewed: false })
    expect(moved.project).toBeUndefined()
    expect(moved.source.kind).toBe('device')
    expect((moved.source as { session_id?: string }).session_id).toBeUndefined()
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
    // 界面上：原节点的没了，目标上有了
    expect(view(SRC)).toEqual([])
    expect(view(SAME).map((v) => v.title)).toEqual(['move me'])
  })

  it('a closed item stays closed on the target (the state is carried with an update after the add)', async () => {
    daemons[SRC]!.todos.set('it', { ...daemons[SRC]!.todos.get('it')!, state: 'done', closed_ms: 9 })
    await syncTodos(SRC)
    useTodos.getState().mergeItems(SRC, [daemons[SRC]!.todos.get('it')!])
    log = []
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r.ok).toBe(true)
    expect(log).toEqual(['m:todo.add', 'm:todo.update', 's:todo.delete'])
    expect([...daemons[SAME]!.todos.values()][0]).toMatchObject({ state: 'done' })
  })

  it('the target refuses (list full): nothing changed, no delete is sent, the item stays', async () => {
    daemons[SAME]!.openLimit = 0
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'rejected', reason: 'too_many' })
    expect(log).toEqual(['m:todo.add'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(view(SRC)).toHaveLength(1)
  })

  it('the connection fails during the add: no delete is sent, the item stays on the source, and the result says it may exist on both', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link' })
    expect(log).toEqual(['m:todo.add'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('the delete on the source fails after the target has the item: both copies are kept and the result says so', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'stuck', copy: 'full' })
    expect(log).toEqual(['m:todo.add', 's:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(view(SRC)).toHaveLength(1)
  })

  it('a source that was deleted elsewhere in the meantime (not_found on the delete) is not a clean success: the target copy exists and the user is told so', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('not_found'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: true, sourceAlreadyGone: true })
    const toast = useUi.getState().toasts.find((x) => x.kind === 'error')!
    expect(toast.text).toMatch(/already been deleted on the source node/)
    expect(toast.text).toContain('garage')
  })
})

describe('a stale copy is never moved', () => {
  it('the item was edited on another device after the dialog was opened: nothing is sent and the user is told to check it and move again', async () => {
    const stale = itemOnSrc()
    useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, title: 'edited elsewhere', updated_ms: 99 }])
    log = []
    const r = await moveTodo(stale, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: false })
    expect(log).toEqual([])
    expect(useUi.getState().toasts.find((x) => x.kind === 'error')!.text).toMatch(/changed since you opened the dialog/)
  })

  it('the item was deleted elsewhere after the dialog was opened: it is not re-created on the target', async () => {
    const stale = itemOnSrc()
    useTodos.getState().applyChanged(SRC, { id: 'it', deleted: true })
    log = []
    const r = await moveTodo(stale, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: true })
    expect(log).toEqual([])
    expect(daemons[SAME]!.todos.size).toBe(0)
  })

  it('the source changing between the copy and the delete keeps the source (both copies exist) instead of deleting the newer edit', async () => {
    const item = itemOnSrc()
    // add 完成之后、删除之前，原节点那一项在别处变了
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, title: 'edited meanwhile', updated_ms: 77 }])
      return out
    }
    const r = await moveTodo(item, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: false, copy: 'full' })
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(log.filter((l) => l.endsWith('todo.delete'))).toEqual([])
  })
})

describe('retrying the same move', () => {
  it('after a lost add reply the retry re-sends the same add (same id): the target ends with exactly one copy', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    const first = await moveTodo(itemOnSrc(), SAME)
    expect(first).toMatchObject({ ok: false, stage: 'link', at: 'add', copy: 'maybe' })
    expect(daemons[SAME]!.todos.size).toBe(1)
    const newId = useMoves.getState().moves[moveKey(SRC, 'it')]!.targetId
    const second = await moveTodo(itemOnSrc(), SAME)
    expect(second).toMatchObject({ ok: true })
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect([...daemons[SAME]!.todos.keys()]).toEqual([newId])
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
  })

  it('restoring the closed state failed: its own stage (the copy is open on the target, the source untouched); the retry only restores the state, then deletes — no second add', async () => {
    daemons[SRC]!.todos.set('it', { ...daemons[SRC]!.todos.get('it')!, state: 'done', closed_ms: 9 })
    await syncTodos(SRC)
    useTodos.getState().mergeItems(SRC, [daemons[SRC]!.todos.get('it')!])
    daemons[SAME]!.failNext('todo.update', rpcError('invalid', 'state'))
    const first = await moveTodo(itemOnSrc(), SAME)
    expect(first).toMatchObject({ ok: false, stage: 'state', maybe: false })
    expect([...daemons[SAME]!.todos.values()][0]!.state).toBe('open')
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(useUi.getState().toasts.some((x) => /open there, not “Done”/.test(x.text))).toBe(true)
    log = []
    const second = await moveTodo(itemOnSrc(), SAME)
    expect(second.ok).toBe(true)
    expect(log).toEqual(['m:todo.update', 's:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect([...daemons[SAME]!.todos.values()][0]!.state).toBe('done')
  })

  it('a link error while restoring the state says it may or may not have been applied', async () => {
    daemons[SRC]!.todos.set('it', { ...daemons[SRC]!.todos.get('it')!, state: 'dropped', closed_ms: 9 })
    useTodos.getState().mergeItems(SRC, [daemons[SRC]!.todos.get('it')!])
    daemons[SAME]!.failNext('todo.update', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'state', maybe: true })
    expect(useUi.getState().toasts.some((x) => /may or may not have been applied/.test(x.text))).toBe(true)
  })

  it('a delete that failed after the copy is retried alone (no second add)', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'stuck' })
    log = []
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(log).toEqual(['s:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
  })
})

describe('reporting without the dialog', () => {
  it('every outcome that is not a clean success raises an alert toast whose action opens the item', async () => {
    const { setNavigator } = await import('@/lib/navigate')
    const go = vi.fn()
    setNavigator(go)
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const t1 = useUi.getState().toasts.find((x) => x.kind === 'error')!
    expect(t1.text).toMatch(/exists on both nodes/)
    t1.action!.run()
    expect(go).toHaveBeenCalledWith('/todos/ssssssss/it')
    // 干净的成功只有一条普通提示（对话框里），这里没有错误提示
    useUi.setState({ toasts: [] })
    daemons[SRC]!.seed(todoItem({ id: 'it2', title: 'second' }))
    await syncTodos(SRC)
    log = []
    const r = await moveTodo(view(SRC).find((v) => v.id === 'it2')!, SAME)
    expect(r.ok).toBe(true)
    expect(useUi.getState().toasts.filter((x) => x.kind === 'error')).toEqual([])
    setNavigator(null)
  })

  it('a refusal is explained in words, not with a code', async () => {
    daemons[SAME]!.openLimit = 0
    await moveTodo(itemOnSrc(), SAME)
    const text = useUi.getState().toasts.find((x) => x.kind === 'error')!.text
    expect(text).toMatch(/list is full/i)
    expect(text).not.toMatch(/too_many/)
  })
})

describe('when it cannot be moved', () => {
  it('either node offline', async () => {
    useNodes.getState().patch(OTHER, { conn: 'peer_offline' })
    expect(moveBlock(itemOnSrc(), OTHER)).toBe('offlineTarget')
    expect(await moveTodo(itemOnSrc(), OTHER)).toMatchObject({ ok: false, stage: 'blocked', block: 'offlineTarget' })
    useNodes.getState().patch(SRC, { conn: 'peer_offline' })
    expect(moveBlock(itemOnSrc())).toBe('offlineSource')
    expect(log).toEqual([])
  })

  it('a target that does not support to-dos', () => {
    useNodes.getState().setInfo(OTHER, info('Work', false))
    expect(moveBlock(itemOnSrc(), OTHER)).toBe('unsupported')
  })

  it('the item has queued changes: not routed through the offline outbox, so the source can never be deleted before the target has the item', async () => {
    rpc.ready = false
    useNodes.getState().patch(SRC, { conn: 'peer_offline' })
    const { updateTodo } = await import('./service')
    await updateTodo(SRC, 'it', { title: 'edited offline' })
    rpc.ready = true
    useNodes.getState().patch(SRC, { conn: 'ready' })
    expect(view(SRC)[0]!.unsynced).toBe(true)
    expect(moveBlock(view(SRC)[0]!, SAME)).toBe('queued')
    log = []
    expect(await moveTodo(view(SRC)[0]!, SAME)).toMatchObject({ stage: 'blocked', block: 'queued' })
    expect(log.filter((l) => l.endsWith('todo.add') || l.endsWith('todo.delete'))).toEqual([])
  })

  it('an unreviewed agent-added item (it would lose its agent marking)', () => {
    const agent = { ...itemOnSrc(), unreviewed: true, source: { kind: 'agent' } } as ReturnType<typeof view>[number]
    expect(moveBlock(agent, SAME)).toBe('unreviewed')
  })

  it('an item with an open edit draft', () => {
    const key = `${SRC}:it`
    useDrafts.getState().start(key, { title: 'move me', body: '', project: '', due: '' })
    useDrafts.getState().set(key, 'title', 'half edited')
    expect(moveBlock(itemOnSrc(), SAME)).toBe('draft')
  })
})

describe('the dialog stays open while a move is running', () => {
  it('Cancel is disabled, Escape and a click outside do nothing, the reason is visible; when the move is done it closes', async () => {
    const user = userEvent.setup()
    const original = rpc.handlers['todo.add']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.add'] = async (p, n) => {
      await gate
      return original(p, n)
    }
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    expect(await screen.findByTestId('move-busy')).toHaveTextContent('stays open')
    expect(screen.getByTestId('move-cancel')).toBeDisabled()
    await user.keyboard('{Escape}')
    expect(onOpenChange).not.toHaveBeenCalled()
    release()
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
  })

  it('after a half-done move only the step that can continue is offered, the target cannot be changed, and “Leave both” closes', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    expect(await screen.findByTestId('move-failure')).toHaveTextContent('exists on both nodes')
    expect(screen.getByTestId('move-confirm')).toHaveTextContent('Retry removing it here')
    expect(screen.getByTestId('move-target')).toBeDisabled()
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(daemons[SRC]!.todos.has('it')).toBe(false))
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect(onOpenChange).not.toHaveBeenCalledWith(true)
  })
})

describe('the dialog', () => {
  const open = (item = itemOnSrc()) => {
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={item} open onOpenChange={onOpenChange} />)
    return { onOpenChange }
  }

  it('lists the other nodes that support to-dos grouped by space, and notes that the project is not carried over', async () => {
    open()
    const select = await screen.findByTestId('move-target')
    const groups = [...select.querySelectorAll('optgroup')].map((g) => [g.getAttribute('label'), [...g.querySelectorAll('option')].map((o) => o.textContent)])
    expect(groups).toEqual([
      ['Home', ['garage']],
      ['Work', ['office']],
    ])
    expect(screen.getByText(/project is not carried over/i)).toBeInTheDocument()
  })

  it('within the same space it moves straight away; the item is gone from the source and a toast says where it went', async () => {
    const user = userEvent.setup()
    const { onOpenChange } = open()
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
    expect(useUi.getState().toasts.some((t) => /Moved to garage/.test(t.text))).toBe(true)
  })

  it('into another space it asks first: the first press only shows the confirmation and sends nothing; the second moves', async () => {
    const user = userEvent.setup()
    expect(crossesSpace(SRC, OTHER)).toBe(true)
    expect(crossesSpace(SRC, SAME)).toBe(false)
    const { onOpenChange } = open()
    await user.selectOptions(await screen.findByTestId('move-target'), OTHER)
    await user.click(screen.getByTestId('move-confirm'))
    expect(screen.getByTestId('move-confirm-space')).toHaveTextContent('different space (Work)')
    expect(log).toEqual([])
    expect(onOpenChange).not.toHaveBeenCalled()
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(log).toEqual(['o:todo.add', 's:todo.delete'])
  })

  it('says exactly why it cannot be moved now, and what happened when it failed halfway', async () => {
    const user = userEvent.setup()
    useNodes.getState().patch(OTHER, { conn: 'peer_offline' })
    open()
    await user.selectOptions(await screen.findByTestId('move-target'), OTHER)
    expect(screen.getByTestId('move-blocked')).toHaveTextContent('office is offline')
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    await user.selectOptions(screen.getByTestId('move-target'), SAME)
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await user.click(screen.getByTestId('move-confirm'))
    expect(await screen.findByTestId('move-failure')).toHaveTextContent('exists on both nodes')
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('tells that an accepted agent-added item becomes the user’s own on the target', async () => {
    const agentAccepted = { ...itemOnSrc(), source: { kind: 'agent' }, unreviewed: false } as ReturnType<typeof view>[number]
    open(agentAccepted)
    expect(await screen.findByText(/recorded as added by you, not by an agent/i)).toBeInTheDocument()
  })

  it('an unreviewed agent item and an item with unsent changes show the reason instead of the target list', async () => {
    const agent = { ...itemOnSrc(), unreviewed: true, source: { kind: 'agent' } } as ReturnType<typeof view>[number]
    open(agent)
    expect(await screen.findByTestId('move-blocked')).toHaveTextContent('Accept it first')
    expect(screen.queryByTestId('move-target')).toBeNull()
  })
})

describe('entry points', () => {
  it('the detail has “Move to another node…” only when another node supports to-dos', async () => {
    const { TodoDetail } = await import('./ui/TodoDetail')
    const item = itemOnSrc()
    const first = render(<TodoDetail item={item} online />)
    expect(screen.getByTestId('todo-move')).toHaveTextContent('Move to another node…')
    first.unmount()
    useNodes.getState().setInfo(SAME, info('Home', false))
    useNodes.getState().setInfo(OTHER, info('Work', false))
    render(<TodoDetail item={item} online />)
    expect(screen.queryByTestId('todo-move')).toBeNull()
  })
})

describe('one move per item, kept outside the dialog', () => {
  const gateAdd = (): { release: () => void } => {
    const original = rpc.handlers['todo.add']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.add'] = async (p, n) => {
      await gate
      return original(p, n)
    }
    return { release }
  }

  it('starting the same item twice while it runs gives the same move: one add, one delete', async () => {
    const { release } = gateAdd()
    const a = moveTodo(itemOnSrc(), SAME)
    const b = moveTodo(itemOnSrc(), OTHER)
    release()
    expect(await b).toBe(await a)
    expect(log.filter((l) => l.endsWith('todo.add'))).toHaveLength(1)
    expect(daemons[OTHER]!.todos.size).toBe(0)
  })

  it('closing and reopening the dialog mid-move shows the running move: busy, target fixed, no second move can be started', async () => {
    const user = userEvent.setup()
    const { release } = gateAdd()
    const first = render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-busy')
    first.unmount()
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    expect(await screen.findByTestId('move-busy')).toBeInTheDocument()
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    expect(screen.getByTestId('move-target')).toHaveValue(SAME)
    release()
    // 重新挂载的对话框也知道这次移动结束了，自己关上
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(log.filter((l) => l.endsWith('todo.add'))).toHaveLength(1)
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('a half-done move is still there after the dialog was unmounted; reopening offers the step that can continue', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const first = render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    first.unmount()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    expect(await screen.findByTestId('move-failure')).toHaveTextContent('exists on both nodes')
    expect(screen.getByTestId('move-confirm')).toHaveTextContent('Retry removing it here')
    expect(screen.getByTestId('move-target')).toBeDisabled()
  })
})

describe('the source is checked against what was confirmed, before every later step', () => {
  it('from the detail page (the dialog gets the live item): editing the source after a half-done move stops “Retry removing it here”; nothing is deleted and the copy keeps the old content', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const view1 = render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    // 别的设备改了原节点这一项；详情页把「现在的」这一份传给对话框
    useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, title: 'edited elsewhere', updated_ms: 99 }])
    log = []
    view1.rerender(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(screen.getByTestId('move-failure')).toHaveTextContent('changed here after it was copied to garage'))
    expect(log).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect([...daemons[SAME]!.todos.values()][0]!.title).toBe('move me')
    // 不能再接着移；可以删掉那一份旧的
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    expect(screen.getByTestId('move-delete-copy')).toHaveTextContent('Delete the copy on garage')
  })

  it('the first run copies what the user confirmed, not what the item became in the meantime', async () => {
    const confirmed = itemOnSrc()
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, title: 'edited while copying', updated_ms: 50 }])
      return out
    }
    const r = await moveTodo(confirmed, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', copy: 'full' })
    expect([...daemons[SAME]!.todos.values()][0]!.title).toBe('move me')
    expect(log.filter((l) => l.endsWith('todo.delete'))).toEqual([])
  })

  it('the source disappeared from the cache after the copy: stop, say so, delete nothing', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    useTodos.getState().applyChanged(SRC, { id: 'it', deleted: true })
    log = []
    const r = resumeMove(SRC, 'it')
    expect(await r).toMatchObject({ ok: false, stage: 'changed', gone: true, copy: 'full' })
    expect(log).toEqual([])
    expect(useUi.getState().toasts.filter((x) => x.kind === 'error').pop()!.text).toMatch(/no longer here/)
  })
})

describe('the copy must still exist before the source is deleted', () => {
  it('the copy was deleted after the failed delete: retrying does not delete the source (that would leave neither), and says so', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const id = useMoves.getState().moves[moveKey(SRC, 'it')]!.targetId
    useTodos.getState().applyChanged(SAME, { id, deleted: true })
    log = []
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'copyGone', copy: 'gone' })
    expect(log).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(useUi.getState().toasts.some((x) => /copy on garage is no longer there. Nothing was deleted here/.test(x.text))).toBe(true)
  })

  it('a target whose list is not loaded cannot confirm the copy: nothing is deleted', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    useTodos.setState((s) => ({ nodes: { ...s.nodes, [SAME]: { ...s.nodes[SAME]!, items: {} } } }))
    log = []
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'copyGone' })
    expect(log).toEqual([])
  })

  it('in the dialog the next step is not offered; “Leave both” forgets the move', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    const id = useMoves.getState().moves[moveKey(SRC, 'it')]!.targetId
    useTodos.getState().applyChanged(SAME, { id, deleted: true })
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(screen.getByTestId('move-failure')).toHaveTextContent('no longer there'))
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    await user.click(screen.getByTestId('move-cancel'))
    expect(useMoves.getState().moves[moveKey(SRC, 'it')]).toBeUndefined()
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })
})

describe('every failure says what exists where', () => {
  it('a link error on the delete is not reported as “exists on both”: it may or may not have been deleted here', async () => {
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link', at: 'delete', copy: 'full' })
    const text = useUi.getState().toasts.find((x) => x.kind === 'error')!.text
    expect(text).toMatch(/may or may not have been deleted/)
    expect(text).not.toMatch(/exists on both nodes/)
    // 删其实成功了：再试一次，原节点那一项已经没了，报告成「已经被删了」，不再重复建一份
    const again = await moveTodo(itemOnSrc(), SAME)
    expect(again).toMatchObject({ ok: true, sourceAlreadyGone: true })
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('a changed source before any copy says nothing was moved; after a copy it says the copy exists and nothing was deleted', async () => {
    const stale = itemOnSrc()
    useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, updated_ms: 99 }])
    await moveTodo(stale, SAME)
    expect(useUi.getState().toasts.find((x) => x.kind === 'error')!.text).not.toMatch(/copied/)
    useUi.setState({ toasts: [] })
    resetMoves()
    const fresh = itemOnSrc()
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      useTodos.getState().mergeItems(SRC, [{ ...daemons[SRC]!.todos.get('it')!, updated_ms: 120 }])
      return out
    }
    await moveTodo(fresh, SAME)
    const text = useUi.getState().toasts.find((x) => x.kind === 'error')!.text
    expect(text).toMatch(/copied to garage/)
    expect(text).toMatch(/nothing was deleted here/)
    expect(text).not.toMatch(/Nothing was moved/)
  })

  it('a refused retry after an unknown add mentions the earlier attempt', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'))
    await moveTodo(itemOnSrc(), SAME)
    daemons[SAME]!.openLimit = 0
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'rejected', copy: 'maybe' })
    expect(useUi.getState().toasts.filter((x) => x.kind === 'error').pop()!.text).toMatch(/earlier attempt/)
  })

  it('a blocked move is also reported by a toast', async () => {
    useNodes.getState().patch(OTHER, { conn: 'peer_offline' })
    await moveTodo(itemOnSrc(), OTHER)
    expect(useUi.getState().toasts.find((x) => x.kind === 'error')!.text).toMatch(/office is offline/)
  })

  it('after a resume was blocked the button says “Resume the move”, not “Move”', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    useNodes.getState().patch(SAME, { conn: 'peer_offline' })
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(screen.getByTestId('move-failure')).toHaveTextContent('offline'))
    expect(screen.getByTestId('move-confirm')).toHaveTextContent('Resume the move')
    expect(screen.getByTestId('move-confirm')).toBeEnabled()
  })
})

describe('deleting the copy and leaving both', () => {
  it('“Delete the copy on …” removes the target copy, keeps the source and forgets the move; the dialog closes', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    await user.click(screen.getByTestId('move-delete-copy'))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(daemons[SAME]!.todos.size).toBe(0)
    expect(view(SAME)).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(useMoves.getState().moves[moveKey(SRC, 'it')]).toBeUndefined()
    expect(useUi.getState().toasts.some((x) => /Deleted the copy on garage/.test(x.text))).toBe(true)
  })

  it('a failed delete of the copy keeps the move and says the copy is still there; a link error says it may have been deleted', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    daemons[SAME]!.failNext('todo.delete', rpcError('internal'))
    expect(await removeCopy(SRC, 'it')).toMatchObject({ ok: false, stage: 'copyDelete', maybe: false })
    expect(useMoves.getState().moves[moveKey(SRC, 'it')]).toBeDefined()
    daemons[SAME]!.failNext('todo.delete', linkError('timeout'))
    expect(await removeCopy(SRC, 'it')).toMatchObject({ ok: false, stage: 'copyDelete', maybe: true })
    expect(useUi.getState().toasts.filter((x) => x.kind === 'error').pop()!.text).toMatch(/may or may not have been deleted/)
  })

  it('abandoning a stopped move forgets it', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    abandonMove(SRC, 'it')
    expect(useMoves.getState().moves[moveKey(SRC, 'it')]).toBeUndefined()
  })
})

describe('after a successful move', () => {
  const go = vi.fn()
  beforeEach(async () => {
    const { setNavigator } = await import('@/lib/navigate')
    go.mockClear()
    setNavigator(go)
  })
  afterEach(async () => {
    const { setNavigator } = await import('@/lib/navigate')
    setNavigator(null)
    window.history.pushState({}, '', '/')
  })

  it('still on that item’s page: back to the list, replacing the entry', async () => {
    window.history.pushState({}, '', '/todos/ssssssss/it')
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(go).toHaveBeenCalledWith('/todos', { replace: true })
  })

  it('the user went somewhere else meanwhile: they are not pulled away', async () => {
    window.history.pushState({}, '', '/inbox')
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(go).not.toHaveBeenCalled()
  })
})
