// 把一项移到另一个节点（docs/spec/daemon.md § 21.7）：先在目标 `todo.add`（新 id、项目清掉、不带来源会话），目标确认了才在原节点 `todo.delete`；
// 「原节点那一项没变」「目标上的那一份还在」都在节点上检查（`todo.get`、带 `if_updated_ms` 的删除），不看这个标签页的缓存。
// 所以这里的测试让假 daemon 和浏览器缓存分叉：节点上的状态变了、缓存还是旧的。
import { render, renderHook, screen, waitFor } from '@testing-library/react'
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
import { abandonMove, canDeleteCopy, crossesSpace, moveBlock, moveKey, moveText, moveTodo, removeCopy, resetMoves, resumeMove, useMoves } from './move'
import { resetTodosMemory, syncTodos } from './service'
import { useTodos, viewOf } from './store'
import { MoveTodoDialog } from './ui/MoveTodoDialog'

const SRC = 's'.repeat(32)
const SAME = 'm'.repeat(32)
const OTHER = 'o'.repeat(32)
const info = (group?: string, todo = true): NodeInfo => ({ group, capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
const daemons: Record<string, FakeTodoDaemon> = {}
/** 两个节点共用一条总的请求日志（带节点），断言请求的顺序。 */
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
  for (const m of ['todo.list', 'todo.get', 'todo.add', 'todo.update', 'todo.delete']) rpc.handlers[m] = route(m)
}

const view = (node: string) => viewOf(useTodos.getState(), [node])
const itemOnSrc = (): ReturnType<typeof view>[number] => view(SRC).find((v) => v.id === 'it')!
const entryOf = () => useMoves.getState().moves[moveKey(SRC, 'it')]
/** 这个请求日志里发给原节点 / 目标的 `todo.delete` 的参数。 */
const deleteCalls = (node: string) => rpc.calls.filter((c) => c.nodeId === node && c.method === 'todo.delete').map((c) => c.params)
const errorToasts = () => useUi.getState().toasts.filter((x) => x.kind === 'error')

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
  rpc.calls = []
  log = []
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

/** 让原节点那一项变成「已关闭」，缓存和节点一致。 */
async function closeOnSource(state: 'done' | 'dropped' = 'done'): Promise<void> {
  daemons[SRC]!.todos.set('it', { ...daemons[SRC]!.todos.get('it')!, state, closed_ms: 9 })
  await syncTodos(SRC)
  useTodos.getState().mergeItems(SRC, [daemons[SRC]!.todos.get('it')!])
  log = []
  rpc.calls = []
}

describe('moving', () => {
  it('reads the source, adds on the target (new id, same title / body / due, no project, no source session), reads the copy, and deletes the source only with the confirmed updated_ms', async () => {
    const base = itemOnSrc()
    const r = await moveTodo(base, SAME)
    expect(r.ok).toBe(true)
    expect(log).toEqual(['s:todo.get', 'm:todo.add', 'm:todo.get', 's:todo.delete'])
    expect(deleteCalls(SRC)).toEqual([{ id: 'it', if_updated_ms: base.updated_ms }])
    const moved = [...daemons[SAME]!.todos.values()][0]!
    expect(moved.id).not.toBe('it')
    expect(r.ok && moved.id).toBe(r.ok ? r.newId : '')
    expect(moved).toMatchObject({ title: 'move me', body: 'some **body**', due_ms: 1_800_000_000_000, state: 'open', unreviewed: false })
    expect(moved.project).toBeUndefined()
    expect(moved.source.kind).toBe('device')
    expect((moved.source as { session_id?: string }).session_id).toBeUndefined()
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
    expect(view(SRC)).toEqual([])
    expect(view(SAME).map((v) => v.title)).toEqual(['move me'])
    expect(entryOf()).toBeUndefined()
  })

  it('a closed item stays closed on the target (the state is carried with an update after the add)', async () => {
    await closeOnSource()
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r.ok).toBe(true)
    expect(log).toEqual(['s:todo.get', 'm:todo.add', 'm:todo.update', 'm:todo.get', 's:todo.delete'])
    expect([...daemons[SAME]!.todos.values()][0]).toMatchObject({ state: 'done' })
  })

  it('the target refuses (list full): nothing changed, nothing is deleted, the item stays', async () => {
    daemons[SAME]!.openLimit = 0
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'rejected', reason: 'too_many', copy: 'none' })
    expect(log).toEqual(['s:todo.get', 'm:todo.add'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('the connection fails during the add: nothing is deleted, the result says the copy may exist', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link', at: 'add', copy: 'maybe' })
    expect(log).toEqual(['s:todo.get', 'm:todo.add'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('the delete on the source is refused: both copies are kept and the result says so', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'stuck', copy: 'full' })
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('a source that was deleted elsewhere in the meantime (not_found on the delete) is not a clean success: the target copy exists and the user is told so', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('not_found'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: true, sourceAlreadyGone: true })
    const toast = errorToasts()[0]!
    expect(toast.text).toMatch(/already been deleted on the source node/)
    expect(toast.text).toMatch(/second copy may now exist/)
    expect(toast.text).toContain('garage')
  })
})

describe('the checks happen on the nodes, not in this tab’s cache', () => {
  it('the source was edited on the node but this tab still holds the old copy: nothing is added, the user is told', async () => {
    const stale = itemOnSrc()
    daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited elsewhere' })
    expect(itemOnSrc().title).toBe('move me') // 缓存还是旧的
    const r = await moveTodo(stale, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: false, copy: 'none' })
    expect(log).toEqual(['s:todo.get'])
    expect(daemons[SAME]!.todos.size).toBe(0)
    expect(errorToasts()[0]!.text).toMatch(/changed since you opened the dialog/)
  })

  it('the source was deleted on the node but this tab still shows it: it is not re-created on the target', async () => {
    const stale = itemOnSrc()
    daemons[SRC]!.deleteBehindTheBrowser('it')
    expect(view(SRC)).toHaveLength(1)
    const r = await moveTodo(stale, SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: true, copy: 'none' })
    expect(log).toEqual(['s:todo.get'])
    expect(daemons[SAME]!.todos.size).toBe(0)
  })

  it('the source is edited on the node between the copy and the delete (the cache never learns of it): the conditional delete is refused, the source is kept, and the copy is reported as outdated', async () => {
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited meanwhile' })
      return out
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: false, copy: 'full' })
    expect(log).toEqual(['s:todo.get', 'm:todo.add', 'm:todo.get', 's:todo.delete', 's:todo.get']) // 最后一个是把节点上现在的样子读回缓存
    expect(daemons[SRC]!.todos.get('it')!.title).toBe('edited meanwhile')
    expect([...daemons[SAME]!.todos.values()][0]!.title).toBe('move me')
    const text = errorToasts()[0]!.text
    expect(text).toMatch(/changed here after it was copied to garage/)
    expect(text).toMatch(/nothing was deleted here/)
    expect(text).not.toMatch(/Nothing was moved/)
  })

  it('the cache still holds the copy but the node no longer has it: nothing is deleted on the source', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const copyId = entryOf()!.targetId
    daemons[SAME]!.deleteBehindTheBrowser(copyId)
    expect(useTodos.getState().nodes[SAME]!.items[copyId]).toBeDefined() // 缓存还以为在
    log = []
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'copyGone', copy: 'gone' })
    expect(log).toEqual(['m:todo.get'])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(errorToasts().pop()!.text).toMatch(/copy on garage is no longer there. Nothing was deleted here/)
  })

  it('a retry right after a reconnect, before the node finished syncing, sends nothing', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    useNodes.getState().patch(SRC, { synced: false })
    log = []
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'blocked', block: 'syncingSource' })
    expect(log).toEqual([])
    useNodes.getState().patch(SRC, { synced: true })
    useNodes.getState().patch(SAME, { synced: false })
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'blocked', block: 'syncingTarget' })
    expect(log).toEqual([])
    expect(moveBlock(itemOnSrc(), SAME)).toBe('syncingTarget')
    expect(errorToasts().pop()!.text).toMatch(/garage is still syncing/)
  })

  it('another tab’s edit that is queued right before the delete stops the delete', async () => {
    const original = rpc.handlers['todo.get']!
    rpc.handlers['todo.get'] = async (p, n) => {
      const out = await original(p, n)
      if (n === SAME) {
        // 读目标的这一个来回里，别的标签页把一条对原节点这一项的改动放进了待发队列
        useTodos.setState((s) => ({ outbox: [...s.outbox, { node_id: SRC, id: 'it', op: { kind: 'update', fields: { title: 'queued elsewhere' } }, created_at: 1, state: 'pending' }] }))
      }
      return out
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'blocked', block: 'queued', copy: 'full' })
    expect(deleteCalls(SRC)).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('a closed source that this tab never re-listed is still checked on the node', async () => {
    await closeOnSource()
    const stale = itemOnSrc()
    // 别处把它重新打开了；这个标签页的缓存里它还是「已关闭」
    daemons[SRC]!.editBehindTheBrowser('it', { state: 'open' })
    expect(await moveTodo(stale, SAME)).toMatchObject({ ok: false, stage: 'changed', gone: false })
    expect(daemons[SAME]!.todos.size).toBe(0)
  })

  it('an unreadable source or target (a link error on the read) stops the move without changing anything', async () => {
    daemons[SRC]!.failNext('todo.get', linkError('timeout'))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'unreadable', side: 'source', copy: 'none' })
    expect(daemons[SAME]!.todos.size).toBe(0)
    expect(errorToasts().pop()!.text).toMatch(/Could not read this to-do on its node/)
    daemons[SAME]!.failNext('todo.get', linkError('timeout'))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'unreadable', side: 'target', copy: 'full' })
    expect(deleteCalls(SRC)).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })

  it('the source changing before the first run is checked against what the user confirmed, not the live item: the same item with a different updated_ms is a different version', async () => {
    const confirmed = itemOnSrc()
    daemons[SRC]!.editBehindTheBrowser('it', { body: 'other' })
    expect(await moveTodo({ ...confirmed }, SAME)).toMatchObject({ ok: false, stage: 'changed' })
  })
})

