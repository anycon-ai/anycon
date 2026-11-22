// 清单的网络和落盘：同步（翻页）、`todo.changed`、本地改动入队、重放（docs/spec/daemon.md § 21.6，spec/web.md § 4.12）。
//
// 待发队列（IndexedDB `todo_outbox`）是只追加的有序日志：用户的每个动作（在线的也一样）追加一条、立刻叠到界面上，再由重放发给节点。
// 不合并、不取消：回执丢了的时候，合并进已经被 daemon 收下的 `add`、或取消一个其实已经到了的 `add`，都会让 daemon 和界面对不上。
// 重放：
//   - 每个节点同一时刻只有一个实例在重放：Web Locks（`anycon-todo-replay:<node>`，**排队等**，不是抢不到就算了）；
//     没有 Web Locks 的环境，本标签页内用一条串行链，跨标签页靠「租约」：认领带实例 id 和时间，别的实例只接手超过
//     `RPC_TIMEOUT_MS + 余量` 的认领，没人在重放开头清掉别人还活着的认领；
//   - 一项一条队：每一项只发它最前面的那一条，前面的没完成不发后面的；一项里的一条失败（被拒 / 反复发不出去）这一项就等着，别的项照常发；
//   - 认领和「选下一条」在同一个读写事务里完成，只改库里现在那条记录的认领字段（不写回之前读到的快照，入队 / 重试的改动不会被盖掉）；
//   - 结果：成功删掉这一条；`not_found`（update / delete）丢掉；重新打开已关闭的项得到 `invalid` / `too_many`：去掉 `state` 再发
//     （只剩 `state` 就丢掉），项仍是关闭的，告诉用户清单满了；别的 `invalid`、别的节点错误、链路错误连续 `MAX_LINK_FAILURES` 次：标成失败，
//     项留在界面上标「未保存」，用户选重试或丢弃，同一项后面的改动等着；纯链路 / 离线错误：这一条留着，这一轮结束；
//   - 只有重放时有入队时节点连不上的记录，才在重放完之后重新 `todo.list`；在线时的改动靠返回的 `Todo` 和 `todo.changed`，不整份重列。
import type { Todo, TodoChanged, TodoListResult } from '@/protocol'
import * as db from '@/db'
import { RPC_TIMEOUT_MS } from '@/lib/constants'
import { storageFailed } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { navigateTo } from '@/lib/navigate'
import { shortNodeId } from '@/lib/ids'
import { toast, useUi } from '@/stores/ui'
import i18n from '@/i18n'
import { toastIfReady } from './reviewToast'
import { useTodoHistory } from './history'
import { useReworkDrafts } from './reworkDraft'
import { nodeSupportsTodo, useTodos, viewOf } from './store'
import { outboxErrorText } from './ui/errors'
import type { OutboxError, OutboxOp, OutboxRecord, UpdateFields } from './types'
import { isClosed } from './types'
import { cleanOneLine, draftError, newTodoId, tabsToSpaces, projectError, titleError, bodyError, dueError, repeatFieldError, type DraftTodo, type FieldError } from './validate'

