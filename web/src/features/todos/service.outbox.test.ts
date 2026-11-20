// 待发队列是只追加的有序日志（outbox.ts 头部）：不合并、不取消；认领是一个事务；一项一队；失败只挡本项；
// 同步翻页期间的删除和变更不被旧页盖掉；没有 Web Locks 时用租约；入队写失败的处理。
// 前半（B1–B5）只用公开的行为（addTodo / updateTodo / deleteTodo / syncTodos / replayOutbox + 假 daemon），不依赖库的内部形状。
import { deleteDB, openDB } from 'idb'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { setNavigator } from '@/lib/navigate'
import { storeShared } from '@/sw/shareStore'
import { SHARE_DB } from '@/sw/shareStore'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon, linkError } from '@/test/fakeTodoDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { addTodo, deleteTodo, discardOp, handleTodoChanged, hydrateTodos, MAX_LINK_FAILURES, refreshAfterReplay, replayOutbox, resetTodosMemory, retryOp, syncTodos, updateTodo } from './service'
import { useTodos, viewOf } from './store'

const NODE = 'n'.repeat(32)
const info = { capabilities: { todo: true } } as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const view = () => viewOf(useTodos.getState(), [NODE])
const byTitle = (t: string) => view().find((v) => v.title === t)
const flush = async (): Promise<void> => {
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 2))
}
const goOffline = (): void => {
  rpc.ready = false
  useNodes.getState().patch(NODE, { conn: 'peer_offline' })
}
const goOnline = async (): Promise<void> => {
  await flush()
  rpc.ready = true
  useNodes.getState().patch(NODE, { conn: 'ready' })
}
const sent = () => daemon.log.filter((l) => l.method !== 'todo.list').map((l) => `${l.method}${(l.params as { title?: string }).title ? `:${(l.params as { title?: string }).title}` : ''}`)
const ids = () => [...daemon.todos.values()].map((t) => t.title).sort()

beforeEach(async () => {
  await db.resetDb()
  await deleteDB(SHARE_DB)
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, info, conn: 'ready' }) }, order: [NODE], loaded: true })
  resetTodosMemory()
  useUi.setState({ toasts: [] })
})
afterEach(async () => {
  await flush() // 后台重放跑完，别漏到下一个用例里
  setRpc(null)
  vi.restoreAllMocks()
})

/** 离线记一项，上线后让第一次 `todo.add` 丢掉回执（daemon 收下了，客户端不知道）。返回项的 id。 */
async function addWithLostReply(title: string): Promise<string> {
  goOffline()
  const r = await addTodo(NODE, { title, body: '' })
  if (!r.ok) throw new Error('add failed')
  await goOnline()
  daemon.failNext('todo.add', linkError('timeout'), { after: true })
  expect(await replayOutbox(NODE)).toBe('offline')
  expect(daemon.todos.get(r.id)?.title).toBe(title)
  return r.id
}