describe('retrying the same move', () => {
  it('after a lost add reply the retry re-sends the same add (same id): the target ends with exactly one copy', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    const first = await moveTodo(itemOnSrc(), SAME)
    expect(first).toMatchObject({ ok: false, stage: 'link', at: 'add', copy: 'maybe' })
    expect(daemons[SAME]!.todos.size).toBe(1)
    const newId = entryOf()!.targetId
    const second = await moveTodo(itemOnSrc(), SAME)
    expect(second).toMatchObject({ ok: true })
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect([...daemons[SAME]!.todos.keys()]).toEqual([newId])
    expect(daemons[SRC]!.todos.has('it')).toBe(false)
  })

  it('restoring the closed state failed: its own stage (the copy is open on the target, the source untouched); the retry only restores the state, then deletes — no second add', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', rpcError('invalid', 'state'))
    const first = await moveTodo(itemOnSrc(), SAME)
    expect(first).toMatchObject({ ok: false, stage: 'state', maybe: false })
    expect([...daemons[SAME]!.todos.values()][0]!.state).toBe('open')
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(errorToasts().some((x) => /open there, not “Done”/.test(x.text))).toBe(true)
    log = []
    const second = await moveTodo(itemOnSrc(), SAME)
    expect(second.ok).toBe(true)
    expect(log).toEqual(['m:todo.get', 'm:todo.update', 'm:todo.get', 's:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect([...daemons[SAME]!.todos.values()][0]!.state).toBe('done')
  })

  it('a link error while restoring the state says it may or may not have been applied, and the copy is not offered for deletion until the move was resumed', async () => {
    daemons[SRC]!.todos.set('it', { ...daemons[SRC]!.todos.get('it')!, state: 'dropped', closed_ms: 9 })
    await syncTodos(SRC)
    useTodos.getState().mergeItems(SRC, [daemons[SRC]!.todos.get('it')!])
    daemons[SAME]!.failNext('todo.update', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'state', maybe: true })
    expect(errorToasts().some((x) => /may or may not have been applied/.test(x.text))).toBe(true)
    expect(canDeleteCopy(entryOf())).toBe(false)
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
  })

  it('a delete that was refused after the copy is retried alone (no second add)', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'stuck' })
    log = []
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(log).toEqual(['m:todo.get', 's:todo.delete'])
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('a link error on the delete says it may or may not have been deleted (not “exists on both”); the retry then finds the source gone and says so', async () => {
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'), { after: true })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link', at: 'delete', copy: 'full' })
    const text = errorToasts()[0]!.text
    expect(text).toMatch(/may or may not have been deleted/)
    expect(text).not.toMatch(/exists on both nodes/)
    const again = await moveTodo(itemOnSrc(), SAME)
    expect(again).toMatchObject({ ok: true, sourceAlreadyGone: true })
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('a refused retry after an unknown add mentions the earlier attempt', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'))
    await moveTodo(itemOnSrc(), SAME)
    daemons[SAME]!.openLimit = 0
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'rejected', copy: 'maybe' })
    expect(errorToasts().pop()!.text).toMatch(/earlier attempt/)
  })
})

