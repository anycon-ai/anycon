// 清单的网络和落盘：同步（翻页）、`todo.changed`、本地改动入队、离线重放（docs/spec/daemon.md § 21.6，spec/web.md § 4.12）。
//
// 所有改动（在线的也一样）都先进待发队列（IndexedDB `todo_outbox`）、立刻叠到界面上，再由重放发给节点：这样在线和离线是同一条路，
// 顺序只有一处保证。重放：
//   - 每个节点同一时刻只有一个标签页在重放（Web Locks，`ifAvailable`；没有 Web Locks 的环境退回本标签页内的互斥）；
//   - 按入队顺序一条一条发，上一条有了结果才发下一条；每条发之前先在库里标「认领」，之后入队的改动不会合并进正在发的那一条；
//   - 回执丢了 / 链路断了：这一条留着（取消认领），下次重放再发——`todo.add` 按 id 幂等，`todo.update` 重放的是同样的字段，
//     `todo.delete` 的 `not_found` 视为已完成，所以重发是安全的；同一项的改动之间有顺序，所以一条没有结果就不发它后面的；
//   - 结果：`not_found`（update / delete）丢掉这一条；重新打开已关闭的项得到 `invalid` / `too_many` 丢掉这一条并告诉用户；
//     别的 `invalid`（比如新建时清单满了）把这一条标成失败，项留在界面上，用户选重试或丢弃——用户输入的东西不会悄悄丢；
//     别的错误停下来等下一次（不标失败，也不打扰用户）；
//   - 全部发完之后再 `todo.list` 一次，以 daemon 的结果为准。
import type { Todo, TodoChanged, TodoListResult } from '@/protocol'
import * as db from '@/db'
import { storageFailed } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import i18n from '@/i18n'
import { planEnqueue } from './outbox'
import { nodeSupportsTodo, useTodos } from './store'
import type { OutboxError, OutboxOp, OutboxRecord, UpdateFields } from './types'
import { isClosed } from './types'
import { draftError, newTodoId, projectError, titleError, bodyError, dueError, type DraftTodo, type FieldError } from './validate'

// ---------------------------------------------------------------------------------------------------------------------
// 多标签页：库是唯一的真相，别的标签页改了就重读
// ---------------------------------------------------------------------------------------------------------------------
let channel: BroadcastChannel | null | undefined
function bc(): BroadcastChannel | null {
  if (channel === undefined) {
    try {
      channel = typeof BroadcastChannel === 'undefined' ? null : new BroadcastChannel('anycon-todos')
      if (channel) channel.onmessage = (e: MessageEvent<unknown>) => void reloadFromDb(typeof e.data === 'string' ? e.data : undefined)
    } catch {
      channel = null
    }
  }
  return channel
}
const announce = (nodeId: string): void => {
  try {
    bc()?.postMessage(nodeId)
  } catch (e) {
    console.debug('anycon: could not tell the other tabs about a to-do change', e)
  }
}

async function reloadOutbox(): Promise<void> {
  useTodos.getState().setOutbox(await db.listOutbox())
}

/** 从库里重读（别的标签页改了缓存或队列）。 */
export async function reloadFromDb(nodeId?: string): Promise<void> {
  try {
    await reloadOutbox()
    if (nodeId) {
      const { todos, synced_at } = await db.listTodoRecords(nodeId)
      useTodos.getState().patchNode(nodeId, (n) => ({ ...n, items: Object.fromEntries(todos.map((t) => [t.id, t])), hydrated: true, ...(synced_at !== undefined ? { syncedAt: synced_at } : {}) }))
    }
  } catch (e) {
    console.warn('anycon: could not re-read the to-do cache', e)
  }
}

