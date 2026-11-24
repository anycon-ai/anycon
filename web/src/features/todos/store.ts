// 清单的内存状态（每个节点一份缓存 + 全部节点的待发改动）。落盘和网络在 `service.ts`；这里只有同步的状态变更和读取。
// 界面读的是 `useTodoView`：缓存叠上待发改动（`overlayOutbox`），离线期间做的改动立刻就在里面。
import { useMemo } from 'react'
import { create } from 'zustand'
import type { Todo, TodoChanged, TodoCursor } from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { overlayItem } from './outbox'
import type { OutboxRecord, ViewTodo } from './types'
import { isClosed } from './types'

export interface ClosedState {
  /** 用户展开了「已关闭」。 */
  expanded: boolean
  loading: boolean
  /** 已经从头到尾翻完一遍。 */
  complete: boolean
  hasMore: boolean
  cursor?: TodoCursor
  failed: boolean
}

export interface NodeTodos {
  items: Record<string, Todo>
  /** 缓存读过了（没读完之前不要说「没有清单项」）。 */
  hydrated: boolean
  /** 这一次连接的同步做完了。 */
  synced: boolean
  /** 同步（翻页）进行中。期间被 `todo.changed` 或本地动作碰过的项由 `service.ts` 记下来，翻完时不被旧的一页盖掉。 */
  syncing: boolean
  /** 最近一次同步（本机时钟）。 */
  syncedAt?: number
  closed: ClosedState
}

const emptyClosed = (): ClosedState => ({ expanded: false, loading: false, complete: false, hasMore: false, failed: false })
const emptyNode = (): NodeTodos => ({ items: {}, hydrated: false, synced: false, syncing: false, closed: emptyClosed() })

interface TodosState {
  nodes: Record<string, NodeTodos>
  outbox: OutboxRecord[]
  outboxLoaded: boolean
  patchNode: (nodeId: string, f: (n: NodeTodos) => NodeTodos) => void
  /** 读完缓存。 */
  setCache: (nodeId: string, todos: readonly Todo[], syncedAt?: number) => void
  /** 全量翻完之后的结果（`service.ts` 已经把翻页期间变过的项并进来）：没关闭的项以它为准；缓存里已关闭的保留。 */
  replaceOpen: (nodeId: string, fetched: readonly Todo[], syncedAt: number) => void
  /** 并入一批项（翻页、已关闭的翻页），按 id 去重，`updated_ms` 新的赢。 */
  mergeItems: (nodeId: string, todos: readonly Todo[]) => void
  applyChanged: (nodeId: string, ch: TodoChanged) => void
  /** 从内存里去掉这些项（修剪已经不在节点上的缓存）。 */
  removeItems: (nodeId: string, ids: readonly string[]) => void
  removeNode: (nodeId: string) => void
  setOutbox: (records: OutboxRecord[]) => void
}

