// 待发队列是只追加的有序日志（outbox.ts 头部）：不合并、不取消；认领是一个事务；一项一队；失败只挡本项；
// 同步翻页期间的删除和变更不被旧页盖掉；没有 Web Locks 时用租约；入队写失败的处理。
// 前半只用公开的行为（addTodo / updateTodo / deleteTodo / syncTodos / replayOutbox + 假 daemon），不依赖库的内部形状。
import { deleteDB, openDB } from 'idb'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { forgetNode } from '@/features/devices/removeNode'
import { setNavigator } from '@/lib/navigate'
import * as client from '@/transport/instance'
import { storeShared } from '@/sw/shareStore'
import { SHARE_DB } from '@/sw/shareStore'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon, linkError } from '@/test/fakeTodoDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { addTodo, classifyReplayError, forgetNodeTodos, MAX_DROPS, reloadFromDb, deleteTodo, discardOp, handleTodoChanged, hydrateTodos, loadClosed, MAX_LINK_FAILURES, refreshAfterReplay, replayOutbox, resetTodosMemory, RETRY_BASE_MS, RETRY_MAX_MS, retryOp, setClosedExpanded, syncTodos, updateTodo } from './service'
import { useDrafts } from './drafts'
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

describe('the add reply was lost, the daemon already has the item', () => {
  it('a later edit is sent as its own update, so the daemon gets the new title', async () => {
    const id = await addWithLostReply('draft')
    expect((await updateTodo(NODE, id, { title: 'final' })).ok).toBe(true)
    await flush()
    expect(daemon.todos.get(id)!.title).toBe('final')
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a later delete (also the capture toast’s Undo) is sent, so the daemon ends without the item', async () => {
    const id = await addWithLostReply('oops')
    await deleteTodo(NODE, id)
    await flush()
    expect(daemon.todos.has(id)).toBe(false)
    expect(await db.listOutbox(NODE)).toEqual([])
  })
})

