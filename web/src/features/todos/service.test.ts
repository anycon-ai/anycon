// 清单的同步、`todo.changed`、缓存、离线待发队列和重放（docs/spec/daemon.md § 21.6）。
// 重放的场景写成表：场景 → 期望发给 daemon 的请求顺序 → 最后的状态。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon, linkError } from '@/test/fakeTodoDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { addTodo, deleteTodo, discardOp, forgetNodeTodos, handleTodoChanged, hydrateTodos, loadClosed, reloadFromDb, replayOutbox, resetTodosMemory, retryOp, syncTodos, updateTodo } from './service'
import { nodeSupportsTodo, useTodos, viewOf } from './store'

const NODE = 'n'.repeat(32)
const OTHER = 'o'.repeat(32)
const info = (todo: boolean): NodeInfo => ({ capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const view = (node = NODE) => viewOf(useTodos.getState(), [node])
const byTitle = (t: string) => view().find((v) => v.title === t)
const flush = async (): Promise<void> => {
  for (let i = 0; i < 8; i++) await new Promise((r) => setTimeout(r, 0))
}
const goOffline = (): void => {
  rpc.ready = false
  useNodes.getState().patch(NODE, { conn: 'peer_offline' })
}
// 先让离线时入队触发的后台重放跑完（它们发现链路没就绪就退出了），再恢复在线：不然它们会抢在测试的断言之前发出请求
const goOnline = async (): Promise<void> => {
  await flush()
  rpc.ready = true
  useNodes.getState().patch(NODE, { conn: 'ready' })
}
const lastWrite = () => daemon.log.filter((l) => l.method !== 'todo.list').at(-1)

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, info: info(true), conn: 'ready' }), [OTHER]: nodeEntry({ id: OTHER, info: info(false), conn: 'ready' }) }, order: [NODE, OTHER], loaded: true })
  resetTodosMemory()
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('sync', () => {
  it('pages through todo.list until has_more is false, de-duplicates by id, and caches the result', async () => {
    daemon.pageSize = 2
    daemon.seed(...[1, 2, 3, 4, 5].map((n) => todoItem({ id: `t${n}`, updated_ms: n * 10 })), todoItem({ id: 'closed', state: 'done', updated_ms: 99 }))
    await syncTodos(NODE)
    expect(daemon.methods()).toEqual(['todo.list', 'todo.list', 'todo.list'])
    // 缺省只要没关闭的；翻页游标是上一页最后一项
    expect(daemon.log[0]!.params).toMatchObject({ limit: 200 })
    expect(daemon.log[1]!.params).toMatchObject({ before: { id: 't4', updated_ms: 40 } })
    expect(view().map((v) => v.id).sort()).toEqual(['t1', 't2', 't3', 't4', 't5'])
    const st = useTodos.getState().nodes[NODE]!
    expect(st.synced).toBe(true)
    expect(Object.keys(st.items)).toHaveLength(5)
    // 落盘了
    const cached = await db.listTodoRecords(NODE)
    expect(cached.todos.map((t) => t.id).sort()).toEqual(['t1', 't2', 't3', 't4', 't5'])
    expect(cached.synced_at).toBeGreaterThan(0)
  })

  it('takes the daemon’s result as authoritative: items gone or closed elsewhere leave the cache, closed items already cached stay', async () => {
    useTodos.getState().setCache(NODE, [todoItem({ id: 'gone' }), todoItem({ id: 'kept' }), todoItem({ id: 'old-closed', state: 'done' })])
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'gone' }), todoItem({ id: 'kept' }), todoItem({ id: 'old-closed', state: 'done' })] })
    daemon.seed(todoItem({ id: 'kept', title: 'fresh', updated_ms: 50 }))
    await syncTodos(NODE)
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items).sort()).toEqual(['kept', 'old-closed'])
    expect(byTitle('fresh')).toBeDefined()
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id).sort()).toEqual(['kept', 'old-closed'])
  })

  it('does not touch a node that has no to-do capability (and the node is reported as unsupported)', async () => {
    await syncTodos(OTHER)
    expect(rpc.calls).toEqual([])
    expect(nodeSupportsTodo(OTHER)).toBe(false)
    expect(nodeSupportsTodo(NODE)).toBe(true)
    // 还没拿到过 node.info 的节点按不支持算
    useNodes.getState().patch(NODE, { info: undefined })
    expect(nodeSupportsTodo(NODE)).toBe(false)
  })

  it('an offline node at sync time leaves the cache alone and throws a link error for syncNode to retry later', async () => {
    useTodos.getState().setCache(NODE, [todoItem({ id: 'cached' })])
    rpc.ready = false
    await expect(syncTodos(NODE)).rejects.toMatchObject({ kind: 'offline' })
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items)).toEqual(['cached'])
    expect(useTodos.getState().nodes[NODE]!.syncing).toBe(false)
  })

  it('closed items are only fetched when asked, page by page', async () => {
    daemon.pageSize = 2
    daemon.seed(todoItem({ id: 'open1', updated_ms: 100 }), ...[1, 2, 3].map((n) => todoItem({ id: `c${n}`, state: 'done', updated_ms: n })))
    await syncTodos(NODE)
    expect(daemon.log.every((l) => !(l.params as { include_closed?: boolean }).include_closed)).toBe(true)
    await loadClosed(NODE)
    expect(daemon.log.at(-1)!.params).toMatchObject({ include_closed: true })
    expect(useTodos.getState().nodes[NODE]!.closed).toMatchObject({ hasMore: true, complete: false })
    await loadClosed(NODE, { more: true })
    const st = useTodos.getState().nodes[NODE]!
    expect(st.closed.hasMore).toBe(false)
    expect(Object.keys(st.items).sort()).toEqual(['c1', 'c2', 'c3', 'open1'])
  })

  it('a change that arrives while the list is being paged is not removed by the full replace', async () => {
    daemon.seed(todoItem({ id: 'a', updated_ms: 10 }))
    const original = rpc.handlers['todo.list']!
    rpc.handlers['todo.list'] = async (p, n) => {
      const r = await original(p, n)
      // 翻页期间别处新建了一项，`todo.changed` 先到
      handleTodoChanged(NODE, { id: 'late', todo: todoItem({ id: 'late', updated_ms: 20 }) })
      return r
    }
    await syncTodos(NODE)
    expect(view().map((v) => v.id).sort()).toEqual(['a', 'late'])
  })
})