describe('B1 / B2: the add reply was lost, the daemon already has the item', () => {
  it('B1: a later edit is sent as its own update, so the daemon gets the new title', async () => {
    const id = await addWithLostReply('draft')
    expect((await updateTodo(NODE, id, { title: 'final' })).ok).toBe(true)
    await flush()
    expect(daemon.todos.get(id)!.title).toBe('final')
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('B2: a later delete (also the capture toast’s Undo) is sent, so the daemon ends without the item', async () => {
    const id = await addWithLostReply('oops')
    await deleteTodo(NODE, id)
    await flush()
    expect(daemon.todos.has(id)).toBe(false)
    expect(await db.listOutbox(NODE)).toEqual([])
  })
})

describe('B3: reopening when the list is full', () => {
  it('re-sends the same update without the state, so the other fields are kept; the item stays closed and the user is told', async () => {
    daemon.seed(todoItem({ id: 'c', title: 'old', state: 'done' }))
    daemon.openLimit = 0
    await syncTodos(NODE)
    await db.applyTodoCache(NODE, { put: [daemon.todos.get('c')!] })
    await hydrateTodos([NODE])
    goOffline()
    await updateTodo(NODE, 'c', { state: 'open', title: 'new title' })
    await goOnline()
    await replayOutbox(NODE)
    expect(daemon.todos.get('c')).toMatchObject({ title: 'new title', state: 'done' })
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(useUi.getState().toasts.some((t) => /full/i.test(t.text))).toBe(true)
  })

  it('B3: separate edits ({state} then {title}) are separate records, so a refused reopen never takes the title with it', async () => {
    daemon.seed(todoItem({ id: 'c', title: 'old', state: 'done' }))
    daemon.openLimit = 0
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'c', { state: 'open' })
    await updateTodo(NODE, 'c', { title: 'T' })
    await goOnline()
    await replayOutbox(NODE)
    expect(daemon.todos.get('c')).toMatchObject({ title: 'T', state: 'done' })
  })
})

describe('B4: edits made while a replay is running', () => {
  it('an edit queued while another one is in flight is not overwritten by a stale snapshot: all of them arrive in order', async () => {
    daemon.seed(todoItem({ id: 'e', title: 'orig', body: 'orig' }))
    await syncTodos(NODE)
    const original = rpc.handlers['todo.update']!
    let first = true
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.update'] = async (p, n) => {
      if (first) {
        first = false
        await gate
      }
      return original(p, n)
    }
    await updateTodo(NODE, 'e', { title: 'one' })
    await flush() // 第一条已发出、卡在 gate 上（认领着）
    await updateTodo(NODE, 'e', { body: 'two' })
    await updateTodo(NODE, 'e', { title: 'three' })
    release()
    await flush()
    expect(daemon.todos.get('e')).toMatchObject({ title: 'three', body: 'two' })
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a burst of edits online all arrive, in order', async () => {
    daemon.seed(todoItem({ id: 'e', title: 'orig' }))
    await syncTodos(NODE)
    await Promise.all(Array.from({ length: 12 }, (_, i) => updateTodo(NODE, 'e', { title: `t${i}` })))
    await vi.waitFor(() => expect(daemon.todos.get('e')!.title).toBe('t11'))
    expect(daemon.log.filter((l) => l.method === 'todo.update').map((l) => (l.params as { title: string }).title)).toEqual(Array.from({ length: 12 }, (_, i) => `t${i}`))
  })

  it('claiming only sets the claim fields on the record as it is stored', async () => {
    const seq = await db.appendOutbox({ node_id: NODE, id: 'x', op: { kind: 'update', fields: { title: 'v1' } }, created_at: 1, state: 'pending' })
    await db.patchOutbox(seq, (r) => ({ ...r, op: { kind: 'update', fields: { title: 'v2' } } }))
    const claimed = await db.claimNextOutbox(NODE, 'me', 10, 0)
    expect(claimed).toMatchObject({ seq, claimed_by: 'me', op: { fields: { title: 'v2' } } })
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ claimed_at: 10, op: { fields: { title: 'v2' } } })
  })
})