describe('reopening when the list is full', () => {
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

  it('separate edits ({state} then {title}) are separate records, so a refused reopen never takes the title with it', async () => {
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

describe('edits made while a replay is running', () => {
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

  it('an unknown error and a not_found on an add mark only that record failed, with Retry available; other items are not blocked', async () => {
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

describe('the list is paged while things change', () => {
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

  it('an item deleted while the initial sync pages does not come back, in memory or in IndexedDB', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'doomed' }), todoItem({ id: 'b', title: 'stays' }))
    duringPaging(() => handleTodoChanged(NODE, { id: 'a', deleted: true }))
    await syncTodos(NODE)
    await flush()
    expect(view().map((v) => v.id)).toEqual(['b'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['b'])
  })

  it('an item deleted by the user while the refresh after a replay pages does not come back either', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'doomed' }), todoItem({ id: 'b' }))
    await syncTodos(NODE)
    duringPaging(() => void deleteTodo(NODE, 'a'))
    await refreshAfterReplay(NODE)
    await flush()
    expect(view().map((v) => v.id)).toEqual(['b'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['b'])
  })

  it('the refresh keeps an item created during paging, does not replace a newer version with an older page, and removes vanished items from IndexedDB', async () => {
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

  it('two pagings of the same node never run at once', async () => {
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

describe('no full re-list per edit', () => {
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

describe('errors while enqueueing', () => {
  it('a failed write shows an error result, queues nothing, and does not throw', async () => {
    vi.spyOn(db, 'appendOutbox').mockRejectedValue(new DOMException('quota', 'QuotaExceededError'))
    vi.spyOn(db, 'appendDelete').mockRejectedValue(new DOMException('quota', 'QuotaExceededError'))
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

describe('no Web Locks (two realms, a lease)', () => {
  // 第二个实例：重新加载模块，得到各自的内存状态和实例 id，共用同一个（假的）IndexedDB 和同一个假 daemon
  const realms: Array<{ closeTodoChannel: () => void }> = []
  afterEach(() => {
    for (const r of realms.splice(0)) r.closeTodoChannel() // 别让这些模块的 BroadcastChannel 留到下一个用例 / 文件里
  })
  async function secondRealm() {
    vi.resetModules()
    const svc = await import('./service')
    realms.push(svc)
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

describe('switching accounts', () => {
  it('wipeAll also deletes the share stash, so a share made under account A does not pre-fill for account B', async () => {
    await storeShared({ title: 'for A', text: 'secret', url: '', at: Date.now() })
    expect((await indexedDB.databases()).map((d) => d.name)).toContain(SHARE_DB)
    await db.wipeAll()
    expect((await indexedDB.databases()).map((d) => d.name)).not.toContain(SHARE_DB)
  })
})

describe('a rejected queued change is announced', () => {
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

describe('a delete behind a failed record', () => {
  it('discards the failed record and what was queued after it, then sends the delete: the item is never hidden while something unsendable is queued', async () => {
    daemon.seed(todoItem({ id: 'x', title: 'keep?' }))
    await syncTodos(NODE)
    goOffline()
    await updateTodo(NODE, 'x', { title: 'refused' })
    await updateTodo(NODE, 'x', { body: 'later' })
    await goOnline()
    daemon.failNext('todo.update', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    expect((await db.listOutbox(NODE)).map((r) => r.state)).toEqual(['failed', 'pending'])
    await deleteTodo(NODE, 'x')
    await vi.waitFor(() => expect(daemon.todos.has('x')).toBe(false))
    expect(await db.listOutbox(NODE)).toEqual([])
  })

  it('a failed add whose item is then deleted leaves nothing queued and nothing hidden behind it', async () => {
    goOffline()
    const r = await addTodo(NODE, { title: 'bad', body: '' })
    if (!r.ok) throw new Error()
    await goOnline()
    daemon.failNext('todo.add', rpcError('invalid', 'title'))
    await replayOutbox(NODE)
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'failed' })
    await deleteTodo(NODE, r.id)
    await flush()
    expect(await db.listOutbox(NODE)).toEqual([])
    expect(view()).toEqual([])
  })
})

describe('retrying while the node stays ready', () => {
  afterEach(() => {
    vi.restoreAllMocks()
    vi.useRealTimers()
  })
  /** 第 n 次尝试之后（失败次数记下、退避的定时器排好了）。 */
  const afterAttempt = (n: number) => vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.attempts).toBe(n))

  it('a request that times out is sent again after a backoff, without anything else having to happen', async () => {
    await syncTodos(NODE)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    daemon.failNext('todo.add', linkError('timeout'))
    const r = await addTodo(NODE, { title: 'slow', body: '' })
    if (!r.ok) throw new Error()
    await afterAttempt(1)
    expect(daemon.todos.size).toBe(0)
    expect(byTitle('slow')).toMatchObject({ unsynced: true })
    await vi.advanceTimersByTimeAsync(RETRY_BASE_MS + 50)
    await vi.waitFor(() => expect(daemon.todos.size).toBe(1))
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
  })

  it('the retry is cancelled when the node goes away: nothing is sent while it is not ready', async () => {
    await syncTodos(NODE)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    daemon.failNext('todo.add', linkError('timeout'))
    await addTodo(NODE, { title: 'slow', body: '' })
    await afterAttempt(1)
    goOffline()
    daemon.log = []
    await vi.advanceTimersByTimeAsync(RETRY_MAX_MS * 2)
    expect(daemon.log).toEqual([])
    expect(await db.listOutbox(NODE)).toHaveLength(1)
  })

  it('the backoff doubles between attempts and is capped', async () => {
    await syncTodos(NODE)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const delays: number[] = []
    const real = globalThis.setTimeout
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(((fn: () => void, ms?: number) => {
      if (ms === RETRY_BASE_MS || ms === RETRY_BASE_MS * 2) delays.push(ms)
      return real(fn, ms)
    }) as typeof setTimeout)
    daemon.failNext('todo.add', linkError('timeout'))
    daemon.failNext('todo.add', linkError('timeout'))
    await addTodo(NODE, { title: 'slow', body: '' })
    await afterAttempt(1)
    await vi.advanceTimersByTimeAsync(RETRY_BASE_MS + 50)
    await afterAttempt(2)
    expect(delays).toEqual([RETRY_BASE_MS, RETRY_BASE_MS * 2])
    expect(RETRY_BASE_MS * 2 ** 10).toBeGreaterThan(RETRY_MAX_MS)
  })
})

describe('which errors count as a failed attempt', () => {
  const add = { kind: 'add', params: { title: 't', body: '' } } as const

  it('the peer or the hub going away is offline, not a strike; only a timeout while the node is ready, or a real error reply, counts', () => {
    for (const kind of ['offline', 'closed', 'undeliverable'] as const) expect(classifyReplayError(add, linkError(kind))).toEqual({ kind: 'offline' })
    expect(classifyReplayError(add, linkError('timeout'), true)).toEqual({ kind: 'link' })
    expect(classifyReplayError(add, linkError('timeout'), false)).toEqual({ kind: 'offline' })
    expect(classifyReplayError(add, rpcError('invalid', 'title'))).toMatchObject({ kind: 'failed' })
    expect(classifyReplayError(add, rpcError('internal'))).toMatchObject({ kind: 'failed' })
    expect(classifyReplayError({ kind: 'delete' }, rpcError('not_found'))).toEqual({ kind: 'drop' })
    expect(classifyReplayError(add, rpcError('not_found'))).toMatchObject({ kind: 'failed' })
  })

  it('a flaky connection (closed / undeliverable many times) never marks a record failed', async () => {
    await syncTodos(NODE)
    daemon.failNext('todo.add', linkError('closed'))
    daemon.failNext('todo.add', linkError('undeliverable'))
    daemon.failNext('todo.add', linkError('closed'))
    daemon.failNext('todo.add', linkError('undeliverable'))
    goOffline()
    await addTodo(NODE, { title: 'flaky', body: '' })
    await goOnline()
    for (let i = 0; i < 4; i++) await replayOutbox(NODE)
    const rec = (await db.listOutbox(NODE))[0]
    expect(rec).toMatchObject({ state: 'pending' })
    expect(rec!.attempts ?? 0).toBe(0)
    await replayOutbox(NODE)
    expect(ids()).toEqual(['flaky'])
  })
})

describe('the claim belongs to whoever made it', () => {
  it('after a timeout, a record that another instance took over is not touched (its claim and attempt count stay)', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    await goOnline()
    const original = rpc.handlers['todo.add']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.add'] = async (p, n) => {
      await gate
      void original(p, n)
      throw linkError('timeout')
    }
    const running = replayOutbox(NODE)
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.claimed_by).toBeDefined())
    const [rec] = await db.listOutbox(NODE)
    await db.patchOutbox(rec!.seq!, (r) => ({ ...r, claimed_by: 'someone-else', claimed_at: Date.now() }))
    release()
    await running
    const after = (await db.listOutbox(NODE))[0]!
    expect(after.claimed_by).toBe('someone-else')
    expect(after.attempts ?? 0).toBe(0)
  })

  it('a claim stamped in the future (the clock went back) is not taken over', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    const [rec] = await db.listOutbox(NODE)
    await db.patchOutbox(rec!.seq!, (r) => ({ ...r, claimed_by: 'other', claimed_at: Date.now() + 3_600_000 }))
    expect(await db.claimNextOutbox(NODE, 'me', Date.now(), 35_000)).toBeNull()
  })
})

describe('the sync writes the disk before it announces, and does not lose a change that arrives while writing', () => {
  it('other tabs are told only after the list has been written', async () => {
    daemon.seed(todoItem({ id: 'a' }))
    const order: string[] = []
    const post = vi.spyOn(BroadcastChannel.prototype, 'postMessage').mockImplementation(() => void order.push('announce'))
    const commit = db.commitOpenList
    vi.spyOn(db, 'commitOpenList').mockImplementation(async (...args) => {
      await commit(...args)
      order.push('written')
    })
    await syncTodos(NODE)
    expect(order.indexOf('written')).toBeGreaterThanOrEqual(0)
    expect(order.indexOf('announce', order.indexOf('written'))).toBeGreaterThan(order.indexOf('written'))
    expect(order.slice(0, order.indexOf('written')).includes('announce')).toBe(false)
    post.mockRestore()
  })

  it('a todo.changed that arrives while the list is being written is neither lost on disk nor in memory', async () => {
    daemon.seed(todoItem({ id: 'a', title: 'old', updated_ms: 10 }))
    const commit = db.commitOpenList
    vi.spyOn(db, 'commitOpenList').mockImplementation(async (...args) => {
      const p = commit(...args)
      handleTodoChanged(NODE, { id: 'a', todo: todoItem({ id: 'a', title: 'newer', updated_ms: 50 }) })
      await p
    })
    await syncTodos(NODE)
    await flush()
    expect(byTitle('newer')).toBeDefined()
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.title)).toEqual(['newer'])
  })

  it('an item deleted by todo.changed while the list is being written does not come back from disk', async () => {
    daemon.seed(todoItem({ id: 'a' }), todoItem({ id: 'b' }))
    const commit = db.commitOpenList
    vi.spyOn(db, 'commitOpenList').mockImplementation(async (...args) => {
      const p = commit(...args)
      handleTodoChanged(NODE, { id: 'a', deleted: true })
      await p
    })
    await syncTodos(NODE)
    await flush()
    expect(view().map((v) => v.id)).toEqual(['b'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['b'])
  })

  it('a replayed delete that completes between two pages of the refresh is not undone by the stale first page', async () => {
    daemon.pageSize = 1
    daemon.seed(todoItem({ id: 'a', updated_ms: 30 }), todoItem({ id: 'b', updated_ms: 20 }), todoItem({ id: 'c', updated_ms: 10 }))
    await syncTodos(NODE)
    const original = rpc.handlers['todo.list']!
    let pages = 0
    rpc.handlers['todo.list'] = async (p, n) => {
      pages++
      if (pages === 2) {
        await deleteTodo(NODE, 'a')
        await vi.waitFor(() => expect(daemon.todos.has('a')).toBe(false))
      }
      return original(p, n)
    }
    await refreshAfterReplay(NODE)
    await flush()
    expect(view().map((v) => v.id).sort()).toEqual(['b', 'c'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id).sort()).toEqual(['b', 'c'])
  })
})

describe('closed items that were deleted elsewhere', () => {
  it('a full walk of the closed list prunes the cached ones the node no longer has, in memory and on disk', async () => {
    daemon.seed(todoItem({ id: 'c1', state: 'done', updated_ms: 2 }), todoItem({ id: 'c2', state: 'done', updated_ms: 1 }))
    await syncTodos(NODE)
    await loadClosed(NODE)
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items).sort()).toEqual(['c1', 'c2'])
    daemon.todos.delete('c2')
    setClosedExpanded(NODE, false)
    setClosedExpanded(NODE, true)
    await vi.waitFor(() => expect(Object.keys(useTodos.getState().nodes[NODE]!.items)).toEqual(['c1']))
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['c1'])
  })

  it('a closed item deleted while the closed list is requested is not added back by that page', async () => {
    daemon.seed(todoItem({ id: 'c1', state: 'done', updated_ms: 2 }), todoItem({ id: 'c2', state: 'done', updated_ms: 1 }))
    await syncTodos(NODE)
    const original = rpc.handlers['todo.list']!
    rpc.handlers['todo.list'] = async (p, n) => {
      const r = await original(p, n)
      handleTodoChanged(NODE, { id: 'c2', deleted: true })
      return r
    }
    await loadClosed(NODE)
    await flush()
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items)).toEqual(['c1'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toEqual(['c1'])
  })

  it('an item closed elsewhere during the walk is kept', async () => {
    daemon.seed(todoItem({ id: 'c1', state: 'done', updated_ms: 2 }))
    await syncTodos(NODE)
    const original = rpc.handlers['todo.list']!
    rpc.handlers['todo.list'] = async (p, n) => {
      const r = await original(p, n)
      handleTodoChanged(NODE, { id: 'late', todo: todoItem({ id: 'late', state: 'done', updated_ms: 9 }) })
      return r
    }
    await loadClosed(NODE)
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items).sort()).toEqual(['c1', 'late'])
  })
})

describe('the “list is full” notice', () => {
  const fullToasts = () => useUi.getState().toasts.filter((t) => /full/i.test(t.text))

  it('is shown exactly once for a refused reopen (alone, or with other fields kept)', async () => {
    daemon.seed(todoItem({ id: 'c', state: 'done' }), todoItem({ id: 'd', state: 'done' }))
    daemon.openLimit = 0
    await syncTodos(NODE)
    await updateTodo(NODE, 'c', { state: 'open' })
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
    expect(fullToasts()).toHaveLength(1)
    useUi.setState({ toasts: [] })
    await updateTodo(NODE, 'd', { state: 'open', title: 'renamed' })
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
    expect(fullToasts()).toHaveLength(1)
    expect(daemon.todos.get('d')).toMatchObject({ title: 'renamed', state: 'done' })
  })
})

describe('forgetting a node also forgets its to-dos in memory', () => {
  it('the forget-node path drops the cached items and the queued changes from the store and tells the user', async () => {
    goOffline()
    await addTodo(NODE, { title: 'unsent', body: '' })
    useTodos.getState().setCache(NODE, [todoItem({ id: 'c' })])
    vi.spyOn(client, 'getClient').mockReturnValue({ removeNode: vi.fn(async () => undefined) } as never)
    await forgetNode(NODE)
    expect(useTodos.getState().nodes[NODE]).toBeUndefined()
    expect(useTodos.getState().outbox.filter((r) => r.node_id === NODE)).toEqual([])
    expect(useUi.getState().toasts.some((t) => /unsynced/i.test(t.text))).toBe(true)
  })
})

describe('a delete issued while a doomed request is in flight', () => {
  /** 第一条 `todo.update` 卡在 gate 上（已认领、在飞），放开之后用 `outcome` 失败。 */
  async function inFlightUpdate(outcome: unknown): Promise<{ release: () => void; running: Promise<unknown> }> {
    daemon.seed(todoItem({ id: 'x', title: 'orig' }))
    await syncTodos(NODE)
    const original = rpc.handlers['todo.update']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    let first = true
    rpc.handlers['todo.update'] = async (p, n) => {
      if (first) {
        first = false
        await gate
        throw outcome
      }
      return original(p, n)
    }
    const running = updateTodo(NODE, 'x', { title: 'doomed' })
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.claimed_by).toBeDefined())
    return { release, running }
  }

  it('the update then fails with an error reply: the failed record and the ones in between go, the delete is sent, the item is gone', async () => {
    const { release } = await inFlightUpdate(rpcError('invalid', 'title'))
    await deleteTodo(NODE, 'x')
    expect(view()).toEqual([])
    release()
    await vi.waitFor(() => expect(daemon.todos.has('x')).toBe(false))
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
    expect(useUi.getState().toasts.some((t) => t.kind === 'error' && /not saved/i.test(t.text))).toBe(false)
  })

  it('the update then times out for the third time while the node is ready: same outcome', async () => {
    daemon.seed(todoItem({ id: 'x', title: 'orig' }))
    await syncTodos(NODE)
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    rpc.handlers['todo.update'] = async () => {
      await gate
      throw linkError('timeout')
    }
    await db.appendOutbox({ node_id: NODE, id: 'x', op: { kind: 'update', fields: { title: 'doomed' } }, created_at: 1, state: 'pending', attempts: MAX_LINK_FAILURES - 1 })
    await reloadFromDb(NODE)
    const running = replayOutbox(NODE)
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.claimed_by).toBeDefined())
    await deleteTodo(NODE, 'x')
    release()
    await running
    await vi.waitFor(() => expect(daemon.todos.has('x')).toBe(false))
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
  })

  it('failOutbox leaves a plain failure alone when no delete is queued behind it', async () => {
    const seq = await db.appendOutbox({ node_id: NODE, id: 'y', op: { kind: 'update', fields: { title: 't' } }, created_at: 1, state: 'pending' })
    expect(await db.failOutbox(seq, (r) => ({ ...r, state: 'failed', error: { code: 'invalid' } }))).toBe(0)
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'failed' })
  })
})