export const useTodos = create<TodosState>()((set) => ({
  nodes: {},
  outbox: [],
  outboxLoaded: false,
  patchNode: (nodeId, f) => set((s) => ({ nodes: { ...s.nodes, [nodeId]: f(s.nodes[nodeId] ?? emptyNode()) } })),
  setCache: (nodeId, todos, syncedAt) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      // 同步比缓存读得快的话（缓存还没读完 changed 就来了）不能被旧缓存盖掉：只补缺的
      const items = { ...cur.items }
      for (const t of todos) if (!items[t.id] || items[t.id]!.updated_ms < t.updated_ms) items[t.id] = t
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items, hydrated: true, ...(syncedAt !== undefined && cur.syncedAt === undefined ? { syncedAt } : {}) } } }
    }),
  replaceOpen: (nodeId, fetched, syncedAt) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const items: Record<string, Todo> = {}
      for (const t of Object.values(cur.items)) if (isClosed(t.state)) items[t.id] = t
      for (const t of fetched) if (!items[t.id] || items[t.id]!.updated_ms <= t.updated_ms) items[t.id] = t
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items, hydrated: true, synced: true, syncing: false, syncedAt } } }
    }),
  mergeItems: (nodeId, todos) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const items = { ...cur.items }
      for (const t of todos) if (!items[t.id] || items[t.id]!.updated_ms <= t.updated_ms) items[t.id] = t
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items } } }
    }),
  applyChanged: (nodeId, ch) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const items = { ...cur.items }
      if (ch.deleted) delete items[ch.id]
      else if (ch.todo && ch.todo.id === ch.id) items[ch.id] = ch.todo
      else return s
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items } } }
    }),
  removeItems: (nodeId, ids) =>
    set((s) => {
      const cur = s.nodes[nodeId]
      if (!cur) return s
      const items = { ...cur.items }
      for (const id of ids) delete items[id]
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items } } }
    }),
  removeNode: (nodeId) =>
    set((s) => {
      const { [nodeId]: _gone, ...nodes } = s.nodes
      return { nodes, outbox: s.outbox.filter((r) => r.node_id !== nodeId) }
    }),
  setOutbox: (outbox) => set({ outbox, outboxLoaded: true }),
}))

/** 节点支持清单（`node.info.capabilities.todo`）。旧 daemon 没有这个字段；还没拿到过 `node.info` 的节点按不支持算。 */
export function nodeSupportsTodo(nodeId: string): boolean {
  return useNodes.getState().byId[nodeId]?.info?.capabilities.todo === true
}

/** 有没有任何已配对的节点支持清单：没有就不提供清单的入口（左栏和它的「＋」、`C`、命令面板、会话页菜单）。 */
export function useTodoAvailable(): boolean {
  return useNodes((s) => s.order.some((id) => s.byId[id]?.info?.capabilities.todo === true))
}

export function useSupportingNodes(nodeIds: readonly string[]): string[] {
  const byId = useNodes((s) => s.byId)
  return useMemo(() => nodeIds.filter((id) => byId[id]?.info?.capabilities.todo === true), [nodeIds, byId])
}

/** 没有待发改动的项的界面形状：按缓存里那个对象记住，缓存没变的项不重新生成（一千项、每项 16 KiB 也不会每次改动都重建）。 */
const baseViews = new WeakMap<Todo, ViewTodo>()
function baseView(nodeId: string, t: Todo): ViewTodo {
  let v = baseViews.get(t)
  if (!v || v.node_id !== nodeId) {
    v = { ...t, node_id: nodeId, unsynced: false }
    baseViews.set(t, v)
  }
  return v
}

/** 这些节点上的清单（缓存叠上待发改动），不排序、不分组。只有带待发改动的项才重新叠，其余复用上一次的对象。 */
export function viewOf(state: Pick<TodosState, 'nodes' | 'outbox'>, nodeIds: readonly string[]): ViewTodo[] {
  const out: ViewTodo[] = []
  for (const nodeId of nodeIds) {
    const items = state.nodes[nodeId]?.items ?? {}
    const pending = new Map<string, OutboxRecord[]>()
    for (const r of state.outbox) {
      if (r.node_id !== nodeId) continue
      const list = pending.get(r.id)
      if (list) list.push(r)
      else pending.set(r.id, [r])
    }
    for (const t of Object.values(items)) {
      const ops = pending.get(t.id)
      if (!ops) {
        out.push(baseView(nodeId, t))
        continue
      }
      const v = overlayItem(nodeId, t.id, t, ops)
      if (v) out.push(v)
    }
    for (const [id, ops] of pending) {
      if (id in items) continue
      const v = overlayItem(nodeId, id, undefined, ops)
      if (v) out.push(v)
    }
  }
  return out
}

export function useTodoView(nodeIds: readonly string[]): ViewTodo[] {
  const nodes = useTodos((s) => s.nodes)
  const outbox = useTodos((s) => s.outbox)
  return useMemo(() => viewOf({ nodes, outbox }, nodeIds), [nodes, outbox, nodeIds])
}