/** 启动时：读待发队列和每个节点的缓存（离线也有东西可看；没读完之前界面不说「没有清单项」）。 */
export async function hydrateTodos(nodeIds: readonly string[]): Promise<void> {
  try {
    await reloadOutbox()
  } catch (e) {
    console.warn('anycon: could not read the pending to-do changes', e)
  }
  await Promise.all(
    nodeIds.map(async (nodeId) => {
      try {
        const { todos, synced_at } = await db.listTodoRecords(nodeId)
        useTodos.getState().setCache(nodeId, todos, synced_at)
      } catch (e) {
        console.warn('anycon: could not read the cached to-dos', e)
        useTodos.getState().patchNode(nodeId, (n) => ({ ...n, hydrated: true }))
      }
    }),
  )
}

/** 节点被忘记：缓存和还没同步的改动一起丢掉。返回丢掉的待发改动数（调用方可以告诉用户）。 */
export async function forgetNodeTodos(nodeId: string): Promise<number> {
  useTodos.getState().removeNode(nodeId)
  try {
    const n = await db.dropNodeTodos(nodeId)
    announce(nodeId)
    return n
  } catch (e) {
    storageFailed('drop the to-dos of a forgotten node')(e)
    return 0
  }
}

/** 退出登录：内存里的清单清掉（IndexedDB 保留，同一账号再登录还能用；「退出并移除本设备」才删库）。 */
export function resetTodosMemory(): void {
  useTodos.setState({ nodes: {}, outbox: [], outboxLoaded: false })
}

// ---------------------------------------------------------------------------------------------------------------------
// 同步
// ---------------------------------------------------------------------------------------------------------------------
const PAGE = 200

/**
 * 连上节点之后：先重放待发改动，再把没关闭的项一页页翻完（`has_more` 为假为止），以 daemon 的结果为准。
 * 链路断了（离线）抛出，由 `syncNode` 当作「会再同步一次」。节点不支持清单时什么都不做。
 */
export async function syncTodos(nodeId: string): Promise<void> {
  if (!nodeSupportsTodo(nodeId)) return
  useTodos.getState().patchNode(nodeId, (n) => ({ ...n, syncing: true, touched: {} }))
  try {
    await reloadOutbox().catch(() => undefined)
    const replay = await replayOutbox(nodeId)
    // 重放被打断（节点又掉线、被拒之外的错误）就不列了：队列还在，下次连上接着来
    if (replay === 'offline') throw new RpcCallError('offline', 'todo replay: node link not ready')
    const fetched = await listPages(nodeId, false)
    const now = Date.now()
    const before = Object.keys(useTodos.getState().nodes[nodeId]?.items ?? {})
    useTodos.getState().replaceOpen(nodeId, fetched, now)
    // 缓存里有、这次没有的没关闭的项：已经在别处被删或关闭了，从缓存里也去掉
    const after = useTodos.getState().nodes[nodeId]?.items ?? {}
    await db.applyTodoCache(nodeId, { put: fetched, remove: before.filter((id) => !(id in after)), syncedAt: now }).catch(storageFailed('cache the to-do list'))
    announce(nodeId)
  } finally {
    useTodos.getState().patchNode(nodeId, (n) => ({ ...n, syncing: false }))
  }
}

async function listPages(nodeId: string, includeClosed: boolean, onPage?: (todos: Todo[]) => void): Promise<Todo[]> {
  const all = new Map<string, Todo>()
  let before: { updated_ms: number; id: string } | undefined
  for (let guard = 0; guard < 1000; guard++) {
    const res = await rpc().request<TodoListResult>(nodeId, 'todo.list', { ...(includeClosed ? { include_closed: true } : {}), limit: PAGE, ...(before ? { before } : {}) })
    for (const t of res.todos) all.set(t.id, t)
    onPage?.(res.todos)
    const last = res.todos.at(-1)
    if (!res.has_more || !last) break
    before = { updated_ms: last.updated_ms, id: last.id }
  }
  return [...all.values()]
}