// ---------------------------------------------------------------------------------------------------------------------
// 多标签页：库是唯一的真相，别的标签页改了就重读
// ---------------------------------------------------------------------------------------------------------------------
let channel: BroadcastChannel | null | undefined
/** 本地库已经被禁用（别的标签页移除了这台设备 / 升级了库）：这个页面不会再读写它，别的标签页的缓存通知也不再理会。 */
let dbGone = false
const isDbGone = (e: unknown): boolean => e instanceof db.DbDisabledError
function bc(): BroadcastChannel | null {
  if (dbGone) return null
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
/** 关掉跨标签页的通知通道（测试收尾用；库被禁用时内部也会调用）。 */
export function closeTodoChannel(): void {
  channel?.close()
  channel = undefined
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
  if (dbGone) return
  try {
    await reloadOutbox()
    if (nodeId) {
      const { todos, synced_at } = await db.listTodoRecords(nodeId)
      useTodos.getState().patchNode(nodeId, (n) => ({ ...n, items: Object.fromEntries(todos.map((t) => [t.id, t])), hydrated: true, ...(synced_at !== undefined ? { syncedAt: synced_at } : {}) }))
    }
  } catch (e) {
    if (isDbGone(e)) {
      // 预期的状态：只提一次，之后不再理会别的标签页的缓存通知
      dbGone = true
      closeTodoChannel()
      console.debug('anycon: the local database is disabled (removed or upgraded in another tab); to-do cache announcements are ignored from now on')
      return
    }
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
  cancelRetry(nodeId)
  retryCounts.delete(nodeId)
  retryFor.delete(nodeId)
  closedWalks.delete(nodeId)
  for (const k of [...changeTicks.keys()]) if (k.startsWith(`${nodeId}:`)) changeTicks.delete(k)
  useTodos.getState().removeNode(nodeId)
  useTodoHistory.getState().dropNode(nodeId)
  useReworkDrafts.getState().clearNode(nodeId)
  try {
    const n = await db.dropNodeTodos(nodeId)
    announce(nodeId)
    return n
  } catch (e) {
    if (!isDbGone(e)) storageFailed('drop the to-dos of a forgotten node')(e)
    return 0
  }
}

/** 退出登录：内存里的清单清掉（IndexedDB 保留，同一账号再登录还能用；「退出并移除本设备」才删库）。 */
export function resetTodosMemory(): void {
  dbGone = false
  useTodoHistory.getState().reset()
  useReworkDrafts.getState().reset()
  closeTodoChannel()
  for (const id of [...retryTimers.keys()]) cancelRetry(id)
  retryCounts.clear()
  retryFor.clear()
  closedWalks.clear()
  paging.clear()
  changeTicks.clear()
  useTodos.setState({ nodes: {}, outbox: [], outboxLoaded: false })
}


// ---------------------------------------------------------------------------------------------------------------------
// 串行：同一个键（节点）的任务排队执行
// ---------------------------------------------------------------------------------------------------------------------
const chains = new Map<string, Promise<unknown>>()
function serial<T>(key: string, fn: () => Promise<T>): Promise<T> {
  const prev = chains.get(key) ?? Promise.resolve()
  const run = prev.then(fn, fn)
  chains.set(
    key,
    run.catch(() => undefined),
  )
  return run
}

// ---------------------------------------------------------------------------------------------------------------------
// 同步
// ---------------------------------------------------------------------------------------------------------------------
const PAGE = 200

/** 翻页期间变过的项：`todo.changed` 和本地动作记在这里（`null` = 删掉了，墓碑），翻完时它们比翻到的旧页新，不被盖掉、不被加回来。 */
const paging = new Map<string, Map<string, Todo | null>>()
/** 「已关闭」从头翻到尾的一趟：翻到过的 id，和这一趟期间变过 / 删掉的项。翻完时用它修剪缓存里已关闭的项。 */
const closedWalks = new Map<string, { seen: Set<string>; touched: Map<string, Todo | null> }>()
function noteTouched(nodeId: string, id: string, todo: Todo | null): void {
  paging.get(nodeId)?.set(id, todo)
  closedWalks.get(nodeId)?.touched.set(id, todo)
}

/**
 * 连上节点之后：先重放待发改动，再把没关闭的项一页页翻完（`has_more` 为假为止），以 daemon 的结果为准。
 * 链路断了（离线）抛出，由 `syncNode` 当作「会再同步一次」。节点不支持清单时什么都不做。
 */
export async function syncTodos(nodeId: string): Promise<void> {
  if (!nodeSupportsTodo(nodeId)) return
  // 新的一次连接：上一次连接里翻到一半的「已关闭」作废（断线期间别处改过的项没有 `todo.changed`，不能让半趟的记录修剪它们）；展开着就重新翻
  closedWalks.delete(nodeId)
  const st = useTodos.getState().nodes[nodeId]
  if (st && (st.closed.loading || st.closed.hasMore || st.closed.complete || st.closed.failed)) {
    useTodos.getState().patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: false, failed: false, complete: false, hasMore: false, cursor: undefined } }))
    if (st.closed.expanded) void loadClosed(nodeId)
  }
  await reloadOutbox().catch(() => undefined)
  const replay = await replayOutbox(nodeId)
  // 重放被打断（节点又掉线）就不列了：队列还在，下次连上接着来
  if (replay === 'offline') throw new RpcCallError('offline', 'todo replay: node link not ready')
  await listOpen(nodeId)
}

/**
 * 唯一的列表实现（首次同步和重放之后的刷新共用），同一个节点一次只翻一份。翻页期间被 `todo.changed` 或本地动作改过 / 删掉的项
 * 不会被翻到的旧页盖掉或加回来，期间新建的项保留；翻完之后，daemon 没有了的没关闭的项从内存和 IndexedDB 里一起去掉。
 */
