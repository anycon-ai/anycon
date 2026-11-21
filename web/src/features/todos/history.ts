// 一项的「经过」（`todo.history` / `todo.event`，docs/spec/daemon.md § 22.4）：每次派发、每一轮的结果、人改的状态。
// 只在内存里（`result` 的文字是 agent 写的，按会话内容对待，不落盘）；打开详情 / 验收页时才加载，之后靠 `todo.event` 追加。
// 按 `seq` 从新到旧存放，同一个 `seq` 只留一份（翻页和通知可能重叠）。
import { create } from 'zustand'
import type { TodoEvent, TodoEventNotice, TodoHistoryResult } from '@/protocol'
import { rpc } from '@/transport/instance'

export interface HistoryEntry {
  /** 按 `seq` 从新到旧。 */
  events: TodoEvent[]
  /** 更旧的还没翻到。 */
  hasMore: boolean
  loading: boolean
  /** 第一页读到过。 */
  loaded: boolean
  failed: boolean
}

const EMPTY: HistoryEntry = { events: [], hasMore: false, loading: false, loaded: false, failed: false }

export const historyKey = (nodeId: string, todoId: string): string => `${nodeId}:${todoId}`

/** 合并两批事件：按 `seq` 去重（后来的赢），从新到旧。 */
export function mergeEvents(a: readonly TodoEvent[], b: readonly TodoEvent[]): TodoEvent[] {
  const bySeq = new Map<number, TodoEvent>()
  for (const e of a) bySeq.set(e.seq, e)
  for (const e of b) bySeq.set(e.seq, e)
  return [...bySeq.values()].sort((x, y) => y.seq - x.seq)
}

interface HistoryState {
  byKey: Record<string, HistoryEntry>
  patch: (key: string, f: (e: HistoryEntry) => Partial<HistoryEntry>) => void
  /** 通知来的新事件：只追加到已经在看的（有记录的）项上；没在看的项之后打开时整页重新读。 */
  append: (key: string, event: TodoEvent) => void
  dropNode: (nodeId: string) => void
  reset: () => void
}

export const useTodoHistory = create<HistoryState>()((set) => ({
  byKey: {},
  patch: (key, f) =>
    set((s) => {
      const cur = s.byKey[key] ?? EMPTY
      return { byKey: { ...s.byKey, [key]: { ...cur, ...f(cur) } } }
    }),
  append: (key, event) =>
    set((s) => {
      const cur = s.byKey[key]
      if (!cur) return s
      return { byKey: { ...s.byKey, [key]: { ...cur, events: mergeEvents(cur.events, [event]) } } }
    }),
  dropNode: (nodeId) =>
    set((s) => {
      const prefix = `${nodeId}:`
      const byKey = Object.fromEntries(Object.entries(s.byKey).filter(([k]) => !k.startsWith(prefix)))
      return { byKey }
    }),
  reset: () => set({ byKey: {} }),
}))

export function useHistoryEntry(nodeId: string, todoId: string): HistoryEntry {
  return useTodoHistory((s) => s.byKey[historyKey(nodeId, todoId)]) ?? EMPTY
}

const PAGE = 50

/**
 * 读第一页（打开详情 / 验收页、重连之后）。读到的和已经追加的合并，不丢通知来的新事件。
 * 同一项同时只有一个请求；失败只记 `failed`，界面给「重试」。
 */
export async function loadHistory(nodeId: string, todoId: string): Promise<void> {
  const key = historyKey(nodeId, todoId)
  const st = useTodoHistory.getState()
  if (st.byKey[key]?.loading) return
  st.patch(key, () => ({ loading: true, failed: false }))
  try {
    const res = await rpc().request<TodoHistoryResult>(nodeId, 'todo.history', { id: todoId, limit: PAGE })
    useTodoHistory.getState().patch(key, (e) => ({ events: mergeEvents(e.events, res.events), hasMore: e.loaded ? e.hasMore && res.has_more : res.has_more, loading: false, loaded: true, failed: false }))
  } catch (e) {
    console.warn('anycon: could not load the history of a to-do', e)
    useTodoHistory.getState().patch(key, () => ({ loading: false, failed: true }))
  }
}

/** 往更早翻一页（用最后一条的 `seq` 作为 `before_seq`）。 */
export async function loadMoreHistory(nodeId: string, todoId: string): Promise<void> {
  const key = historyKey(nodeId, todoId)
  const cur = useTodoHistory.getState().byKey[key]
  if (!cur || cur.loading || !cur.hasMore || cur.events.length === 0) return
  const before = cur.events[cur.events.length - 1]!.seq
  useTodoHistory.getState().patch(key, () => ({ loading: true, failed: false }))
  try {
    const res = await rpc().request<TodoHistoryResult>(nodeId, 'todo.history', { id: todoId, before_seq: before, limit: PAGE })
    useTodoHistory.getState().patch(key, (e) => ({ events: mergeEvents(e.events, res.events), hasMore: res.has_more, loading: false }))
  } catch (e) {
    console.warn('anycon: could not load more history of a to-do', e)
    useTodoHistory.getState().patch(key, () => ({ loading: false, failed: true }))
  }
}

/** `todo.event`：形状不对的丢掉（来自节点，但不能让一条坏通知打断别的）。 */
export function handleTodoEvent(nodeId: string, raw: unknown): void {
  if (typeof raw !== 'object' || raw === null) return
  const n = raw as Partial<TodoEventNotice>
  const e = n.event
  if (typeof n.todo_id !== 'string' || n.todo_id === '' || typeof e !== 'object' || e === null) return
  if (typeof e.seq !== 'number' || !Number.isFinite(e.seq) || typeof e.ts_ms !== 'number' || typeof e.kind !== 'string') return
  useTodoHistory.getState().append(historyKey(nodeId, n.todo_id), e)
}

/** 最近一条 `result`（最新的那一轮的结果），给验收页和「待验收」行当结论。没有就是 `undefined`。 */
export function latestResult(events: readonly TodoEvent[]): Extract<TodoEvent, { kind: 'result' }> | undefined {
  return events.find((e): e is Extract<TodoEvent, { kind: 'result' }> => e.kind === 'result')
}