/** 用户展开「已关闭」：向节点要已关闭的项，一页页往下翻（`more` = 继续翻下一页）。 */
export async function loadClosed(nodeId: string, opts: { more?: boolean } = {}): Promise<void> {
  const st = useTodos.getState().nodes[nodeId]
  if (!st || st.closed.loading || !nodeSupportsTodo(nodeId)) return
  if (opts.more ? !st.closed.hasMore : st.closed.complete) return
  const { patchNode, mergeItems } = useTodos.getState()
  patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: true, failed: false } }))
  try {
    const cursor = opts.more ? st.closed.cursor : undefined
    const res = await rpc().request<TodoListResult>(nodeId, 'todo.list', { include_closed: true, limit: PAGE, ...(cursor ? { before: cursor } : {}) })
    mergeItems(nodeId, res.todos)
    void db.applyTodoCache(nodeId, { put: res.todos.filter((t) => isClosed(t.state)) }).catch(storageFailed('cache the closed to-dos'))
    const last = res.todos.at(-1)
    patchNode(nodeId, (n) => ({
      ...n,
      closed: { ...n.closed, loading: false, hasMore: res.has_more && !!last, complete: !res.has_more || !last, ...(last ? { cursor: { updated_ms: last.updated_ms, id: last.id } } : {}) },
    }))
  } catch (e) {
    console.warn('anycon: could not load the closed to-dos', e)
    patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: false, failed: true } }))
  }
}

export function setClosedExpanded(nodeId: string, expanded: boolean): void {
  useTodos.getState().patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, expanded } }))
  if (expanded) void loadClosed(nodeId)
}

/** `todo.changed`：并入缓存（同一个 id 只留一份；`deleted` 删掉），并落盘。 */
export function handleTodoChanged(nodeId: string, raw: unknown): void {
  if (typeof raw !== 'object' || raw === null) return
  const ch = raw as Partial<TodoChanged>
  if (typeof ch.id !== 'string' || ch.id === '') return
  if (!nodeSupportsTodo(nodeId)) return
  const valid: TodoChanged = { id: ch.id, ...(ch.deleted ? { deleted: true } : {}), ...(ch.todo ? { todo: ch.todo } : {}) }
  if (!valid.deleted && (!valid.todo || valid.todo.id !== valid.id)) return
  useTodos.getState().applyChanged(nodeId, valid)
  const write = valid.deleted ? db.applyTodoCache(nodeId, { remove: [valid.id] }) : db.applyTodoCache(nodeId, { put: [valid.todo!] })
  write.catch(storageFailed('cache a to-do change'))
  announce(nodeId)
}

// ---------------------------------------------------------------------------------------------------------------------
// 本地改动：入队 → 叠到界面 → 重放
// ---------------------------------------------------------------------------------------------------------------------
export type ActionResult = { ok: true; id: string } | { ok: false; error: FieldError }

async function enqueue(nodeId: string, id: string, op: OutboxOp): Promise<void> {
  const now = Date.now()
  await db.mutateOutbox(nodeId, id, (existing) => planEnqueue(nodeId, id, existing, op, now))
  await reloadOutbox()
  announce(nodeId)
  void replayOutbox(nodeId).then((r) => {
    if (r === 'done') void refreshAfterReplay(nodeId)
  })
}

/** 新建一项（`todo.add`，id 由客户端生成）。校验不过返回错误，什么都不入队。节点离线也照常记。 */
export async function addTodo(nodeId: string, draft: DraftTodo): Promise<ActionResult> {
  const error = draftError(draft)
  if (error) return { ok: false, error }
  const id = newTodoId()
  const project = draft.project?.trim()
  const params = { title: draft.title.trim(), body: draft.body, ...(project ? { project } : {}), ...(draft.due_ms != null ? { due_ms: draft.due_ms } : {}) }
  await enqueue(nodeId, id, { kind: 'add', params })
  return { ok: true, id }
}

/** 改一项里的几个字段（只带改了的）。校验同新建。 */
export async function updateTodo(nodeId: string, id: string, fields: UpdateFields): Promise<ActionResult> {
  const error = (fields.title !== undefined ? titleError(fields.title) : null) ?? (fields.body !== undefined ? bodyError(fields.body) : null) ?? projectError(fields.project) ?? dueError(fields.due_ms)
  if (error) return { ok: false, error }
  const f: UpdateFields = { ...fields }
  if (f.title !== undefined) f.title = f.title.trim()
  if (f.project !== undefined && f.project !== null) {
    const p = f.project.trim()
    if (p === '') {
      delete f.project
      f.clear = [...new Set([...(f.clear ?? []), 'project' as const])]
    } else f.project = p
  }
  await enqueue(nodeId, id, { kind: 'update', fields: f })
  return { ok: true, id }
}