function listOpen(nodeId: string): Promise<void> {
  return serial(`list:${nodeId}`, async () => {
    const touched = new Map<string, Todo | null>()
    paging.set(nodeId, touched)
    useTodos.getState().patchNode(nodeId, (n) => ({ ...n, syncing: true }))
    try {
      const fetched = await listPages(nodeId)
      // 翻到的页和翻页期间变过的项合并：墓碑删掉、期间新建 / 更新的（不比翻到的旧）保留
      const merge = (): Todo[] => {
        const result = new Map<string, Todo>()
        for (const t of fetched) result.set(t.id, t)
        for (const [id, t] of touched) {
          if (t === null) result.delete(id)
          else {
            const f = result.get(id)
            if (!f || f.updated_ms <= t.updated_ms) result.set(id, t)
          }
        }
        return [...result.values()]
      }
      const now = Date.now()
      // 先写盘（一个事务里读—改—写，`todo.changed` 自己的写入排在它后面，不会被旧的覆盖），再换内存，最后才通知别的标签页：
      // 别的标签页收到通知时盘上已经是新的，不会读到旧内容把已删除的项又带回来
      try {
        await db.commitOpenList(nodeId, merge, now)
      } catch (e) {
        storageFailed('cache the to-do list')(e)
      }
      // 写盘期间又到的变更（它们各自的写入在后面）也要进内存里的结果
      useTodos.getState().replaceOpen(nodeId, merge(), now)
      announce(nodeId)
    } finally {
      paging.delete(nodeId)
      useTodos.getState().patchNode(nodeId, (n) => ({ ...n, syncing: false }))
    }
  })
}

async function listPages(nodeId: string): Promise<Todo[]> {
  const all: Todo[] = []
  let before: { updated_ms: number; id: string } | undefined
  for (let guard = 0; guard < 1000; guard++) {
    const res = await rpc().request<TodoListResult>(nodeId, 'todo.list', { limit: PAGE, ...(before ? { before } : {}) })
    all.push(...res.todos)
    const last = res.todos.at(-1)
    if (!res.has_more || !last) break
    before = { updated_ms: last.updated_ms, id: last.id }
  }
  return all
}

/** 重放之后重新列一次（只翻没关闭的），以 daemon 的结果为准。 */
export async function refreshAfterReplay(nodeId: string): Promise<void> {
  if (!rpc().isReady(nodeId) || !nodeSupportsTodo(nodeId)) return
  try {
    await listOpen(nodeId)
  } catch (e) {
    console.debug('anycon: could not list the to-dos after replaying', e)
  }
}

/**
 * 用户展开「已关闭」：向节点要已关闭的项，一页页往下翻（`more` = 继续翻下一页）。从头翻到尾翻完一趟，缓存里（内存和 IndexedDB）
 * 这一趟没翻到的已关闭的项（别处删了）修剪掉；这一趟期间删掉的项不会被翻到的页加回来，期间新关闭的保留。
 */
export async function loadClosed(nodeId: string, opts: { more?: boolean } = {}): Promise<void> {
  const st = useTodos.getState().nodes[nodeId]
  if (!st || st.closed.loading || !nodeSupportsTodo(nodeId)) return
  if (opts.more ? !st.closed.hasMore : st.closed.complete) return
  const { patchNode, mergeItems } = useTodos.getState()
  patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: true, failed: false } }))
  // 一趟只在同一次连接里走完才修剪：没有这一趟的记录（断线重连清掉了）就从头翻
  const fresh = !opts.more || !closedWalks.has(nodeId)
  if (fresh) closedWalks.set(nodeId, { seen: new Set(), touched: new Map() })
  const walk = closedWalks.get(nodeId)!
  try {
    const cursor = fresh ? undefined : st.closed.cursor
    const res = await rpc().request<TodoListResult>(nodeId, 'todo.list', { include_closed: true, limit: PAGE, ...(cursor ? { before: cursor } : {}) })
    // 这一趟期间在本地或经 `todo.changed` 删掉的项不能被这一页加回来
    // 这一趟已经作废（断线重连清掉了、或者被新的一趟取代）：迟到的结果不用
    if (closedWalks.get(nodeId) !== walk) {
      patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: false } }))
      return
    }
    const live = res.todos.filter((t) => walk.touched.get(t.id) !== null)
    for (const t of live) walk.seen.add(t.id)
    mergeItems(nodeId, live)
    const last = res.todos.at(-1)
    const finished = !res.has_more || !last
    let prune: string[] = []
    if (finished) {
      const items = useTodos.getState().nodes[nodeId]?.items ?? {}
      prune = Object.values(items)
        .filter((t) => isClosed(t.state) && !walk.seen.has(t.id) && !walk.touched.has(t.id))
        .map((t) => t.id)
      if (prune.length > 0) useTodos.getState().removeItems(nodeId, prune)
      closedWalks.delete(nodeId)
    }
    try {
      await db.applyTodoCache(nodeId, { put: live.filter((t) => isClosed(t.state)), remove: prune })
      if (finished) await db.pruneClosedCache(nodeId, walk.seen, walk.touched)
    } catch (e) {
      storageFailed('cache the closed to-dos')(e)
    }
    patchNode(nodeId, (n) => ({
      ...n,
      closed: { ...n.closed, loading: false, hasMore: res.has_more && !!last, complete: finished, ...(last ? { cursor: { updated_ms: last.updated_ms, id: last.id } } : {}) },
    }))
  } catch (e) {
    console.warn('anycon: could not load the closed to-dos', e)
    // 这一趟已经被取代（重连、新的一趟）：它的失败不能盖掉新一趟的状态
    if (closedWalks.get(nodeId) !== walk) {
      return
    }
    // 翻到一半失败：这一趟作废（不修剪），下次从头翻
    closedWalks.delete(nodeId)
    patchNode(nodeId, (n) => ({ ...n, closed: { ...n.closed, loading: false, failed: true, complete: false, hasMore: false, cursor: undefined } }))
  }
}