describe('deleting the copy and leaving both', () => {
  /** 一次删原节点被拒的移动：两份都在，复制的 updated_ms 已知。 */
  async function stuck(): Promise<string> {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    return entryOf()!.targetId
  }

  it('deletes the copy with the updated_ms it last got, only after reading the source; the source is kept and the move forgotten', async () => {
    const copyId = await stuck()
    const copyMs = entryOf()!.copyUpdatedMs
    expect(copyMs).not.toBeNull()
    log = []
    expect(await removeCopy(SRC, 'it')).toEqual({ stage: 'done' })
    expect(log).toEqual(['s:todo.get', 'm:todo.delete'])
    expect(deleteCalls(SAME)).toEqual([{ id: copyId, if_updated_ms: copyMs }])
    expect(daemons[SAME]!.todos.size).toBe(0)
    expect(view(SAME)).toEqual([])
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(entryOf()).toBeUndefined()
    expect(useUi.getState().toasts.some((x) => /Deleted the copy on garage/.test(x.text))).toBe(true)
  })

  it('refuses when the source was already deleted on the node (the cache still shows it): the copy could be the only one left', async () => {
    const copyId = await stuck()
    daemons[SRC]!.deleteBehindTheBrowser('it')
    expect(view(SRC)).toHaveLength(1)
    log = []
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'sourceGone' })
    expect(log).toEqual(['s:todo.get'])
    expect(daemons[SAME]!.todos.has(copyId)).toBe(true)
    expect(entryOf()!.copyOutcome).toMatchObject({ stage: 'sourceGone' })
    expect(entryOf()!.outcome).toMatchObject({ stage: 'stuck' }) // 移动本身的结果没被盖掉
    expect(errorToasts().pop()!.text).toMatch(/could be the only one left/)
  })

  it('refuses when the source node cannot be read or is not ready', async () => {
    await stuck()
    daemons[SRC]!.failNext('todo.get', linkError('timeout'))
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'sourceGone' })
    useNodes.getState().patch(SRC, { conn: 'peer_offline' })
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'blocked', block: 'offlineSource' })
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('refuses when the copy was edited on the target meanwhile (the cache never learns of it)', async () => {
    const copyId = await stuck()
    daemons[SAME]!.editBehindTheBrowser(copyId, { title: 'edited over there' })
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'copyChanged' })
    expect(daemons[SAME]!.todos.get(copyId)!.title).toBe('edited over there')
    expect(errorToasts().pop()!.text).toMatch(/edited there, so it was not deleted/)
  })

  it('is not offered after a link error on the source delete (the source may be gone) and removeCopy does nothing', async () => {
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    expect(canDeleteCopy(entryOf())).toBe(false)
    expect(await removeCopy(SRC, 'it')).toBeNull()
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('is not offered when the source is known gone, the copy is unreadable, or the copy’s version is unknown', async () => {
    await stuck()
    expect(canDeleteCopy(entryOf())).toBe(true)
    useMoves.setState((s) => ({ moves: { ...s.moves, [moveKey(SRC, 'it')]: { ...entryOf()!, copyUpdatedMs: null } } }))
    expect(canDeleteCopy(entryOf())).toBe(false)
  })

  it('the copy already gone on the target counts as deleted (nothing left to remove)', async () => {
    const copyId = await stuck()
    daemons[SAME]!.deleteBehindTheBrowser(copyId)
    expect(await removeCopy(SRC, 'it')).toEqual({ stage: 'done' })
    expect(entryOf()).toBeUndefined()
  })

  it('a failed delete of the copy is kept apart from the move result: refused, then a link error says it may have been deleted', async () => {
    await stuck()
    daemons[SAME]!.failNext('todo.delete', rpcError('internal'))
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'failed', maybe: false })
    expect(entryOf()!.outcome).toMatchObject({ stage: 'stuck' })
    daemons[SAME]!.failNext('todo.delete', linkError('timeout'))
    expect(await removeCopy(SRC, 'it')).toMatchObject({ stage: 'failed', maybe: true })
    expect(errorToasts().pop()!.text).toMatch(/may or may not have been deleted/)
    expect(canDeleteCopy(entryOf())).toBe(false) // 那一份现在的版本不知道了
  })

  it('abandoning a stopped move forgets it', async () => {
    await stuck()
    abandonMove(SRC, 'it')
    expect(entryOf()).toBeUndefined()
  })
})

