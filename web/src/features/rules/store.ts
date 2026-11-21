// 规则的内存状态（每个节点一份，docs/spec/daemon.md § 23）。规则的文字是用户内容，也可能是 agent 写的待确认的话：
// 只放在内存里，不落盘（没有离线缓存、没有待发队列）；写操作直接发给节点，节点连不上时界面禁用并说明原因。
import { useMemo } from 'react'
import { create } from 'zustand'
import type { Rule, RuleBudget, RuleChanged, RuleSourceKind, RuleStatus } from '@/protocol'
import { useNodes } from '@/stores/nodes'

export interface NodeRules {
  items: Record<string, Rule>
  budget: RuleBudget | null
  /** 这个节点至少读到过一次（没读到之前不要说「没有规则」）。 */
  loaded: boolean
  /** 最近一次读失败了。 */
  failed: boolean
}

const emptyNode = (): NodeRules => ({ items: {}, budget: null, loaded: false, failed: false })

interface RulesState {
  nodes: Record<string, NodeRules>
  patchNode: (nodeId: string, f: (n: NodeRules) => NodeRules) => void
  /** 整份 `rule.list` 的结果：以它为准。 */
  replaceAll: (nodeId: string, rules: readonly Rule[], budget: RuleBudget) => void
  applyChanged: (nodeId: string, ch: RuleChanged) => void
  removeNode: (nodeId: string) => void
  reset: () => void
}

export const useRules = create<RulesState>()((set) => ({
  nodes: {},
  patchNode: (nodeId, f) => set((s) => ({ nodes: { ...s.nodes, [nodeId]: f(s.nodes[nodeId] ?? emptyNode()) } })),
  replaceAll: (nodeId, rules, budget) => set((s) => ({ nodes: { ...s.nodes, [nodeId]: { items: Object.fromEntries(rules.map((r) => [r.id, r])), budget, loaded: true, failed: false } } })),
  applyChanged: (nodeId, ch) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const items = { ...cur.items }
      if (ch.deleted) delete items[ch.id]
      else if (ch.rule && ch.rule.id === ch.id) {
        // `updated_ms` 严格递增：旧的通知不能盖掉新的（`last_injected_ms` 不动 `updated_ms`，所以同一个值也接受）
        const old = items[ch.id]
        if (old && old.updated_ms > ch.rule.updated_ms) return s
        items[ch.id] = ch.rule
      } else return s
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items } } }
    }),
  removeNode: (nodeId) =>
    set((s) => {
      const { [nodeId]: _gone, ...nodes } = s.nodes
      return { nodes }
    }),
  reset: () => set({ nodes: {} }),
}))

/** 节点支持决策档案（`node.info.capabilities.rules`）。旧 daemon 没有这个字段；还没拿到过 `node.info` 的节点按不支持算。 */
export function nodeSupportsRules(nodeId: string): boolean {
  return useNodes.getState().byId[nodeId]?.info?.capabilities.rules === true
}

/** 有没有任何已配对的节点支持规则：没有就不提供规则的入口（侧栏、「更多」、命令面板）。 */
export function useRulesAvailable(): boolean {
  return useNodes((s) => s.order.some((id) => s.byId[id]?.info?.capabilities.rules === true))
}

export function useRulesNodes(nodeIds: readonly string[]): string[] {
  const byId = useNodes((s) => s.byId)
  return useMemo(() => nodeIds.filter((id) => byId[id]?.info?.capabilities.rules === true), [nodeIds, byId])
}

/** 界面上用的一条：规则带着它所在的节点。 */
export interface ViewRule extends Rule {
  node_id: string
}

const views = new WeakMap<Rule, ViewRule>()
function viewOf(nodeId: string, r: Rule): ViewRule {
  let v = views.get(r)
  if (!v || v.node_id !== nodeId) {
    v = { ...r, node_id: nodeId }
    views.set(r, v)
  }
  return v
}

/** 这些节点上的规则，按创建时间从早到晚（再按 id，顺序稳定）。 */
export function rulesOf(nodes: Record<string, NodeRules>, nodeIds: readonly string[]): ViewRule[] {
  const out: ViewRule[] = []
  for (const nodeId of nodeIds) for (const r of Object.values(nodes[nodeId]?.items ?? {})) out.push(viewOf(nodeId, r))
  return out.sort((a, b) => a.created_ms - b.created_ms || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
}

export function useRuleView(nodeIds: readonly string[]): ViewRule[] {
  const nodes = useRules((s) => s.nodes)
  return useMemo(() => rulesOf(nodes, nodeIds), [nodes, nodeIds])
}

/** 一条规则的状态：不认识的值（更新的 daemon）按「不生效」处理，单独成一类，不当成任何已知状态。 */
export type KnownStatus = Exclude<RuleStatus, 'unknown'> | 'unknown'
export const statusOf = (r: Pick<Rule, 'status'>): KnownStatus => (r.status === 'pending' || r.status === 'active' || r.status === 'disabled' || r.status === 'rejected' ? r.status : 'unknown')
export const isActiveRule = (r: Pick<Rule, 'status'>): boolean => r.status === 'active'
export const isPendingRule = (r: Pick<Rule, 'status'>): boolean => r.status === 'pending'

/** 来源：不认识的种类按「不是人写的」处理。 */
export type KnownSource = Exclude<RuleSourceKind, 'unknown'> | 'other'
export const sourceOf = (r: Pick<Rule, 'source'>): KnownSource => (r.source.kind === 'manual' || r.source.kind === 'answer' || r.source.kind === 'extracted' || r.source.kind === 'agent' ? r.source.kind : 'other')
/** 人写下的（手写、答复里记成规则的）。 */
export const writtenByPerson = (r: Pick<Rule, 'source'>): boolean => r.source.kind === 'manual' || r.source.kind === 'answer'