/**
 * 把「已关闭」从头到尾翻完一趟（不需要用户展开）。「不在没关闭的列表里」不等于「被删了」：别处关掉的项也不在里面，
 * 只有翻完已关闭的也没找到，才能说它被删了。翻到一半失败（离线）就停下，`closed.complete` 仍是假。
 */
export async function completeClosedWalk(nodeId: string): Promise<void> {
  let more = false
  for (let i = 0; i < 1000; i++) {
    for (let w = 0; w < 200 && useTodos.getState().nodes[nodeId]?.closed.loading; w++) await new Promise((r) => setTimeout(r, 10))
    await loadClosed(nodeId, { more })
    const c = useTodos.getState().nodes[nodeId]?.closed
    if (!c || c.failed || c.complete || !c.hasMore) return
    more = true
  }
}

export function setClosedExpanded(nodeId: string, expanded: boolean): void {
  // 每次展开都从头翻一趟：翻完才能把缓存里别处已经删掉的已关闭的项修剪掉（翻的过程中先显示缓存里的）
  useTodos.getState().patchNode(nodeId, (n) => ({ ...n, closed: expanded ? { ...n.closed, expanded, complete: false, hasMore: false, failed: false } : { ...n.closed, expanded } }))
  if (expanded) void loadClosed(nodeId)
}

/** `todo.changed`：并入缓存（同一个 id 只留一份；`deleted` 删掉），并落盘。 */
/**
 * `todo.changed` 的到达序号：每收到并入一条就加一，记在那一项上。直接发给节点的请求（派发、`todo.get`…）发出前取一次 `todoTickNow()`，
 * 回执回来时 `applyConfirmedTodo` 用它判断「请求发出之后有没有收到过这一项的通知」。回执是请求处理那一刻的快照，
 * 通知可能是它之后的状态（比如派发完、会话这一轮很快结束，`activity` 已经又变成 review）：这时回执不能盖掉通知带来的内容。
 */
let tickCounter = 0
const changeTicks = new Map<string, number>()
export const todoTickNow = (): number => tickCounter
const todoChangeTick = (nodeId: string, id: string): number => changeTicks.get(`${nodeId}:${id}`) ?? 0

export function handleTodoChanged(nodeId: string, raw: unknown): void {
  if (typeof raw !== 'object' || raw === null) return
  const ch = raw as Partial<TodoChanged>
  if (typeof ch.id !== 'string' || ch.id === '') return
  if (!nodeSupportsTodo(nodeId)) return
  const valid: TodoChanged = { id: ch.id, ...(ch.deleted ? { deleted: true } : {}), ...(ch.todo ? { todo: ch.todo } : {}) }
  if (!valid.deleted && (!valid.todo || valid.todo.id !== valid.id)) return
  const before = useTodos.getState().nodes[nodeId]?.items[valid.id]
  changeTicks.set(`${nodeId}:${valid.id}`, ++tickCounter)
  useTodos.getState().applyChanged(nodeId, valid)
  if (!valid.deleted) toastIfReady(nodeId, before, valid.todo!)
  noteTouched(nodeId, valid.id, valid.deleted ? null : valid.todo!)
  const write = valid.deleted ? db.applyTodoCache(nodeId, { remove: [valid.id] }) : db.applyTodoCache(nodeId, { put: [valid.todo!] })
  write.catch(storageFailed('cache a to-do change'))
  announce(nodeId)
}

// ---------------------------------------------------------------------------------------------------------------------
// 本地改动：追加到队列 → 叠到界面 → 重放
// ---------------------------------------------------------------------------------------------------------------------
export type ActionResult = { ok: true; id: string } | { ok: false; error: FieldError }
const STORAGE_ERROR: FieldError = { field: 'storage', reason: 'failed' }

/**
 * 追加一条。写库失败（配额、库被别的标签页删掉 / 升级了）返回错误，什么都没入队，调用方保留用户的输入；
 * 写成功之后的任何一步出错都不再报错（记录已经在队列里了，再报错会让用户重发一份）。
 */