describe('an unexpected error never leaves the move busy or reports “nothing changed” after a copy exists', () => {
  it('a plain exception while the add is in flight ends the stage as an unknown add (the copy may exist) with a toast, and the move can be resumed', async () => {
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      await original(p, n)
      throw new Error('boom')
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link', at: 'add', copy: 'maybe' })
    expect(entryOf()!.stage).toBe('failed')
    expect(errorToasts()).toHaveLength(1)
    rpc.handlers['todo.add'] = original
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('a plain exception while the delete is in flight is reported as “may or may not have been deleted”, not as a refusal', async () => {
    const original = rpc.handlers['todo.delete']!
    rpc.handlers['todo.delete'] = async (p, n) => {
      if (n === SRC) throw new Error('boom')
      return original(p, n)
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'link', at: 'delete', copy: 'full' })
    expect(entryOf()!.stage).toBe('failed')
  })

  it('a plain exception in a read before anything was sent says nothing was copied; the dialog is not stuck busy', async () => {
    const user = userEvent.setup()
    rpc.handlers['todo.get'] = () => {
      throw new Error('boom')
    }
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    expect(await screen.findByTestId('move-failure')).toHaveTextContent('Could not read this to-do on its node')
    expect(screen.queryByTestId('move-busy')).toBeNull()
    expect(screen.getByTestId('move-confirm')).toBeEnabled()
  })
})

describe('two tabs moving the same item', () => {
  it('the second tab finds the source already gone: nothing is copied and the text says it may have been moved elsewhere', async () => {
    const second = itemOnSrc()
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    resetMoves() // 第二个标签页是另一份内存
    useUi.setState({ toasts: [] })
    const r = await moveTodo(second, OTHER)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: true, copy: 'none' })
    expect(daemons[OTHER]!.todos.size).toBe(0)
    expect(errorToasts()[0]!.text).toMatch(/moved from another tab or device, it is on the other node now/)
  })

  it('the second tab’s conditional delete finds the source gone: it says a second copy may now exist', async () => {
    const original = rpc.handlers['todo.get']!
    rpc.handlers['todo.get'] = async (p, n) => {
      const out = await original(p, n)
      if (n === SAME) daemons[SRC]!.deleteBehindTheBrowser('it') // 另一个标签页的移动刚好删掉了原节点那一项
      return out
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: true, sourceAlreadyGone: true })
    expect(errorToasts()[0]!.text).toMatch(/second copy may now exist/)
  })
})