describe('a closed-list walk that spans a reconnect', () => {
  it('does not prune an item the node still has: a new connection starts the walk over', async () => {
    daemon.pageSize = 1
    daemon.seed(todoItem({ id: 'c1', state: 'done', updated_ms: 30 }), todoItem({ id: 'c2', state: 'done', updated_ms: 20 }), todoItem({ id: 'c3', state: 'done', updated_ms: 10 }))
    await syncTodos(NODE)
    setClosedExpanded(NODE, true)
    await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed.hasMore).toBe(true))
    await loadClosed(NODE, { more: true })
    // 断线期间 c3 在别处改过：没有 todo.changed，它的 updated_ms 排到游标前面去了
    daemon.todos.set('c3', { ...daemon.todos.get('c3')!, title: 'edited while away', updated_ms: 99 })
    await syncTodos(NODE)
    // 新的一次连接重新从头翻（展开着，自动开始）；一页页翻到底
    for (let i = 0; i < 6 && !useTodos.getState().nodes[NODE]!.closed.complete; i++) {
      await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed.loading).toBe(false))
      await loadClosed(NODE, { more: true })
    }
    expect(useTodos.getState().nodes[NODE]!.closed.complete).toBe(true)
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items).sort()).toEqual(['c1', 'c2', 'c3'])
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id).sort()).toEqual(['c1', 'c2', 'c3'])
  })

  it('a walk that fails half way prunes nothing and starts over next time', async () => {
    daemon.pageSize = 1
    daemon.seed(todoItem({ id: 'c1', state: 'done', updated_ms: 20 }), todoItem({ id: 'c2', state: 'done', updated_ms: 10 }))
    await syncTodos(NODE)
    await db.applyTodoCache(NODE, { put: [todoItem({ id: 'stale', state: 'done', updated_ms: 1 })] })
    await hydrateTodos([NODE])
    const original = rpc.handlers['todo.list']!
    let calls = 0
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    rpc.handlers['todo.list'] = async (p, n) => {
      calls++
      if (calls === 2) throw linkError('closed')
      return original(p, n)
    }
    await loadClosed(NODE)
    await loadClosed(NODE, { more: true })
    expect(useTodos.getState().nodes[NODE]!.closed).toMatchObject({ failed: true, complete: false })
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items)).toContain('stale')
    expect((await db.listTodoRecords(NODE)).todos.map((t) => t.id)).toContain('stale')
  })
})

