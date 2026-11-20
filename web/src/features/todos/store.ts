// 清单的内存状态（每个节点一份缓存 + 全部节点的待发改动）。落盘和网络在 `service.ts`；这里只有同步的状态变更和读取。
// 界面读的是 `useTodoView`：缓存叠上待发改动（`overlayOutbox`），离线期间做的改动立刻就在里面。
import { useMemo } from 'react'
import { create } from 'zustand'
import type { Todo, TodoChanged, TodoCursor } from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { overlayOutbox } from './outbox'
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
  /** 同步进行中：期间 `todo.changed` 碰过的项不能被「整份替换」删掉。 */
  syncing: boolean
  touched: Record<string, true>
  /** 最近一次同步（本机时钟）。 */
  syncedAt?: number
  closed: ClosedState
}

const emptyClosed = (): ClosedState => ({ expanded: false, loading: false, complete: false, hasMore: false, failed: false })
const emptyNode = (): NodeTodos => ({ items: {}, hydrated: false, synced: false, syncing: false, touched: {}, closed: emptyClosed() })

interface TodosState {
  nodes: Record<string, NodeTodos>
  outbox: OutboxRecord[]
  outboxLoaded: boolean
  patchNode: (nodeId: string, f: (n: NodeTodos) => NodeTodos) => void
  /** 读完缓存。 */
  setCache: (nodeId: string, todos: readonly Todo[], syncedAt?: number) => void
  /** 全量翻完之后的结果：没关闭的项以它为准（同步期间被 `todo.changed` 碰过的除外）；缓存里已关闭的保留。 */
  replaceOpen: (nodeId: string, fetched: readonly Todo[], syncedAt: number) => void
  /** 并入一批项（翻页、已关闭的翻页），按 id 去重，`updated_ms` 新的赢。 */
  mergeItems: (nodeId: string, todos: readonly Todo[]) => void
  applyChanged: (nodeId: string, ch: TodoChanged) => void
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
      for (const t of Object.values(cur.items)) if (isClosed(t.state) || cur.touched[t.id]) items[t.id] = t
      for (const t of fetched) if (!items[t.id] || items[t.id]!.updated_ms <= t.updated_ms) items[t.id] = t
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items, hydrated: true, synced: true, syncing: false, touched: {}, syncedAt } } }
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
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items, touched: cur.syncing ? { ...cur.touched, [ch.id]: true } : cur.touched } } }
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

export function useSupportingNodes(nodeIds: readonly string[]): string[] {
  const byId = useNodes((s) => s.byId)
  return useMemo(() => nodeIds.filter((id) => byId[id]?.info?.capabilities.todo === true), [nodeIds, byId])
}

/** 这些节点上的清单（缓存叠上待发改动），不排序、不分组。 */
export function viewOf(state: Pick<TodosState, 'nodes' | 'outbox'>, nodeIds: readonly string[]): ViewTodo[] {
  const out: ViewTodo[] = []
  for (const id of nodeIds) {
    const n = state.nodes[id]
    const base = new Map(Object.entries(n?.items ?? {}))
    const ops = state.outbox.filter((r) => r.node_id === id)
    out.push(...overlayOutbox(id, base, ops).values())
  }
  return out
}

export function useTodoView(nodeIds: readonly string[]): ViewTodo[] {
  const nodes = useTodos((s) => s.nodes)
  const outbox = useTodos((s) => s.outbox)
  return useMemo(() => viewOf({ nodes, outbox }, nodeIds), [nodes, outbox, nodeIds])
}
