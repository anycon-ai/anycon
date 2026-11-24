// 分身的内存快照：重连补读，登出和忘记节点时清掉，不落盘。
import { create } from 'zustand'
import type { TwinDayStats, TwinGetResult, TwinQuestion, TwinRun } from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { useNodeFilter } from '@/lib/nodeFilter'

export interface NodeTwin {
  value?: TwinGetResult
  runs: TwinRun[]
  questions: TwinQuestion[]
  failed: boolean
  /** 趋势区间（天）。每次重读都按它取 `twin.stats`，`twin.changed` 之后跟着刷新。 */
  statsDays: number
  /** 最近一次 `twin.stats` 的结果：升序，最后一项是今天。节点不支持或还没读到时缺省。 */
  stats?: TwinDayStats[]
  /** 最近一次 `twin.stats` 读取失败；下一次成功时清掉。只影响趋势区，不算整个分身读取失败。 */
  statsFailed: boolean
}
export const emptyTwin = (): NodeTwin => ({ runs: [], questions: [], failed: false, statsDays: 7, statsFailed: false })
export const useTwin = create<{ nodes: Record<string, NodeTwin>; patch: (id: string, patch: Partial<NodeTwin>) => void; remove: (id: string) => void; reset: () => void }>()(
  (set) => ({
    nodes: {},
    patch: (id, patch) => set((s) => ({ nodes: { ...s.nodes, [id]: { ...(s.nodes[id] ?? emptyTwin()), ...patch } } })),
    remove: (id) => set((s) => ({ nodes: Object.fromEntries(Object.entries(s.nodes).filter(([key]) => key !== id)) })),
    reset: () => set({ nodes: {} }),
  }),
)
/** 分身连接列表要重读的信号：每收到一条 `twin.changed` 加一，`TwinLinks` 据此（合并之后）重新读 `twin.links`。 */
export const useTwinLinksTick = create<{ tick: Record<string, number> }>()(() => ({ tick: {} }))
export const bumpTwinLinks = (id: string): void =>
  useTwinLinksTick.setState((s) => ({ tick: { ...s.tick, [id]: (s.tick[id] ?? 0) + 1 } }))
export const supportsTwinStats = (id: string): boolean => useNodes.getState().byId[id]?.info?.capabilities.twin_stats === true
export const supportsTwin = (id: string): boolean => useNodes.getState().byId[id]?.info?.capabilities.twin === true
export function useTwinAvailable(): boolean {
  return useNodes((s) => s.order.some((id) => s.byId[id]?.info?.capabilities.twin === true))
}
export function useTwinNodeIds(): string[] {
  const { selected } = useNodeFilter()
  const nodes = useNodes((s) => s.byId)
  return selected.filter((id) => nodes[id]?.info?.capabilities.twin === true)
}
export function twinQuestions(nodes: Record<string, NodeTwin>, ids: readonly string[]): Array<TwinQuestion & { node_id: string }> {
  return ids.flatMap((id) => (nodes[id]?.questions ?? []).map((q) => ({ ...q, node_id: id })))
}

// 连续无事折成一组，仍保留每次运行，用户可以展开查看。
export function groupRuns(runs: readonly TwinRun[]): TwinRun[][] {
  const groups: TwinRun[][] = []
  for (const run of [...runs].sort((a, b) => b.started_ms - a.started_ms || b.id.localeCompare(a.id))) {
    const last = groups.at(-1)
    if (run.result === 'idle' && !run.dry_run && last?.[0]?.result === 'idle' && !last[0].dry_run) last.push(run)
    else groups.push([run])
  }
  return groups
}