describe('claims with the Web Lock held', () => {
  it('a leftover claim by a dead instance is taken over whatever time it carries, even one stamped in the future', async () => {
    goOffline()
    await addTodo(NODE, { title: 'x', body: '' })
    const [rec] = await db.listOutbox(NODE)
    await db.patchOutbox(rec!.seq!, (r) => ({ ...r, claimed_by: 'dead', claimed_at: Date.now() + 3_600_000 }))
    expect(await db.claimNextOutbox(NODE, 'me', Date.now(), 0)).toMatchObject({ claimed_by: 'me' })
  })
})

describe('a poison record and the retry bookkeeping', () => {
  afterEach(() => {
    vi.restoreAllMocks()
    vi.useRealTimers()
  })

  /** 一条离线时记下的新增，上线后依次用 `reason` 让它的请求被掐断 `n` 次。 */
  async function dropped(n: number, reason: 'hub_lost' | 'peer_offline' | 'peer_reconnected' | 'noise_reset'): Promise<void> {
    goOffline()
    await addTodo(NODE, { title: 'poison', body: '' })
    await goOnline()
    for (let i = 0; i < n; i++) {
      daemon.failNext('todo.add', linkError('closed', reason))
      await replayOutbox(NODE)
    }
  }

  it('a record in flight while the node side keeps changing state (peer reconnected / noise reset) ends as failed after MAX_DROPS, not silently looping', async () => {
    goOffline()
    await addTodo(NODE, { title: 'poison', body: '' })
    await goOnline()
    for (let i = 1; i < MAX_DROPS; i++) {
      daemon.failNext('todo.add', linkError('closed', i % 2 ? 'peer_reconnected' : 'noise_reset'))
      await replayOutbox(NODE)
      expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'pending', drops: i })
    }
    daemon.failNext('todo.add', linkError('closed', 'peer_reconnected'))
    await replayOutbox(NODE)
    expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'failed', error: { code: 'link' } })
  })

  it('losing the hub connection or the peer going offline (coverage, a sleeping laptop) is never counted, however often it happens', async () => {
    await dropped(MAX_DROPS, 'hub_lost')
    await dropped(0, 'peer_offline')
    daemon.failNext('todo.add', linkError('closed', 'peer_offline'))
    await replayOutbox(NODE)
    daemon.failNext('todo.add', linkError('closed', 'peer_offline'))
    await replayOutbox(NODE)
    const rec = (await db.listOutbox(NODE))[0]!
    expect(rec).toMatchObject({ state: 'pending' })
    expect(rec.drops ?? 0).toBe(0)
  })

  it('Retry starts from zero: a record failed as poison is not failed again by one more ordinary drop', async () => {
    await dropped(MAX_DROPS, 'peer_reconnected')
    const failed = (await db.listOutbox(NODE))[0]!
    expect(failed).toMatchObject({ state: 'failed', drops: MAX_DROPS })
    daemon.failNext('todo.add', linkError('closed', 'peer_reconnected'))
    await retryOp(failed)
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]).toMatchObject({ state: 'pending', drops: 1 }))
  })

  it('the transport tells why it cut an in-flight request', async () => {
    const { RpcClient } = await import('@/transport/rpcClient')
    const client = new RpcClient(() => [1])
    const reasons: Array<string | undefined> = []
    for (const [msg, reason] of [['a', 'hub_lost'], ['b', 'peer_reconnected']] as const) {
      const p = client.request('x').catch((e: { closeReason?: string }) => reasons.push(e.closeReason))
      client.rejectAll('closed', msg, reason)
      await p
    }
    expect(reasons).toEqual(['hub_lost', 'peer_reconnected'])
  })

  it('the backoff starts over for the next record instead of continuing from the previous one', async () => {
    await syncTodos(NODE)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const delays: number[] = []
    const real = globalThis.setTimeout
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(((fn: () => void, ms?: number) => {
      if (ms !== undefined && ms >= RETRY_BASE_MS && ms <= RETRY_MAX_MS) delays.push(ms)
      return real(fn, ms)
    }) as typeof setTimeout)
    daemon.failNext('todo.add', linkError('timeout'))
    await addTodo(NODE, { title: 'one', body: '' })
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.attempts).toBe(1))
    await vi.advanceTimersByTimeAsync(RETRY_BASE_MS + 50)
    await vi.waitFor(() => expect(daemon.todos.size).toBe(1))
    await vi.waitFor(async () => expect(await db.listOutbox(NODE)).toEqual([]))
    daemon.failNext('todo.add', linkError('timeout'))
    await addTodo(NODE, { title: 'two', body: '' })
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.attempts).toBe(1))
    expect(delays.filter((d) => d === RETRY_BASE_MS)).toHaveLength(2)
    expect(delays).not.toContain(RETRY_BASE_MS * 2)
  })

  it('resetTodosMemory (sign-out) cancels pending retries: nothing is sent afterwards', async () => {
    await syncTodos(NODE)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    daemon.failNext('todo.add', linkError('timeout'))
    await addTodo(NODE, { title: 'slow', body: '' })
    await vi.waitFor(async () => expect((await db.listOutbox(NODE))[0]?.attempts).toBe(1))
    resetTodosMemory()
    daemon.log = []
    await vi.advanceTimersByTimeAsync(RETRY_MAX_MS * 2)
    expect(daemon.log).toEqual([])
  })
})