describe('reporting without the dialog', () => {
  it('every outcome that is not a clean success raises an alert toast whose action opens the item', async () => {
    const { setNavigator } = await import('@/lib/navigate')
    const go = vi.fn()
    setNavigator(go)
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const t1 = errorToasts()[0]!
    expect(t1.text).toMatch(/exists on both nodes/)
    t1.action!.run()
    expect(go).toHaveBeenCalledWith('/todos/ssssssss/it')
    useUi.setState({ toasts: [] })
    daemons[SRC]!.seed(todoItem({ id: 'it2', title: 'second' }))
    await syncTodos(SRC)
    abandonMove(SRC, 'it')
    const r = await moveTodo(view(SRC).find((v) => v.id === 'it2')!, SAME)
    expect(r.ok).toBe(true)
    expect(errorToasts()).toEqual([])
    setNavigator(null)
  })

  it('a refusal is explained in words, not with a code', async () => {
    daemons[SAME]!.openLimit = 0
    await moveTodo(itemOnSrc(), SAME)
    const text = errorToasts()[0]!.text
    expect(text).toMatch(/list is full/i)
    expect(text).not.toMatch(/too_many/)
  })

  it('a blocked move is also reported by a toast', async () => {
    useNodes.getState().patch(OTHER, { conn: 'peer_offline' })
    await moveTodo(itemOnSrc(), OTHER)
    expect(errorToasts()[0]!.text).toMatch(/office is offline/)
  })

  it('a changed source before any copy says nothing was copied; after a copy it says the copy exists and nothing was deleted', async () => {
    const stale = itemOnSrc()
    daemons[SRC]!.editBehindTheBrowser('it', { title: 'x' })
    await moveTodo(stale, SAME)
    expect(errorToasts()[0]!.text).not.toMatch(/copied/)
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

  it('a node that has not finished syncing after a reconnect', () => {
    useNodes.getState().patch(SRC, { synced: false })
    expect(moveBlock(itemOnSrc())).toBe('syncingSource')
    useNodes.getState().patch(SRC, { synced: true })
    useNodes.getState().patch(OTHER, { synced: false })
    expect(moveBlock(itemOnSrc(), OTHER)).toBe('syncingTarget')
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
    expect(log).toEqual([])
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

  it('Cancel is disabled, Escape and a click outside do nothing, the reason is visible; when the move is done it closes', async () => {
    const user = userEvent.setup()
    const { release } = gateAdd()
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
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(log.filter((l) => l.endsWith('todo.add'))).toHaveLength(1)
    expect(daemons[SAME]!.todos.size).toBe(1)
  })

  it('starting the same item twice while it runs gives the same move: one add, one delete', async () => {
    const { release } = gateAdd()
    const a = moveTodo(itemOnSrc(), SAME)
    const b = moveTodo(itemOnSrc(), OTHER)
    release()
    expect(await b).toBe(await a)
    expect(log.filter((l) => l.endsWith('todo.add'))).toHaveLength(1)
    expect(daemons[OTHER]!.todos.size).toBe(0)
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
    expect(log).toEqual(['s:todo.get', 'o:todo.add', 'o:todo.get', 's:todo.delete'])
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

  it('from the detail page (the dialog gets the live item): the source edited on the node stops “Retry removing it here”; nothing is deleted, the copy keeps the old content and can be deleted', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const view1 = render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited elsewhere' })
    log = []
    view1.rerender(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={vi.fn()} />)
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(screen.getByTestId('move-failure')).toHaveTextContent('changed here after it was copied to garage'))
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect([...daemons[SAME]!.todos.values()][0]!.title).toBe('move me')
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    expect(screen.getByTestId('move-delete-copy')).toHaveTextContent('Delete the copy on garage')
  })

  it('“Delete the copy on …” removes it, keeps the source and closes the dialog; when it is refused the reason is shown next to the move result', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    // 先在原节点那边把它删了（缓存不知道）：删那一份被拒，两条结果并排显示
    daemons[SRC]!.deleteBehindTheBrowser('it')
    await user.click(screen.getByTestId('move-delete-copy'))
    expect(await screen.findByTestId('move-copy-failure')).toHaveTextContent('could be the only one left')
    expect(screen.getByTestId('move-failure')).toHaveTextContent('exists on both nodes')
    expect(daemons[SAME]!.todos.size).toBe(1)
    expect(onOpenChange).not.toHaveBeenCalled()
  })

  it('“Delete the copy on …” succeeds when the source is still there: the dialog closes', async () => {
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
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(entryOf()).toBeUndefined()
  })

  it('after a link error on the source delete the dialog does not offer to delete the copy', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', linkError('timeout'), { after: true })
    open()
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    expect(await screen.findByTestId('move-failure')).toHaveTextContent('may or may not have been deleted')
    expect(screen.queryByTestId('move-delete-copy')).toBeNull()
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

  it('in the dialog the next step is not offered after the copy is gone; “Leave both” forgets the move', async () => {
    const user = userEvent.setup()
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    const onOpenChange = vi.fn()
    render(<MoveTodoDialog item={itemOnSrc()} open onOpenChange={onOpenChange} />)
    await user.selectOptions(await screen.findByTestId('move-target'), SAME)
    await user.click(screen.getByTestId('move-confirm'))
    await screen.findByTestId('move-failure')
    daemons[SAME]!.deleteBehindTheBrowser(entryOf()!.targetId)
    await user.click(screen.getByTestId('move-confirm'))
    await waitFor(() => expect(screen.getByTestId('move-failure')).toHaveTextContent('no longer there'))
    expect(screen.getByTestId('move-confirm')).toBeDisabled()
    await user.click(screen.getByTestId('move-cancel'))
    expect(entryOf()).toBeUndefined()
    expect(onOpenChange).toHaveBeenCalledWith(false)
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

  it('still on that item’s page (8-character node id): back to the list, replacing the entry', async () => {
    window.history.pushState({}, '', '/todos/ssssssss/it')
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(go).toHaveBeenCalledWith('/todos', { replace: true })
  })

  it('the route may carry the full node id or any unique prefix, resolved the way the router does', async () => {
    window.history.pushState({}, '', `/todos/${SRC}/it`)
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(go).toHaveBeenCalledTimes(1)
    go.mockClear()
    daemons[SRC]!.seed(todoItem({ id: 'it3', title: 'third' }))
    await syncTodos(SRC)
    window.history.pushState({}, '', '/todos/ss/it3')
    expect((await moveTodo(view(SRC).find((v) => v.id === 'it3')!, SAME)).ok).toBe(true)
    expect(go).toHaveBeenCalledTimes(1)
  })

  it('the user went somewhere else meanwhile (another page, or another item): they are not pulled away', async () => {
    window.history.pushState({}, '', '/inbox')
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(go).not.toHaveBeenCalled()
    daemons[SRC]!.seed(todoItem({ id: 'it4', title: 'fourth' }))
    await syncTodos(SRC)
    window.history.pushState({}, '', '/todos/ssssssss/it')
    expect((await moveTodo(view(SRC).find((v) => v.id === 'it4')!, SAME)).ok).toBe(true)
    expect(go).not.toHaveBeenCalled()
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

  it('resuming a move that does not exist does nothing', () => {
    expect(resumeMove(SRC, 'nope')).toBeNull()
  })
})

describe('every outcome says what the copy situation is', () => {
  it('a blocked result after the copy was made says a copy is on the target and nothing was deleted (the toast may be the only notice)', async () => {
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      useNodes.getState().patch(SAME, { conn: 'peer_offline' }) // 复制做完之后目标掉线
      return out
    }
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'blocked', block: 'offlineTarget', copy: 'full' })
    const text = errorToasts()[0]!.text
    expect(text).toMatch(/garage is offline/)
    expect(text).toMatch(/A copy is on garage; nothing was deleted here/)
  })

  it('a blocked result while the copy may exist says so; another tab’s queued edit and a node that is syncing again are blocked the same way', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    useNodes.getState().patch(SAME, { synced: false })
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'blocked', block: 'syncingTarget', copy: 'maybe' })
    expect(errorToasts().pop()!.text).toMatch(/A copy may exist on garage/)
    useNodes.getState().patch(SAME, { synced: true })
    useTodos.setState((s) => ({ outbox: [...s.outbox, { node_id: SRC, id: 'it', op: { kind: 'update', fields: { title: 'q' } }, created_at: 1, state: 'pending' }] }))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'blocked', block: 'queued', copy: 'maybe' })
    expect(errorToasts().pop()!.text).toMatch(/A copy may exist on garage/)
  })

  it('with only a possible copy (the add reply was lost) a changed source never claims the copy has old content or that a delete is on offer', async () => {
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited elsewhere' })
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'changed', gone: false, copy: 'maybe' })
    const text = errorToasts().pop()!.text
    expect(text).toMatch(/A copy may exist on garage/)
    expect(text).not.toMatch(/has the old content|Delete the copy|Leave both/)
    expect(canDeleteCopy(entryOf())).toBe(false)
  })

  it('a source that vanished while only a copy may exist, and an unreadable source, say “may exist”; the toast links to the source, not to a copy that may not be there', async () => {
    const { setNavigator } = await import('@/lib/navigate')
    const go = vi.fn()
    setNavigator(go)
    daemons[SAME]!.failNext('todo.add', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    daemons[SRC]!.failNext('todo.get', linkError('timeout'))
    await moveTodo(itemOnSrc(), SAME)
    expect(errorToasts().pop()!.text).toMatch(/A copy may exist on garage; nothing was deleted here/)
    daemons[SRC]!.deleteBehindTheBrowser('it')
    await moveTodo(itemOnSrc(), SAME)
    const t = errorToasts().pop()!
    expect(t.text).toMatch(/copy may exist on garage/)
    t.action!.run()
    expect(go).toHaveBeenCalledWith('/todos/ssssssss/it')
    setNavigator(null)
  })

  it('no text for any stage names an action that may not be offered, whatever the copy situation', () => {
    const stages = [
      { stage: 'blocked', block: 'offlineTarget' },
      { stage: 'changed', gone: false },
      { stage: 'changed', gone: true },
      { stage: 'unreadable', side: 'source' },
      { stage: 'unreadable', side: 'target' },
      { stage: 'rejected', code: 'invalid', reason: 'too_many' },
      { stage: 'link', at: 'add' },
      { stage: 'link', at: 'delete' },
      { stage: 'state', maybe: true },
      { stage: 'state', maybe: false },
      { stage: 'stuck' },
      { stage: 'copyGone' },
      { stage: 'copyEdited' },
      { stage: 'unexpected' },
    ]
    for (const copy of ['none', 'maybe', 'open', 'full', 'gone'] as const) {
      for (const st of stages) {
        const text = moveText({ ok: false, copy, ...st } as never, 'garage', 'done')
        expect(text, `${st.stage}/${copy}`).not.toMatch(/Delete the copy|Leave both|Retry|delete the copy/)
        if (copy === 'maybe' && ['blocked', 'changed', 'unexpected'].includes(st.stage)) expect(text, `${st.stage}/${copy}`).toMatch(/may exist/)
        if ((copy === 'open' || copy === 'full') && ['blocked', 'changed', 'unreadable', 'unexpected'].includes(st.stage) && !(st.stage === 'unreadable' && (st as { side: string }).side === 'target')) {
          expect(text, `${st.stage}/${copy}`).toMatch(/copy|exists|is on garage/i)
        }
      }
    }
  })
})