describe('per-item ordering and blocking', () => {
  it('only the head record of an item is sent; a failed head blocks that item and no other', async () => {
    goOffline()
    const a = await addTodo(NODE, { title: 'bad', body: '' })
    if (!a.ok) throw new Error()
    await updateTodo(NODE, a.id, { state: 'working' })
    await addTodo(NODE, { title: 'fine', body: '' })
    await goOnline()
    daemon.failNext('todo.add', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    expect(sent()).toEqual(['todo.add:bad', 'todo.add:fine'])
    expect((await db.listOutbox(NODE)).map((r) => `${r.op.kind}:${r.state}`)).toEqual(['add:failed', 'update:pending'])
  })

  it('S3: an unknown error and a not_found on an add mark only that record failed, with Retry available; other items are not blocked', async () => {
    goOffline()
    await addTodo(NODE, { title: 'one', body: '' })
    await addTodo(NODE, { title: 'two', body: '' })
    await addTodo(NODE, { title: 'three', body: '' })
    await goOnline()
    daemon.failNext('todo.add', rpcError('internal'))
    daemon.failNext('todo.add', rpcError('not_found'))
    expect(await replayOutbox(NODE)).toBe('done')
    expect(ids()).toEqual(['three'])
    const failed = (await db.listOutbox(NODE)).filter((r) => r.state === 'failed')
    expect(failed).toHaveLength(2)
    await retryOp(failed[0]!)
    await flush()
    expect(ids()).toHaveLength(2)
  })

  it('a link error that keeps failing becomes “failed” after MAX_LINK_FAILURES, persisted across replays', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await goOnline()
    for (let i = 0; i < MAX_LINK_FAILURES; i++) {
      expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'pending' })
      expect((await db.listOutbox(NODE))[0]!.attempts ?? 0).toBe(i)
      daemon.failNext('todo.add', linkError('timeout'))
      await replayOutbox(NODE)
    }
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'failed', error: { code: 'link' } })
    expect(byTitle('x')).toMatchObject({ failed: { code: 'link' } })
  })

  it('an offline error is not counted as a failure', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    for (let i = 0; i < 5; i++) await replayOutbox(NODE)
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'pending' })
    expect((await db.listOutbox(NODE))[0]!.attempts).toBeUndefined()
  })

  it('Discard of a failed add also discards the updates queued behind it for the same item, and says so', async () => {
    goOffline()
    const a = await addTodo(NODE, { title: 'bad', body: '' })
    if (!a.ok) throw new Error()
    await updateTodo(NODE, a.id, { state: 'working' })
    await updateTodo(NODE, a.id, { title: 'bad2' })
    const other = await addTodo(NODE, { title: 'other', body: '' })
    expect(other.ok).toBe(true)
    await goOnline()
    daemon.failNext('todo.add', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    const failed = (await db.listOutbox(NODE)).find((r) => r.state === 'failed')!
    await discardOp(failed)
    expect((await db.listOutbox(NODE)).map((r) => r.id)).toEqual([])
    expect(useUi.getState().toasts.some((t) => /2/.test(t.text))).toBe(true)
    expect(ids()).toEqual(['other'])
  })
})

describe('B5 / S1: the list is paged while things change', () => {
  /** 第一页回来之后、翻页还没结束时，执行 `during`（模拟别的设备 / 本地动作在这期间做了事）。 */
  function duringPaging(during: () => void): void {
    const original = rpc.handlers['todo.list']!
    let done = false
    rpc.handlers['todo.list'] = async (p, n) => {
      const r = await original(p, n)
      if (!done) {
        done = true
        during()
      }
      return r
    }
  }

  it('B5: an item deleted while the initial sync pages does not come back, in memory or in IndexedDB', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'doomed' }), todoItem({ id: 'b', title: 'stays' }))
    duringPaging(() => handleTodoChanged(NODE, { id: 'a', deleted: true }))
    await syncTodos(NODE)
    await flush()
    expect(view().map((v) => v.id)).toEqual(['b'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['b'])
  })

  it('B5: an item deleted by the user while the refresh after a replay pages does not come back either', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'doomed' }), todoItem({ id: 'b' }))
    await syncTodos(NODE)
    duringPaging(() => void deleteTodo(NODE, 'a'))
    await refreshAfterReplay(NODE)
    await flush()
    expect(view().map((v) => v.id)).toEqual(['b'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['b'])
  })

  it('S1: the refresh keeps an item created during paging, does not replace a newer version with an older page, and removes vanished items from IndexedDB', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'old', updated_ms: 10 }))
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'vanished' })] })
    await hydrateTodos([NODE])
    duringPaging(() => {
      handleTodoChanged(NODE, { id: 'a', todo: todoItem({ id: 'a', title: 'newer', updated_ms: 50 }) })
      handleTodoChanged(NODE, { id: 'late', todo: todoItem({ id: 'late', updated_ms: 20 }) })
    })
    await refreshAfterReplay(NODE)
    await flush()
    expect(view().map((v) => `${v.id}:${v.title}`).sort()).toEqual(['a:newer', 'late:todo late'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id).sort()).toEqual(['a', 'late'])
  })

  it('S1: two pagings of the same node never run at once', async () => {
    daemon.seed(todoItem({ id: 'a' }))
    let active = 0
    let max = 0
    const original = rpc.handlers['todo.list']!
    rpc.handlers['todo.list'] = async (p, n) => {
      active++
      max = Math.max(max, active)
      await new Promise((r) => setTimeout(r, 5))
      active--
      return original(p, n)
    }
    await Promise.all([refreshAfterReplay(NODE), refreshAfterReplay(NODE), syncTodos(NODE)])
    expect(max).toBe(1)
  })
})