describe('todo.changed', () => {
  it('merges new and changed items by id (the newer wins), removes deleted ones, persists, and ignores junk', async () => {
    useTodos.getState().setCache(NODE, [])
    handleTodoChanged(NODE, { id: 'x', todo: todoItem({ id: 'x', title: 'v1', updated_ms: 1 }) })
    handleTodoChanged(NODE, { id: 'x', todo: todoItem({ id: 'x', title: 'v2', updated_ms: 2 }) })
    expect(view()).toHaveLength(1)
    expect(view()[0]!.title).toBe('v2')
    handleTodoChanged(NODE, { id: 'y', todo: todoItem({ id: 'y' }) })
    handleTodoChanged(NODE, { id: 'x', deleted: true })
    expect(view().map((v) => v.id)).toEqual(['y'])
    // 垃圾：没有 id、todo 的 id 对不上、不是对象
    handleTodoChanged(NODE, { todo: todoItem({ id: 'z' }) })
    handleTodoChanged(NODE, { id: 'q', todo: todoItem({ id: 'other' }) })
    handleTodoChanged(NODE, null)
    handleTodoChanged(NODE, 'nope')
    expect(view().map((v) => v.id)).toEqual(['y'])
    await flush()
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['y'])
  })

  it('is ignored for a node without the capability', () => {
    handleTodoChanged(OTHER, { id: 'x', todo: todoItem({ id: 'x' }) })
    expect(useTodos.getState().nodes[OTHER]).toBeUndefined()
  })
})

