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
import { crossesSpace, moveBlock, moveTodo } from './move'
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
    expect(r).toMatchObject({ ok: false, stage: 'link', copied: false })
    expect(log).toEqual(['m:todo.add'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('the delete on the source fails after the target has the item: both copies are kept and the result says so', async () => {
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'stuck' })
    expect(log).toEqual(['m:todo.add', 's:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(view(SRC)).toHaveLength(1)
  })

  it('a source item that is already gone counts as moved (not_found on the delete)', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('not_found'))
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
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

describe('the dialog', () => {
  const open = (item = itemOnSrc()) => {
    const onMoved = vi.fn()
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={item} open onOpenChange={onOpenChange} onMoved={onMoved} />)
    return { onMoved, onOpenChange }
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
    const { onMoved } = open()
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(onMoved).toHaveBeenCalled())
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
    expect(useUi.getState().toasts.some((t) => /Moved to garage/.test(t.text))).toBe(true)
  })

  it('into another space it asks first: the first press only shows the confirmation and sends nothing; the second moves', async () => {
    const user = userEvent.setup()
    expect(crossesSpace(SRC, OTHER)).toBe(true)
    expect(crossesSpace(SRC, SAME)).toBe(false)
    const { onMoved } = open()
    await user.selectOptions(await screen.findByTestId('move-target'), OTHER)
    await user.click(screen.getByTestId('move-confirm'))
    expect(screen.getByTestId('move-confirm-space')).toHaveTextContent('different space (Work)')
    expect(log).toEqual([])
    expect(onMoved).not.toHaveBeenCalled()
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(onMoved).toHaveBeenCalled())
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
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'))
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