describe('S2: no full re-list per edit', () => {
  it('an edit made online relies on the returned Todo and todo.changed: no todo.list', async () => {
    daemon.seed(todoItem({ id: 'a' }))
    await syncTodos(NODE)
    daemon.log = []
    await updateTodo(NODE, 'a', { title: 'edited' })
    await addTodo(NODE, { title: 'added', body: '' })
    await flush()
    expect(daemon.methods()).toEqual(['todo.update', 'todo.add'])
  })

  it('records queued while the node was not ready are followed by one re-list', async () => {
    goOffline()
    await addTodo(NODE, { title: 'queued', body: '' })
    await goOnline()
    daemon.log = []
    await replayOutbox(NODE)
    await refreshAfterReplay(NODE)
    expect(daemon.methods()).toEqual(['todo.add', 'todo.list'])
  })
})

describe('S5: errors while enqueueing', () => {
  it('a failed write shows an error result, queues nothing, and does not throw', async () => {
    vi.spyOn(db, 'appendOutbox').mockRejectedValue(new DOMException('quota', 'QuotaExceededError'))
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const r = await addTodo(NODE, { title: 'x', body: '' })
    expect(r).toEqual({ ok: false, error: { field: 'storage', reason: 'failed' } })
    expect(await updateTodo(NODE, 'a', { title: 'y' })).toMatchObject({ ok: false })
    expect(await deleteTodo(NODE, 'a')).toBe(false)
    expect(useUi.getState().toasts.some((t) => /device/i.test(t.text))).toBe(true)
    vi.restoreAllMocks()
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('when the write committed and a later step throws, the result is still ok (the record is queued exactly once)', async () => {
    vi.spyOn(db, 'listOutbox').mockRejectedValue(new Error('reload failed'))
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    goOffline()
    const r = await addTodo(NODE, { title: 'once', body: '' })
    expect(r.ok).toBe(true)
    vi.restoreAllMocks()
    expect(await db.listOutbox(NODE)).toHaveLength(1)
  })
})

describe('S4: no Web Locks (two realms, a lease)', () => {
  // 第二个实例：重新加载模块，得到各自的内存状态和实例 id，共用同一个（假的）IndexedDB 和同一个假 daemon
  async function secondRealm() {
    vi.resetModules()
    const svc = await import('./service')
    const inst = await import('@/transport/instance')
    const nodes = await import('@/stores/nodes')
    const store = await import('./store')
    inst.setRpc(rpc)
    nodes.useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, info, conn: 'ready' }) }, order: [NODE], loaded: true })
    return { svc, store }
  }
  beforeEach(() => {
    expect(navigator.locks).toBeUndefined()
  })

  it('while one realm is sending, the other sees a live claim and sends nothing; the item arrives once and the claim is not stripped', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await goOnline()
    const original = rpc.handlers['todo.add']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.add'] = async (p, n) => {
      await gate
      return original(p, n)
    }
    const first = replayOutbox(NODE)
    await flush()
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ claimed_by: expect.any(String) })
    const { svc } = await secondRealm()
    expect(await svc.replayOutbox(NODE)).toBe('done')
    expect(daemon.log.filter((l) => l.method === 'todo.add')).toHaveLength(0) // 第二个实例没发（第一个的请求还卡在 gate 上，不算）
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ claimed_by: expect.any(String) })
    release()
    expect(await first).toBe('done')
    expect(ids()).toEqual(['x'])
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a claim older than the request timeout plus the margin is taken over', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    const [r] = await db.listOutbox(NODE)
    await db.patchOutbox(r!.seq!, (x) => ({ ...x, claimed_at: Date.now() - 10 * 60_000, claimed_by: 'dead-realm' }))
    await goOnline()
    const { svc } = await secondRealm()
    expect(await svc.replayOutbox(NODE)).toBe('done')
    expect(ids()).toEqual(['x'])
  })

  it('a fresh claim by someone else is not taken over', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    const [r] = await db.listOutbox(NODE)
    await db.patchOutbox(r!.seq!, (x) => ({ ...x, claimed_at: Date.now(), claimed_by: 'live-realm' }))
    await goOnline()
    expect(await replayOutbox(NODE)).toBe('done')
    expect(daemon.todos.size).toBe(0)
    expect((await db.listOutbox(NODE))[0]!.claimed_by).toBe('live-realm')
  })
})