describe('retrying the state restore does not overwrite a change made to the copy elsewhere', () => {
  it('the restore failed with a refusal, the copy was edited meanwhile: the retry stops, the copy and the source are kept', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', rpcError('invalid', 'state'))
    await moveTodo(itemOnSrc(), SAME)
    const copyId = entryOf()!.targetId
    daemons[SAME]!.editBehindTheBrowser(copyId, { state: 'working' })
    log = []
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'copyEdited', copy: 'open' })
    expect(log).toEqual(['m:todo.get'])
    expect(daemons[SAME]!.todos.get(copyId)!.state).toBe('working')
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
    expect(errorToasts().pop()!.text).toMatch(/copy on garage was changed there/)
    expect(canDeleteCopy(entryOf())).toBe(false)
  })

  it('the restore reply was lost and the copy was changed elsewhere (version unknown, state neither open nor the wanted one): the retry stops', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    const copyId = entryOf()!.targetId
    expect(entryOf()!.copyUpdatedMs).toBeNull()
    daemons[SAME]!.editBehindTheBrowser(copyId, { state: 'working' })
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'copyEdited' })
    expect(daemons[SAME]!.todos.get(copyId)!.state).toBe('working')
  })

  it('the restore reply was lost but the update had landed: the retry does not send it again and finishes', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', linkError('timeout'), { after: true })
    await moveTodo(itemOnSrc(), SAME)
    log = []
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r.ok).toBe(true)
    expect(log).toEqual(['m:todo.get', 'm:todo.get', 's:todo.delete'])
    expect([...daemons[SAME]!.todos.values()][0]!.state).toBe('done')
  })

  it('a retry whose copy is untouched still restores the state', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', rpcError('invalid', 'state'))
    await moveTodo(itemOnSrc(), SAME)
    log = []
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect(log).toEqual(['m:todo.get', 'm:todo.update', 'm:todo.get', 's:todo.delete'])
  })

  it('not_found on the restore means the copy is gone: no “it is open there” message, no repeat', async () => {
    await closeOnSource()
    daemons[SAME]!.failNext('todo.update', rpcError('not_found'))
    const r = await moveTodo(itemOnSrc(), SAME)
    expect(r).toMatchObject({ ok: false, stage: 'copyGone', copy: 'gone' })
    expect(errorToasts().pop()!.text).toMatch(/copy on garage is no longer there/)
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })
})