export async function deleteTodo(nodeId: string, id: string): Promise<void> {
  await enqueue(nodeId, id, { kind: 'delete' })
}

/** 被拒的改动：重试（重新排队）。 */
export async function retryOp(rec: OutboxRecord): Promise<void> {
  if (rec.seq === undefined) return
  const { error: _e, claimed_at: _c, ...rest } = rec
  await db.putOutbox({ ...rest, state: 'pending' })
  await reloadOutbox()
  announce(rec.node_id)
  void replayOutbox(rec.node_id).then((r) => {
    if (r === 'done') void refreshAfterReplay(rec.node_id)
  })
}

/** 被拒的改动：丢弃（这一项里排在它后面的待发改动一起丢，它们依赖它）。 */
export async function discardOp(rec: OutboxRecord): Promise<void> {
  const all = await db.listOutbox(rec.node_id)
  const seqs = all.filter((r) => r.id === rec.id && (r.seq ?? 0) >= (rec.seq ?? 0)).map((r) => r.seq!)
  await db.deleteOutbox(seqs)
  await reloadOutbox()
  announce(rec.node_id)
}

// ---------------------------------------------------------------------------------------------------------------------
// 重放
// ---------------------------------------------------------------------------------------------------------------------
export type ReplayResult = 'done' | 'offline' | 'blocked'

const memoryChains = new Map<string, Promise<unknown>>()

/**
 * 同一个节点的重放排队执行：Web Locks（跨标签页）；没有 Web Locks 的环境退回本标签页内的串行。
 * 是排队等，不是「抢不到就算了」：一个重放结束前一瞬间入队的新改动，由排在后面的那一轮发出去，不会漏掉；
 * 每一轮开头都重新读库，所以前一轮已经发掉的不会再发。
 */
async function withNodeLock<T>(nodeId: string, fn: () => Promise<T>): Promise<T> {
  const locks = typeof navigator === 'undefined' ? undefined : navigator.locks
  if (locks?.request) return locks.request(`anycon-todo-replay:${nodeId}`, () => fn())
  const prev = memoryChains.get(nodeId) ?? Promise.resolve()
  const run = prev.then(fn, fn)
  memoryChains.set(nodeId, run.catch(() => undefined))
  return run
}

/** 一条重放得到的错误该怎么办。 */
export type ReplayVerdict = { kind: 'drop' } | { kind: 'drop_full' } | { kind: 'failed'; error: OutboxError } | { kind: 'transient' } | { kind: 'stop' }

export function classifyReplayError(op: OutboxOp, e: unknown): ReplayVerdict {
  if (!(e instanceof RpcCallError)) return { kind: 'stop' }
  // 链路问题（离线、断开、没送达、超时）：这一条没有结果，留着下次再发
  if (e.kind !== 'rpc') return { kind: 'transient' }
  const code = e.code
  if (code === 'not_found') return op.kind === 'add' ? { kind: 'stop' } : { kind: 'drop' }
  if (code === 'invalid') {
    if (op.kind === 'update' && e.reason === 'too_many' && op.fields.state !== undefined && !isClosed(op.fields.state)) return { kind: 'drop_full' }
    if (op.kind === 'delete') return { kind: 'drop' }
    return { kind: 'failed', error: { code: 'invalid', ...(e.field ? { field: e.field } : {}), ...(e.reason ? { reason: e.reason } : {}) } }
  }
  return { kind: 'stop' }
}