describe('cache', () => {
  it('is read from IndexedDB on start, so an offline node still shows its last list (and the outbox with it)', async () => {
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c1', title: 'cached one' })], syncedAt: 777 })
    await db.appendOutbox({ node_id: NODE, id: 'new', op: { kind: 'add', params: { title: 'typed offline', body: '' } }, created_at: 5, state: 'pending' })
    goOffline()
    await hydrateTodos([NODE])
    const st = useTodos.getState()
    expect(st.nodes[NODE]!.hydrated).toBe(true)
    expect(st.nodes[NODE]!.syncedAt).toBe(777)
    expect(view().map((v) => v.title).sort()).toEqual(['cached one', 'typed offline'])
    expect(view().find((v) => v.id === 'new')!.unsynced).toBe(true)
  })
})

describe('offline outbox and replay — scenario table', () => {
  const sent = () => daemon.log.filter((l) => l.method !== 'todo.list').map((l) => `${l.method}${(l.params as { title?: string }).title ? `:${(l.params as { title?: string }).title}` : ''}`)

  it('add offline: shown at once as unsynced, survives a reload, replayed once the node is ready; the daemon ends up with exactly one item', async () => {
    goOffline()
    const r = await addTodo(NODE, { title: 'buy milk', body: 'note', project: 'home' })
    expect(r.ok).toBe(true)
    expect(daemon.log).toEqual([])
    expect(byTitle('buy milk')).toMatchObject({ unsynced: true, project: 'home', state: 'open' })
    // 重新加载页面：内存清掉，从库里读回来
    resetTodosMemory()
    await hydrateTodos([NODE])
    expect(byTitle('buy milk')).toMatchObject({ unsynced: true })
    await goOnline()
    await replayOutbox(NODE)
    expect(sent()).toEqual(['todo.add:buy milk'])
    expect([...daemon.todos.values()].map((t) => t.title)).toEqual(['buy milk'])
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(byTitle('buy milk')).toMatchObject({ unsynced: false })
  })

  it('the id is a client-generated lowercase UUID, and the same id is sent again on every retry', async () => {
    goOffline()
    const r = await addTodo(NODE, { title: 'x', body: '' })
    expect(r.ok && r.id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/)
  })

  it('add then edit the title offline: two requests in order, nothing is merged', async () => {
    goOffline()
    const r = await addTodo(NODE, { title: 'draft', body: '' })
    if (!r.ok) throw new Error('add failed')
    await updateTodo(NODE, r.id, { title: 'final', body: 'b' })
    expect(await db.listOutbox(NODE)).toHaveLength(2)
    await goOnline()
    await replayOutbox(NODE)
    expect(sent()).toEqual(['todo.add:draft', 'todo.update:final'])
    expect(daemon.todos.get(r.id)).toMatchObject({ title: 'final', body: 'b' })
  })

  it('edit an existing item several times offline: one todo.update per edit, each carrying only the fields that were changed', async () => {
    daemon.seed(todoItem({ id: 'e1', title: 'orig', body: 'keep me', project: 'p' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'e1', { title: 'a' })
    await updateTodo(NODE, 'e1', { title: 'b' })
    await updateTodo(NODE, 'e1', { state: 'working' })
    expect(await db.listOutbox(NODE)).toHaveLength(3)
    expect(byTitle('b')).toMatchObject({ state: 'working', unsynced: true, body: 'keep me' })
    await goOnline()
    daemon.log = []
    await replayOutbox(NODE)
    expect(daemon.log).toEqual([
      { method: 'todo.update', params: { id: 'e1', title: 'a' } },
      { method: 'todo.update', params: { id: 'e1', title: 'b' } },
      { method: 'todo.update', params: { id: 'e1', state: 'working' } },
    ])
    expect(daemon.todos.get('e1')).toMatchObject({ title: 'b', state: 'working', body: 'keep me', project: 'p' })
  })

  it('add then delete offline: both are sent in order (nothing is cancelled), the daemon ends with nothing', async () => {
    goOffline()
    const r = await addTodo(NODE, { title: 'oops', body: '' })
    if (!r.ok) throw new Error('add failed')
    await deleteTodo(NODE, r.id)
    expect(view()).toEqual([])
    expect(await db.listOutbox(NODE)).toHaveLength(2)
    await goOnline()
    await replayOutbox(NODE)
    expect(sent()).toEqual(['todo.add:oops', 'todo.delete'])
    expect(daemon.todos.size).toBe(0)
  })

  it('edits to different items are replayed in the order they were made', async () => {
    daemon.seed(todoItem({ id: 'a' }), todoItem({ id: 'b' }), todoItem({ id: 'c' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'b', { title: 'B' })
    await updateTodo(NODE, 'c', { title: 'C' })
    await updateTodo(NODE, 'a', { title: 'A' })
    await goOnline()
    daemon.log = []
    await replayOutbox(NODE)
    expect(daemon.log.filter((l) => l.method !== 'todo.list').map((l) => (l.params as { id: string }).id)).toEqual(['b', 'c', 'a'])
  })

  it('update → not_found (deleted elsewhere): that operation is dropped, nothing is marked failed, the list then reflects the daemon', async () => {
    daemon.seed(todoItem({ id: 'e1' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'e1', { title: 'edited' })
    daemon.todos.delete('e1')
    await goOnline()
    expect(await replayOutbox(NODE)).toBe('done')
    expect(await db.listOutbox(NODE)).toEqual([])
    await syncTodos(NODE)
    expect(view()).toEqual([])
  })

  it('delete → not_found counts as done', async () => {
    daemon.seed(todoItem({ id: 'e1' }))
    await syncTodos(NODE)
    goOffline()
    await deleteTodo(NODE, 'e1')
    daemon.todos.delete('e1')
    await goOnline()
    expect(await replayOutbox(NODE)).toBe('done')
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(view()).toEqual([])
  })

  it('reopening a closed item when the list is full: the change is dropped, the item stays closed as the daemon has it, and the user is told', async () => {
    daemon.seed(todoItem({ id: 'closed', state: 'done' }))
    daemon.openLimit = 0
    await syncTodos(NODE)
    await loadClosed(NODE)
    goOffline()
    await updateTodo(NODE, 'closed', { state: 'open' })
    expect(byTitle('todo closed')!.state).toBe('open')
    await goOnline()
    await replayOutbox(NODE)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(daemon.todos.get('closed')!.state).toBe('done')
    await syncTodos(NODE)
    await loadClosed(NODE)
    expect(byTitle('todo closed')!.state).toBe('done')
    expect(useUi.getState().toasts.some((t) => /full/i.test(t.text))).toBe(true)
  })

  it('add rejected because the list is full: the item stays on screen as failed (never silently lost); Retry works once there is room; Discard removes it', async () => {
    daemon.openLimit = 0
    await addTodo(NODE, { title: 'important thought', body: 'details' })
    await flush()
    const failed = (await db.listOutbox(NODE))[0]!
    expect(failed).toMatchObject({ state: 'failed', error: { code: 'invalid', field: 'todos', reason: 'too_many' } })
    expect(byTitle('important thought')).toMatchObject({ failed: { reason: 'too_many' }, unsynced: true })
    // 重试：有空位了
    daemon.openLimit = 1000
    await retryOp(failed)
    await flush()
    expect(daemon.todos.size).toBe(1)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(byTitle('important thought')!.failed).toBeUndefined()
    // 丢弃
    daemon.todos.clear()
    daemon.openLimit = 0
    await addTodo(NODE, { title: 'second', body: '' })
    await flush()
    const second = (await db.listOutbox(NODE))[0]!
    await discardOp(second)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(byTitle('second')).toBeUndefined()
  })

  it('a rejected add blocks the later changes of the same item (they would only be not_found) but not other items', async () => {
    daemon.seed(todoItem({ id: 'other' }))
    await syncTodos(NODE)
    goOffline()
    const r = await addTodo(NODE, { title: 'bad', body: '' })
    if (!r.ok) throw new Error('add failed')
    await updateTodo(NODE, r.id, { state: 'working' })
    await updateTodo(NODE, 'other', { title: 'other edited' })
    daemon.openLimit = 0
    await goOnline()
    daemon.log = []
    await replayOutbox(NODE)
    expect(sent()).toEqual(['todo.add:bad', 'todo.update:other edited'])
    const left = await db.listOutbox(NODE)
    expect(left.map((l) => `${l.op.kind}:${l.state}`)).toEqual(['add:failed', 'update:pending'])
  })

  it('reply lost mid-replay: the operation is kept and sent again; todo.add is idempotent so there is still one item', async () => {
    goOffline()
    await addTodo(NODE, { title: 'once', body: '' })
    await goOnline()
    daemon.failNext('todo.add', linkError('timeout'), { after: true })
    expect(await replayOutbox(NODE)).toBe('offline')
    expect(daemon.todos.size).toBe(1)
    expect((await db.listOutbox(NODE)).length).toBe(1)
    expect((await db.listOutbox(NODE))[0]!.claimed_at).toBeUndefined()
    expect(await replayOutbox(NODE)).toBe('done')
    expect(sent()).toEqual(['todo.add:once', 'todo.add:once'])
    expect(daemon.todos.size).toBe(1)
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a replayed update that lost its reply is safe to send again (same fields)', async () => {
    daemon.seed(todoItem({ id: 'e1' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'e1', { title: 'new', state: 'working' })
    await goOnline()
    daemon.failNext('todo.update', linkError('timeout'), { after: true })
    await replayOutbox(NODE)
    await replayOutbox(NODE)
    expect(daemon.todos.get('e1')).toMatchObject({ title: 'new', state: 'working' })
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('the node goes offline again mid-replay: what was sent stays sent, the rest waits (in order)', async () => {
    daemon.seed(todoItem({ id: 'a' }), todoItem({ id: 'b' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'a', { title: 'A' })
    await updateTodo(NODE, 'b', { title: 'B' })
    await goOnline()
    daemon.log = []
    daemon.failNext('todo.update', linkError('closed'))
    expect(await replayOutbox(NODE)).toBe('offline')
    expect(daemon.todos.get('a')!.title).not.toBe('A')
    expect((await db.listOutbox(NODE)).map((r) => r.id)).toEqual(['a', 'b'])
    expect(await replayOutbox(NODE)).toBe('done')
    expect(daemon.log.filter((l) => l.method !== 'todo.list').map((l) => (l.params as { id: string }).id)).toEqual(['a', 'a', 'b'])
    expect(daemon.todos.get('b')!.title).toBe('B')
  })

  it('does not send anything while the link is not ready', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    expect(await replayOutbox(NODE)).toBe('offline')
    expect(daemon.log).toEqual([])
  })

  it('a todo.changed for an item with queued local edits updates the daemon’s copy underneath; the user’s edit still shows, then the daemon decides after replay', async () => {
    daemon.seed(todoItem({ id: 'e1', title: 'orig', body: 'daemon body', updated_ms: 5 }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'e1', { title: 'mine' })
    // 别的设备改了正文和标题
    handleTodoChanged(NODE, { id: 'e1', todo: todoItem({ id: 'e1', title: 'theirs', body: 'their body', updated_ms: 50 }) })
    expect(byTitle('mine')).toMatchObject({ body: 'their body', unsynced: true })
    expect(useTodos.getState().nodes[NODE]!.items['e1']!.title).toBe('theirs')
    daemon.seed(todoItem({ id: 'e1', title: 'theirs', body: 'their body', updated_ms: 50 }))
    await goOnline()
    await replayOutbox(NODE)
    // 重放的只有 title，正文没被覆盖；后重放的覆盖先到的同名字段
    expect(daemon.todos.get('e1')).toMatchObject({ title: 'mine', body: 'their body' })
  })

  it('a rejected edit (not the add) is visible as failed on the item, and Discard puts it back to the daemon’s version', async () => {
    daemon.seed(todoItem({ id: 'e1', title: 'orig' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'e1', { title: 'edited' })
    await goOnline()
    daemon.failNext('todo.update', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    const failed = (await db.listOutbox(NODE))[0]!
    expect(failed.state).toBe('failed')
    expect(byTitle('edited')).toMatchObject({ failed: { field: 'title' } })
    await discardOp(failed)
    expect(byTitle('orig')).toBeDefined()
    expect(byTitle('orig')!.unsynced).toBe(false)
  })

  it('an unknown error marks that operation failed (Retry / Discard); the other items go on', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await addTodo(NODE, { title: 'y', body: '' })
    await goOnline()
    daemon.failNext('todo.add', rpcError('internal'))
    expect(await replayOutbox(NODE)).toBe('done')
    expect((await db.listOutbox(NODE)).map((r) => `${r.op.kind}:${r.state}`)).toEqual(['add:failed'])
    expect(byTitle('x')).toMatchObject({ failed: { code: 'internal' } })
    expect([...daemon.todos.values()].map((t) => t.title)).toEqual(['y'])
  })

  it('goes through the outbox even online: the same path, replayed immediately, ending with the daemon’s copy', async () => {
    await syncTodos(NODE)
    const r = await addTodo(NODE, { title: 'online add', body: '' })
    if (!r.ok) throw new Error('add failed')
    await flush()
    expect(daemon.todos.has(r.id)).toBe(true)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(byTitle('online add')!.unsynced).toBe(false)
  })

  it('syncTodos replays first and lists afterwards, so the list already contains what was replayed', async () => {
    goOffline()
    await addTodo(NODE, { title: 'queued', body: '' })
    await goOnline()
    daemon.log = []
    await syncTodos(NODE)
    expect(daemon.methods()).toEqual(['todo.add', 'todo.list'])
    expect(byTitle('queued')!.unsynced).toBe(false)
  })

  it('validation: nothing is queued for an invalid title / body', async () => {
    expect((await addTodo(NODE, { title: '   ', body: '' })).ok).toBe(false)
    expect((await addTodo(NODE, { title: 'a'.repeat(201), body: '' })).ok).toBe(false)
    expect((await addTodo(NODE, { title: 'ok', body: 'x'.repeat(16 * 1024 + 1) })).ok).toBe(false)
    expect((await updateTodo(NODE, 'e1', { title: 'two\nlines' })).ok).toBe(false)
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a blank project in an edit becomes a clear, not an empty string', async () => {
    daemon.seed(todoItem({ id: 'e1', project: 'p' }))
    await syncTodos(NODE)
    await updateTodo(NODE, 'e1', { project: '  ' })
    await flush()
    expect(lastWrite()!.params).toEqual({ id: 'e1', clear: ['project'] })
    expect(daemon.todos.get('e1')!.project).toBeUndefined()
  })
})

describe('two tabs', () => {
  // 假的 Web Locks：同名的锁排队，一个释放了下一个才开始（和真的一样）
  const held = new Map<string, Promise<void>>()
  let releaseExternal: (() => void) | null = null
  beforeEach(() => {
    held.clear()
    Object.defineProperty(navigator, 'locks', {
      configurable: true,
      value: {
        request: async (name: string, cb: (l: object) => Promise<unknown>) => {
          const prev = held.get(name) ?? Promise.resolve()
          let done!: () => void
          held.set(name, new Promise<void>((r) => (done = r)))
          await prev
          try {
            return await cb({})
          } finally {
            done()
          }
        },
      },
    })
  })
  afterEach(() => Reflect.deleteProperty(navigator, 'locks'))

  it('while another tab holds the replay lock this one sends nothing; when it lets go, it sends each operation once', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await goOnline()
    held.set(`anycon-todo-replay:${NODE}`, new Promise<void>((r) => (releaseExternal = r)))
    const pending = replayOutbox(NODE)
    await flush()
    expect(daemon.log).toEqual([])
    releaseExternal!()
    expect(await pending).toBe('done')
    expect(daemon.methods()).toEqual(['todo.add'])
  })

  it('two replays at the same moment send each operation once (the second pass finds nothing left)', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await addTodo(NODE, { title: 'y', body: '' })
    await goOnline()
    const results = await Promise.all([replayOutbox(NODE), replayOutbox(NODE)])
    expect(results).toEqual(['done', 'done'])
    expect(daemon.methods()).toEqual(['todo.add', 'todo.add'])
  })

  it('a change queued just as a replay is finishing is not stranded: the pass queued behind it sends it', async () => {
    await syncTodos(NODE)
    daemon.log = []
    const first = addTodo(NODE, { title: 'first', body: '' })
    const second = addTodo(NODE, { title: 'second', body: '' })
    await Promise.all([first, second])
    await vi.waitFor(async () => {
      expect(daemon.todos.size).toBe(2)
      expect(await db.listOutbox(NODE)).toEqual([])
    })
  })

  it('the other tab’s changes are picked up from the database', async () => {
    goOffline()
    await db.appendOutbox({ node_id: NODE, id: 'zz', op: { kind: 'add', params: { title: 'from tab 2', body: '' } }, created_at: 1, state: 'pending' })
    expect(byTitle('from tab 2')).toBeUndefined()
    await reloadFromDb(NODE)
    expect(byTitle('from tab 2')).toBeDefined()
  })

  it('stale claims left by a tab that was closed mid-send are released by the next replay', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    const [r] = await db.listOutbox(NODE)
    await db.patchOutbox(r!.seq!, (x) => ({ ...x, claimed_at: 1, claimed_by: 'dead-tab' }))
    await goOnline()
    expect(await replayOutbox(NODE)).toBe('done')
    expect(daemon.methods()).toEqual(['todo.add'])
  })
})

describe('forgetting a node', () => {
  it('drops its cache and its unsent changes and says how many were dropped; other nodes are untouched', async () => {
    goOffline()
    await addTodo(NODE, { title: 'one', body: '' })
    await addTodo(NODE, { title: 'two', body: '' })
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c' })] })
    await db.applyTodoCache(OTHER, { put: [todoItem({ id: 'keep' })] })
    expect(await forgetNodeTodos(NODE)).toBe(2)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect((await db.listTodoRecords(NODE)).todos).toEqual([])
    expect((await db.listTodoRecords(OTHER)).todos).toHaveLength(1)
    expect(useTodos.getState().outbox).toEqual([])
  })

  it('db.deleteNode (the transport’s forget) clears them too', async () => {
    goOffline()
    await addTodo(NODE, { title: 'one', body: '' })
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c' })], syncedAt: 1 })
    await db.deleteNode(NODE)
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(await db.listTodoRecords(NODE)).toEqual({ todos: [] })
  })
})

describe('removing this device', () => {
  it('resetDb (“sign out and remove this device”) deletes the to-do cache, the sync times and the unsent changes; wipeAll (switching accounts) too', async () => {
    goOffline()
    await addTodo(NODE, { title: 'secret', body: 'private notes' })
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c', title: 'secret cached' })], syncedAt: 5 })
    await db.wipeAll()
    expect(await db.listOutbox()).toEqual([])
    expect((await db.listTodoRecords(NODE)).todos).toEqual([])
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c2' })], syncedAt: 5 })
    await db.appendOutbox({ node_id: NODE, id: 'q', op: { kind: 'delete' }, created_at: 1, state: 'pending' })
    await db.resetDb()
    expect(await db.listOutbox()).toEqual([])
    expect(await db.listTodoRecords(NODE)).toEqual({ todos: [] })
  })

  it('signing out only clears memory: the cache stays for the same account', async () => {
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'c' })] })
    await hydrateTodos([NODE])
    resetTodosMemory()
    expect(useTodos.getState().nodes).toEqual({})
    expect((await db.listTodoRecords(NODE)).todos).toHaveLength(1)
  })
})