describe('v1 → v2 migration', () => {
  it('keeps everything that was in a version 1 database and adds the empty to-do stores', async () => {
    await db.resetDb()
    const old = await openDB(db.DB_NAME, 1, {
      upgrade(d) {
        d.createObjectStore('keys', { keyPath: 'id' })
        d.createObjectStore('nodes', { keyPath: 'node_id' })
        d.createObjectStore('sessions', { keyPath: 'session_id' }).createIndex('by_node', 'node_id')
        d.createObjectStore('events', { keyPath: ['session_id', 'seq'] })
        d.createObjectStore('drafts', { keyPath: 'session_id' })
        d.createObjectStore('blobs', { keyPath: 'blob_id' })
      },
    })
    await old.put('keys', { id: 'self', device_id: 'dev-v1', device_token: 'tok-v1', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32).fill(7) }, device_name: 'old' })
    await old.put('nodes', { node_id: 'n-v1', name: 'old node', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 })
    await old.put('drafts', { session_id: 's-v1', text: 'unsent v1', unsent: [] })
    old.close()

    expect((await db.getIdentity())?.device_token).toBe('tok-v1')
    expect((await db.listNodes()).map((n) => n.node_id)).toEqual(['n-v1'])
    expect((await db.listDrafts()).map((d) => d.text)).toEqual(['unsent v1'])
    expect(await db.listOutbox()).toEqual([])
    expect((await db.listTodoRecords('n-v1')).todos).toEqual([])
    await db.appendOutbox({ node_id: 'n-v1', id: 'a', op: { kind: 'delete' }, created_at: 1, state: 'pending' })
    expect(await db.listOutbox('n-v1')).toHaveLength(1)
  })
})

describe('S7: switching accounts', () => {
  it('wipeAll also deletes the share stash, so a share made under account A does not pre-fill for account B', async () => {
    await storeShared({ title: 'for A', text: 'secret', url: '', at: Date.now() })
    expect((await indexedDB.databases()).map((d) => d.name)).toContain(SHARE_DB)
    await db.wipeAll()
    expect((await indexedDB.databases()).map((d) => d.name)).not.toContain(SHARE_DB)
  })
})

describe('U10: a rejected queued change is announced', () => {
  it('after reconnecting, a change the node refuses gives an error toast naming the item, with an action that opens it (Retry / Discard live there)', async () => {
    const go = vi.fn()
    setNavigator(go)
    goOffline()
    const r = await addTodo(NODE, { title: 'refused one', body: '' })
    if (!r.ok) throw new Error()
    await goOnline()
    daemon.failNext('todo.add', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    const toast = useUi.getState().toasts.find((x) => x.kind === 'error')!
    expect(toast.text).toContain('refused one')
    expect(toast.action).toBeDefined()
    toast.action!.run()
    expect(go).toHaveBeenCalledWith(`/todos/nnnnnnnn/${r.id}`)
    setNavigator(null)
  })
})