async function sendOp(nodeId: string, rec: OutboxRecord): Promise<Todo | null> {
  const op = rec.op
  if (op.kind === 'add') return rpc().request<Todo>(nodeId, 'todo.add', { id: rec.id, title: op.params.title, body: op.params.body, ...(op.params.project ? { project: op.params.project } : {}), ...(op.params.due_ms != null ? { due_ms: op.params.due_ms } : {}) })
  if (op.kind === 'update') return rpc().request<Todo>(nodeId, 'todo.update', { id: rec.id, ...op.fields })
  await rpc().request(nodeId, 'todo.delete', { id: rec.id })
  return null
}

/** 按入队顺序重放这个节点上的待发改动。链路没就绪时什么都不发。 */
export async function replayOutbox(nodeId: string): Promise<ReplayResult> {
  if (!nodeSupportsTodo(nodeId)) return 'blocked'
  return withNodeLock<ReplayResult>(nodeId, async () => {
    // 持锁的就是我：库里残留的「认领」是崩掉 / 关掉的标签页留下的，放掉
    for (const r of await db.listOutbox(nodeId)) if (r.claimed_at !== undefined) await db.putOutbox(stripClaim(r))
    for (;;) {
      if (!rpc().isReady(nodeId)) return 'offline'
      const ops = await db.listOutbox(nodeId)
      // 一项里有被拒的改动时，它后面的改动先不发（它们依赖它）；别的项照常
      const stuck = new Set(ops.filter((r) => r.state === 'failed').map((r) => r.id))
      const next = ops.find((r) => r.state === 'pending' && !stuck.has(r.id))
      if (!next) return 'done'
      await db.putOutbox({ ...next, claimed_at: Date.now() })
      try {
        const todo = await sendOp(nodeId, next)
        if (todo) {
          useTodos.getState().mergeItems(nodeId, [todo])
          await db.applyTodoCache(nodeId, { put: [todo] }).catch(storageFailed('cache a to-do'))
        } else {
          useTodos.getState().applyChanged(nodeId, { id: next.id, deleted: true })
          await db.applyTodoCache(nodeId, { remove: [next.id] }).catch(storageFailed('cache a to-do deletion'))
        }
        await db.deleteOutbox([next.seq!])
      } catch (e) {
        const v = classifyReplayError(next.op, e)
        if (v.kind === 'drop' || v.kind === 'drop_full') {
          await db.deleteOutbox([next.seq!])
          // not_found：这一项已经不在 daemon 上了（别处删了）：缓存里也去掉，不等下一次整份列表
          if (v.kind === 'drop' && next.op.kind !== 'add') {
            useTodos.getState().applyChanged(nodeId, { id: next.id, deleted: true })
            await db.applyTodoCache(nodeId, { remove: [next.id] }).catch(storageFailed('cache a to-do deletion'))
          }
          if (v.kind === 'drop_full') toast.error(i18n.t('todos.replay.full'))
        } else if (v.kind === 'failed') {
          await db.putOutbox({ ...stripClaim(next), state: 'failed', error: v.error })
        } else {
          await db.putOutbox(stripClaim(next))
          await reloadOutbox().catch(() => undefined)
          announce(nodeId)
          return v.kind === 'transient' ? 'offline' : 'blocked'
        }
      }
      await reloadOutbox().catch(() => undefined)
      announce(nodeId)
    }
  })
}

function stripClaim(r: OutboxRecord): OutboxRecord {
  const { claimed_at: _c, ...rest } = r
  return rest
}

/** 重放结束后再列一次（只翻没关闭的），以 daemon 的结果为准。 */
export async function refreshAfterReplay(nodeId: string): Promise<void> {
  if (!rpc().isReady(nodeId) || !nodeSupportsTodo(nodeId)) return
  try {
    const fetched = await listPages(nodeId, false)
    useTodos.getState().replaceOpen(nodeId, fetched, Date.now())
    await db.applyTodoCache(nodeId, { put: fetched, syncedAt: Date.now() }).catch(storageFailed('cache the to-do list'))
  } catch (e) {
    console.debug('anycon: could not list the to-dos after replaying', e)
  }
}

/** 节点现在能发请求（界面上「未同步」的标记据此）。 */
export function nodeReady(nodeId: string): boolean {
  return useNodes.getState().byId[nodeId]?.conn === 'ready'
}

export { newTodoId }