describe('forgetting a node also drops its unsaved edits', () => {
  it('drafts of that node go, drafts of other nodes stay', async () => {
    const v = { title: 'a', body: '', project: '', due: '' }
    useDrafts.getState().start(`${NODE}:a`, v)
    useDrafts.getState().start('other:b', v)
    vi.spyOn(client, 'getClient').mockReturnValue({ removeNode: vi.fn(async () => undefined) } as never)
    await forgetNode(NODE)
    expect(Object.keys(useDrafts.getState().drafts)).toEqual(['other:b'])
    useDrafts.setState({ drafts: {} })
  })
})

describe('a disabled local database is an expected state, not an error', () => {
  it('after the first occurrence the page stops reacting to the other tabs’ cache announcements and logs no warning each time', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const debug = vi.spyOn(console, 'debug').mockImplementation(() => undefined)
    db.disableDb()
    await reloadFromDb(NODE)
    await reloadFromDb(NODE)
    await reloadFromDb(NODE)
    expect(warn).not.toHaveBeenCalled()
    expect(debug).toHaveBeenCalledTimes(1)
    // 通道关掉了：别的标签页的通知到不了这里
    const other = new BroadcastChannel('anycon-todos')
    other.postMessage(NODE)
    await new Promise((r) => setTimeout(r, 20))
    other.close()
    expect(warn).not.toHaveBeenCalled()
    await db.resetDb()
    resetTodosMemory()
  })

  it('forgetting a node while the database is disabled does not raise a storage error either', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    useUi.setState({ storageError: false })
    db.disableDb()
    await forgetNodeTodos(NODE)
    expect(warn).not.toHaveBeenCalled()
    expect(useUi.getState().storageError).toBe(false)
    await db.resetDb()
  })
})