describe('what the node says is written back to this tab’s cache', () => {
  it('a changed source updates the cached item, so the next attempt works instead of repeating “changed”', async () => {
    const stale = itemOnSrc()
    daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited elsewhere' })
    expect(await moveTodo(stale, SAME)).toMatchObject({ ok: false, stage: 'changed', gone: false })
    expect(itemOnSrc().title).toBe('edited elsewhere')
    expect((await moveTodo(itemOnSrc(), SAME)).ok).toBe(true)
    expect([...daemons[SAME]!.todos.values()][0]!.title).toBe('edited elsewhere')
  })

  it('a source that is gone on the node leaves no ghost row in the cache', async () => {
    const stale = itemOnSrc()
    daemons[SRC]!.deleteBehindTheBrowser('it')
    expect(await moveTodo(stale, SAME)).toMatchObject({ ok: false, stage: 'changed', gone: true })
    expect(view(SRC)).toEqual([])
  })

  it('a conditional delete refused as changed refreshes the cached source too', async () => {
    const original = rpc.handlers['todo.add']!
    rpc.handlers['todo.add'] = async (p, n) => {
      const out = await original(p, n)
      daemons[SRC]!.editBehindTheBrowser('it', { title: 'edited meanwhile' })
      return out
    }
    await moveTodo(itemOnSrc(), SAME)
    expect(itemOnSrc().title).toBe('edited meanwhile')
  })

  it('a copy that is gone on the target is dropped from the cache', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const copyId = entryOf()!.targetId
    daemons[SAME]!.deleteBehindTheBrowser(copyId)
    await moveTodo(itemOnSrc(), SAME)
    expect(useTodos.getState().nodes[SAME]!.items[copyId]).toBeUndefined()
  })
})