async function enqueue(nodeId: string, id: string, op: OutboxOp, opts: { discardFailed?: boolean } = {}): Promise<FieldError | null> {
  try {
    const rec: OutboxRecord = { node_id: nodeId, id, op, created_at: Date.now(), state: 'pending', ...(nodeReady(nodeId) ? {} : { queued_offline: true }) }
    // 删除一项时，它已有的失败记录和排在它们后面的记录一起丢掉再追加删除：不能留着发不出去的记录、又把项藏起来
    if (opts.discardFailed) await db.appendDelete(rec)
    else await db.appendOutbox(rec)
  } catch (e) {
    storageFailed('queue a to-do change')(e)
    return STORAGE_ERROR
  }
  try {
    await reloadOutbox()
    announce(nodeId)
  } catch (e) {
    console.warn('anycon: the to-do change was saved but the list could not be refreshed', e)
  }
  kickReplay(nodeId)
  return null
}

/** 后台重放；有入队时节点连不上的记录才在重放完之后重新列一次。 */
function kickReplay(nodeId: string): void {
  cancelRetry(nodeId)
  void runReplay(nodeId)
    .then((r) => (r.result === 'done' && r.relist ? refreshAfterReplay(nodeId) : undefined))
    .catch((e: unknown) => console.warn('anycon: replaying the to-do changes failed', e))
}

/** 新建一项（`todo.add`，id 由客户端生成）。校验不过返回错误，什么都不入队。节点离线也照常记。 */
export async function addTodo(nodeId: string, draft: DraftTodo): Promise<ActionResult> {
  draft = { ...draft, title: tabsToSpaces(draft.title), ...(draft.project != null ? { project: tabsToSpaces(draft.project) } : {}) }
  const error = draftError(draft)
  if (error) return { ok: false, error }
  const id = newTodoId()
  const project = draft.project?.trim()
  const params = { ...(draft.handler ? { handler: draft.handler } : {}), title: draft.title.trim(), body: draft.body, ...(project ? { project } : {}), ...(draft.due_ms != null ? { due_ms: draft.due_ms } : {}), ...(draft.repeat ? { repeat: draft.repeat } : {}) }
  const failed = await enqueue(nodeId, id, { kind: 'add', params })
  return failed ? { ok: false, error: failed } : { ok: true, id }
}

/** 改一项里的几个字段（只带改了的）。校验同新建。 */
export async function updateTodo(nodeId: string, id: string, fields: UpdateFields): Promise<ActionResult> {
  if (fields.title !== undefined) fields = { ...fields, title: tabsToSpaces(fields.title) }
  if (typeof fields.project === 'string') fields = { ...fields, project: tabsToSpaces(fields.project) }
  const error = (fields.title !== undefined ? titleError(fields.title) : null) ?? (fields.body !== undefined ? bodyError(fields.body) : null) ?? projectError(fields.project) ?? dueError(fields.due_ms) ?? repeatFieldError(fields.repeat)
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
  const failed = await enqueue(nodeId, id, { kind: 'update', fields: f })
  return failed ? { ok: false, error: failed } : { ok: true, id }
}

/** 删一项。返回是否入队了（失败时已经提示用户，不抛）。 */
export async function deleteTodo(nodeId: string, id: string): Promise<boolean> {
  const failed = await enqueue(nodeId, id, { kind: 'delete' }, { discardFailed: true })
  if (failed) toast.error(i18n.t('todos.errors.storage.failed'))
  return !failed
}

/** 被拒的改动：重试（重新排队，失败计数清零）。 */
export async function retryOp(rec: OutboxRecord): Promise<void> {
  if (rec.seq === undefined) return
  try {
    await db.patchOutbox(rec.seq, (stored) => {
      const { error: _e, claimed_at: _c, claimed_by: _b, attempts: _a, drops: _d, ...rest } = stored
      return { ...rest, state: 'pending' }
    })
    await reloadOutbox()
    announce(rec.node_id)
  } catch (e) {
    storageFailed('retry a to-do change')(e)
    return
  }
  kickReplay(rec.node_id)
}