describe('closed walks and reconnects', () => {
  it('an expanded group that ended as “failed” starts over after a reconnect instead of sitting on the failure', async () => {
    daemon.seed(todoItem({ id: 'c1', state: 'done' }))
    await syncTodos(NODE)
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const original = rpc.handlers['todo.list']!
    let failOnce = true
    rpc.handlers['todo.list'] = async (p, n) => {
      if ((p as { include_closed?: boolean }).include_closed && failOnce) {
        failOnce = false
        throw linkError('timeout')
      }
      return original(p, n)
    }
    setClosedExpanded(NODE, true)
    await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed.failed).toBe(true))
    await syncTodos(NODE)
    await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed).toMatchObject({ complete: true, failed: false }))
    expect(Object.keys(useTodos.getState().nodes[NODE]!.items)).toContain('c1')
  })

  it('a stale walk that fails late does not overwrite the state of the newer walk', async () => {
    daemon.seed(todoItem({ id: 'c1', state: 'done' }))
    await syncTodos(NODE)
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const original = rpc.handlers['todo.list']!
    let release!: () => void
    const gate = new Promise<void>((r) => (release = r))
    let first = true
    rpc.handlers['todo.list'] = async (p, n) => {
      if ((p as { include_closed?: boolean }).include_closed && first) {
        first = false
        await gate
        throw linkError('closed', 'peer_offline')
      }
      return original(p, n)
    }
    setClosedExpanded(NODE, true) // 第一趟：卡在 gate 上
    await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed.loading).toBe(true))
    await syncTodos(NODE) // 重连：第一趟作废，展开着，重新翻
    await vi.waitFor(() => expect(useTodos.getState().nodes[NODE]!.closed.complete).toBe(true))
    release() // 第一趟现在才失败
    await flush()
    expect(useTodos.getState().nodes[NODE]!.closed).toMatchObject({ complete: true, failed: false, loading: false })
  })
})