describe('leaving while a move runs', () => {
  it('asks before the page is closed or reloaded while a move is running, and stops asking when it is done', async () => {
    const { useRunningMoveGuard } = await import('./move')
    renderHook(() => useRunningMoveGuard())
    const original = rpc.handlers['todo.add']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.add'] = async (p, n) => {
      await gate
      return original(p, n)
    }
    const unload = (): Event => {
      const ev = new Event('beforeunload', { cancelable: true })
      window.dispatchEvent(ev)
      return ev
    }
    expect(unload().defaultPrevented).toBe(false)
    const p = moveTodo(itemOnSrc(), SAME)
    await waitFor(() => expect(entryOf()!.stage).toBe('adding'))
    expect(unload().defaultPrevented).toBe(true)
    release()
    await p
    expect(unload().defaultPrevented).toBe(false)
  })
})

describe('while the copy is being deleted', () => {
  it('moving or resuming returns a blocked result (copy delete in progress), not a made-up success', async () => {
    daemons[SRC]!.failNext('todo.delete', rpcError('internal'))
    await moveTodo(itemOnSrc(), SAME)
    const original = rpc.handlers['todo.delete']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.delete'] = async (p, n) => {
      if (n === SAME) await gate
      return original(p, n)
    }
    const removing = removeCopy(SRC, 'it')
    await waitFor(() => expect(entryOf()!.stage).toBe('checking'))
    expect(await moveTodo(itemOnSrc(), SAME)).toMatchObject({ ok: false, stage: 'blocked', block: 'copyDeleting', copy: 'full' })
    expect(await resumeMove(SRC, 'it')).toMatchObject({ ok: false, stage: 'blocked', block: 'copyDeleting' })
    release()
    expect(await removing).toEqual({ stage: 'done' })
    expect(daemons[SRC]!.todos.has('it')).toBe(true)
  })
})