/** 被拒的改动：丢弃。这一项里排在它后面的待发改动依赖它，一起丢，并告诉用户丢了几条。 */
export async function discardOp(rec: OutboxRecord): Promise<void> {
  if (rec.seq === undefined) return
  try {
    const n = await db.discardOutboxFrom(rec.node_id, rec.id, rec.seq)
    await reloadOutbox()
    announce(rec.node_id)
    if (n > 1) toast.info(i18n.t('todos.discardedMore', { count: n - 1 }))
  } catch (e) {
    storageFailed('discard a to-do change')(e)
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// 重放
// ---------------------------------------------------------------------------------------------------------------------
export type ReplayResult = 'done' | 'offline' | 'blocked'

/** 链路错误连续失败这么多次，这一条标成失败（用户重试或丢弃），不再一直占着这一项。 */
export const MAX_LINK_FAILURES = 3
/** 同一条记录在飞的时候，节点那一侧出了状态变化（对端又上线 / 加密会话重建，见 `isNodeSideDrop`）这么多次，就标成失败（毒记录）。hub 连接断了、对端下线不算。 */
export const MAX_DROPS = 5
/** 别的实例的认领超过「一次请求的超时 + 这个余量」就算死了，可以接手（没有 Web Locks 时）。 */
export const LEASE_MARGIN_MS = 5_000
/** 这个实例（模块）的 id，写在它认领的记录上。 */
const OWNER = newTodoId()

const hasLocks = (): boolean => typeof navigator !== 'undefined' && !!navigator.locks?.request

/** 同一个节点的重放排队执行：Web Locks（跨标签页，排队等）；没有就本实例内串行，跨标签页靠租约。 */
async function withNodeLock<T>(nodeId: string, fn: () => Promise<T>): Promise<T> {
  if (hasLocks()) return navigator.locks.request(`anycon-todo-replay:${nodeId}`, () => fn())
  return serial(`replay:${nodeId}`, fn)
}

/** 请求在途时节点那一侧出了状态变化（对端又上线了 / 加密会话重建）：算「毒记录」的一次。hub 连接断了、对端下线（手机没信号、笔记本合盖）不算。 */
const isNodeSideDrop = (e: unknown): boolean => e instanceof RpcCallError && e.kind === 'closed' && (e.closeReason === 'peer_reconnected' || e.closeReason === 'noise_reset')

/** 一条重放得到的错误该怎么办。 */
export type ReplayVerdict =
  | { kind: 'drop' }
  | { kind: 'drop_full' }
  | { kind: 'failed'; error: OutboxError }
  | { kind: 'link' }
  | { kind: 'offline' }

/**
 * `ready`：出错的时候节点是不是还连着。对端或 hub 掉线引起的错误（`offline`、`closed`、`undeliverable`，以及节点已经不在线时的超时）
 * 算「离线」：留着这一条、不记失败次数，等重连。只有节点连着时的超时才记一次，和节点真正的错误答复。
 */
export function classifyReplayError(op: OutboxOp, e: unknown, ready = true): ReplayVerdict {
  if (!(e instanceof RpcCallError)) return { kind: 'failed', error: { code: 'unknown' } }
  if (e.kind === 'offline' || e.kind === 'closed' || e.kind === 'undeliverable') return { kind: 'offline' }
  if (e.kind === 'too_large') return { kind: 'failed', error: { code: 'too_large' } }
  if (e.kind !== 'rpc') return ready ? { kind: 'link' } : { kind: 'offline' }
  const code = e.code
  if (code === 'not_found' && op.kind !== 'add') return { kind: 'drop' }
  if (code === 'invalid' && op.kind === 'update' && e.reason === 'too_many' && op.fields.state !== undefined && !isClosed(op.fields.state)) return { kind: 'drop_full' }
  return { kind: 'failed', error: { code: code ?? 'unknown', ...(e.field ? { field: e.field } : {}), ...(e.reason ? { reason: e.reason } : {}) } }
}

// 节点连着、请求却超时 / 因对端波动失败时，过一会儿自己再试（退避），不用等别的事来触发
const retryTimers = new Map<string, ReturnType<typeof setTimeout>>()
const retryCounts = new Map<string, number>()
export const RETRY_BASE_MS = 2_000
export const RETRY_MAX_MS = 30_000
function cancelRetry(nodeId: string): void {
  const t = retryTimers.get(nodeId)
  if (t) clearTimeout(t)
  retryTimers.delete(nodeId)
}
/** 退避是按记录算的：换了一条（上一条成功 / 失败了）就从头开始。 */
const retryFor = new Map<string, number>()
function scheduleRetry(nodeId: string, seq: number): void {
  cancelRetry(nodeId)
  if (retryFor.get(nodeId) !== seq) retryCounts.delete(nodeId)
  retryFor.set(nodeId, seq)
  const n = retryCounts.get(nodeId) ?? 0
  retryCounts.set(nodeId, n + 1)
  retryTimers.set(
    nodeId,
    setTimeout(() => {
      retryTimers.delete(nodeId)
      // 这时节点又不在线了：不再排，重连之后的同步会重放
      if (rpc().isReady(nodeId)) kickReplay(nodeId)
    }, Math.min(RETRY_MAX_MS, RETRY_BASE_MS * 2 ** n)),
  )
}

async function sendOp(nodeId: string, rec: OutboxRecord): Promise<Todo | null> {
  const op = rec.op
  if (op.kind === 'add') return rpc().request<Todo>(nodeId, 'todo.add', { ...(op.params.handler ? { handler: op.params.handler } : {}), id: rec.id, title: op.params.title, body: op.params.body, ...(op.params.project ? { project: op.params.project } : {}), ...(op.params.due_ms != null ? { due_ms: op.params.due_ms } : {}), ...(op.params.repeat ? { repeat: op.params.repeat } : {}) })
  if (op.kind === 'update') return rpc().request<Todo>(nodeId, 'todo.update', { id: rec.id, ...op.fields })
  await rpc().request(nodeId, 'todo.delete', { id: rec.id })
  return null
}

/** 只改自己认领的记录：认领已经被别的实例接手（租约过期）时，不去动别人正在发的那条。 */
const mine = (f: (stored: OutboxRecord) => OutboxRecord | null) => (stored: OutboxRecord): OutboxRecord | null => (stored.claimed_by !== undefined && stored.claimed_by !== OWNER ? stored : f(stored))

/** 标成失败（只动自己认领的）；这一项后面已经排了删除时，失败的记录和夹在中间的一起丢掉，删除才发得出去。 */
async function markFailed(seq: number, f: (stored: OutboxRecord) => OutboxRecord): Promise<number> {
  return db.failOutbox(seq, (stored) => (stored.claimed_by !== undefined && stored.claimed_by !== OWNER ? stored : f(stored)))
}

const release = (r: OutboxRecord): OutboxRecord => {
  const { claimed_at: _c, claimed_by: _b, ...rest } = r
  return rest
}

function dropFromView(nodeId: string, id: string): void {
  useTodos.getState().applyChanged(nodeId, { id, deleted: true })
  noteTouched(nodeId, id, null)
}

/** 按入队顺序重放这个节点上的待发改动。链路没就绪时什么都不发。 */
export async function replayOutbox(nodeId: string): Promise<ReplayResult> {
  return (await runReplay(nodeId)).result
}

async function runReplay(nodeId: string): Promise<{ result: ReplayResult; relist: boolean }> {
  if (!nodeSupportsTodo(nodeId)) return { result: 'blocked', relist: false }
  cancelRetry(nodeId)
  return withNodeLock(nodeId, async () => {
    let relist = false
    try {
      for (;;) {
        if (!rpc().isReady(nodeId)) return { result: 'offline' as const, relist }
        // 持有 Web Lock 的实例独占，库里留的认领都是死掉的实例的；没有锁时只接手过期的认领
        const rec = await db.claimNextOutbox(nodeId, OWNER, Date.now(), hasLocks() ? 0 : RPC_TIMEOUT_MS + LEASE_MARGIN_MS)
        if (!rec) return { result: 'done' as const, relist }
        const seq = rec.seq!
        let sent: Todo | null = null
        let error: unknown
        let ok = false
        try {
          sent = await sendOp(nodeId, rec)
          ok = true
        } catch (e) {
          error = e
        }
        if (ok) {
          if (sent) {
            useTodos.getState().mergeItems(nodeId, [sent])
            noteTouched(nodeId, sent.id, sent)
            await db.applyTodoCache(nodeId, { put: [sent] }).catch(storageFailed('cache a to-do'))
          } else {
            dropFromView(nodeId, rec.id)
            await db.applyTodoCache(nodeId, { remove: [rec.id] }).catch(storageFailed('cache a to-do deletion'))
          }
          await db.deleteOutbox([seq])
          if (rec.queued_offline) relist = true
          retryCounts.delete(nodeId)
        } else {
          const v = classifyReplayError(rec.op, error, rpc().isReady(nodeId))
          if (v.kind === 'drop') {
            await db.deleteOutbox([seq])
            // not_found：这一项已经不在 daemon 上了（别处删了）：缓存里也去掉，不等下一次整份列表
            dropFromView(nodeId, rec.id)
            await db.applyTodoCache(nodeId, { remove: [rec.id] }).catch(storageFailed('cache a to-do deletion'))
          } else if (v.kind === 'drop_full') {
            // 重新打开被拒：项留在关闭状态；这条改动里别的字段照常发，只去掉 `state`
            const { state: _s, ...rest } = (rec.op as Extract<OutboxOp, { kind: 'update' }>).fields
            if (Object.keys(rest).length > 0) {
              await db.patchOutbox(seq, mine((stored) => ({ ...release(stored), op: { kind: 'update', fields: rest } })))
              toast.error(i18n.t('todos.replay.fullKept'))
            } else {
              await db.deleteOutbox([seq])
              toast.error(i18n.t('todos.replay.full'))
            }
          } else if (v.kind === 'failed') {
            const dropped = await markFailed(seq, (stored) => ({ ...release(stored), state: 'failed', error: v.error }))
            await reloadOutbox().catch(() => undefined)
            if (dropped === 0) announceRejected(nodeId, rec, v.error)
          } else if (v.kind === 'link' && (rec.attempts ?? 0) + 1 >= MAX_LINK_FAILURES) {
            const dropped = await markFailed(seq, (stored) => ({ ...release(stored), state: 'failed', error: { code: 'link' }, attempts: (stored.attempts ?? 0) + 1 }))
            await reloadOutbox().catch(() => undefined)
            if (dropped === 0) announceRejected(nodeId, rec, { code: 'link' })
          } else if (v.kind === 'offline' && isNodeSideDrop(error) && (rec.drops ?? 0) + 1 >= MAX_DROPS) {
            // 毒记录：发这一条的时候连接一次次被掐断（比如 daemon 一收到它就崩）：标成失败，别无声地一直循环
            const dropped = await markFailed(seq, (stored) => ({ ...release(stored), state: 'failed', error: { code: 'link' }, drops: (stored.drops ?? 0) + 1 }))
            await reloadOutbox().catch(() => undefined)
            if (dropped === 0) announceRejected(nodeId, rec, { code: 'link' })
          } else {
            // 纯链路 / 离线错误：这一条留着（节点连着时的超时记一次失败），这一轮结束；节点还连着就过一会儿自己再试
            await db.patchOutbox(seq, mine((stored) => ({ ...release(stored), ...(v.kind === 'link' ? { attempts: (stored.attempts ?? 0) + 1 } : {}), ...(isNodeSideDrop(error) ? { drops: (stored.drops ?? 0) + 1 } : {}) })))
            await reloadOutbox().catch(() => undefined)
            announce(nodeId)
            if (rpc().isReady(nodeId)) scheduleRetry(nodeId, seq)
            return { result: 'offline' as const, relist }
          }
        }
        await reloadOutbox().catch(() => undefined)
        announce(nodeId)
      }
    } catch (e) {
      // 本地库不可用（被别的标签页删掉 / 升级了）：这一轮停下，不抛给后台调用方
      console.warn('anycon: the to-do replay stopped because the local database failed', e)
      return { result: 'blocked' as const, relist }
    }
  })
}

/**
 * 排队的改动被节点拒绝了（重连之后才发现的那种）：不只是行上的「未保存」标记，弹一条错误提示（`role="alert"`）说是哪一项、为什么，
 * 带「查看」直达那一项的详情，「重试」和「丢弃」就在那里。
 */
function announceRejected(nodeId: string, rec: OutboxRecord, error: OutboxError): void {
  const view = viewOf(useTodos.getState(), [nodeId]).find((v) => v.id === rec.id)
  const title = view?.title ?? (rec.op.kind === 'add' ? rec.op.params.title : rec.id.slice(0, 8))
  const order = useNodes.getState().order
  useUi.getState().push(
    'error',
    i18n.t('todos.rejectedToast', { title: cleanOneLine(title, 60), reason: outboxErrorText(i18n.t.bind(i18n), error) }),
    { label: i18n.t('todos.review'), run: () => navigateTo(`/todos/${shortNodeId(nodeId, order)}/${rec.id}`) },
    12_000,
  )
}

/**
 * 节点已经确认了的一项（直接发给节点的请求，不经待发队列，比如「移到另一个节点」）：并进缓存和界面。
 * `sentAtTick`：请求发出时的 `todoTickNow()`。之后收到过这一项的 `todo.changed`、而回执并不比缓存里的更新（`updated_ms` 不大于）时，
 * 回执是旧快照，什么都不改（`activity` 的变化不动 `updated_ms`，所以光靠 `mergeItems` 的比较挡不住）。
 */
export async function applyConfirmedTodo(nodeId: string, todo: Todo, sentAtTick?: number): Promise<void> {
  if (sentAtTick !== undefined && todoChangeTick(nodeId, todo.id) > sentAtTick) {
    const cached = useTodos.getState().nodes[nodeId]?.items[todo.id]
    if (cached && cached.updated_ms >= todo.updated_ms) return
  }
  useTodos.getState().mergeItems(nodeId, [todo])
  noteTouched(nodeId, todo.id, todo)
  await db.applyTodoCache(nodeId, { put: [todo] }).catch(storageFailed('cache a to-do'))
  announce(nodeId)
}

/** 节点已经确认删掉了的一项（同上）：从缓存和界面去掉。 */
export async function applyConfirmedDelete(nodeId: string, id: string): Promise<void> {
  dropFromView(nodeId, id)
  await db.applyTodoCache(nodeId, { remove: [id] }).catch(storageFailed('cache a to-do deletion'))
  announce(nodeId)
}

/** 节点现在能发请求（界面上「未同步」的标记据此）。 */
export function nodeReady(nodeId: string): boolean {
  return useNodes.getState().byId[nodeId]?.conn === 'ready'
}

export { newTodoId }
