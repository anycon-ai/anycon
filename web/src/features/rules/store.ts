// 规则的内存状态（每个节点一份，docs/spec/daemon.md § 23）。规则的文字是用户内容，也可能是 agent 写的待确认的话：
// 只放在内存里，不落盘（没有离线缓存、没有待发队列）；写操作直接发给节点，节点连不上时界面禁用并说明原因。
import { useMemo } from 'react'
import { create } from 'zustand'
import type { Rule, RuleBudget, RuleChanged, RuleSourceKind, RuleStatus } from '@/protocol'
import { useNodes } from '@/stores/nodes'

export interface NodeRules {
  items: Record<string, Rule>
  budget: RuleBudget | null
  /** 每条规则最近一次被通知 / 写的回执改动的序号（`tick`）和是不是删除：整表的结果比它们早发出，就不能盖掉它们（见 `replaceAll`）。 */
  touched: Record<string, { tick: number; deleted: boolean }>
  /** 这个节点至少读到过一次（没读到之前不要说「没有规则」）。 */
  loaded: boolean
  /** 最近一次读失败了。 */
  failed: boolean
}

const emptyNode = (): NodeRules => ({ items: {}, budget: null, touched: {}, loaded: false, failed: false })

interface RulesState {
  nodes: Record<string, NodeRules>
  /** 单调递增：每次改动加一。读整表之前取一次（`since`），回来时只有在那之后没被改过的规则才以整表为准。 */
  tick: number
  patchNode: (nodeId: string, f: (n: NodeRules) => NodeRules) => void
  /** 整份 `rule.list` 的结果：以它为准。 */
  replaceAll: (nodeId: string, rules: readonly Rule[], budget: RuleBudget, since?: number) => void
  applyChanged: (nodeId: string, ch: RuleChanged) => void
  removeNode: (nodeId: string) => void
  reset: () => void
}

export const useRules = create<RulesState>()((set) => ({
  nodes: {},
  tick: 0,
  patchNode: (nodeId, f) => set((s) => ({ nodes: { ...s.nodes, [nodeId]: f(s.nodes[nodeId] ?? emptyNode()) } })),
  // 整表是请求发出那一刻的样子；回来之前收到的 `rule.changed` 或写的回执（`since` 之后改过的规则）更新，不能被它盖掉，也不能让删掉的规则复活
  replaceAll: (nodeId, rules, budget, since) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const newer = (id: string): { tick: number; deleted: boolean } | undefined => {
        const t = cur.touched[id]
        return since !== undefined && t && t.tick > since ? t : undefined
      }
      const items: Record<string, Rule> = {}
      for (const r of rules) {
        const t = newer(r.id)
        if (t?.deleted) continue
        const local = cur.items[r.id]
        // 请求发出之后改过的：本地的不比整表旧就留本地的；没改过的：本地的比整表新（不该发生）也留本地的
        items[r.id] = local && (t ? local.updated_ms >= r.updated_ms : local.updated_ms > r.updated_ms) ? local : r
      }
      // 整表里没有、但请求发出之后才出现的（新建的）规则：保留
      for (const [id, local] of Object.entries(cur.items)) if (!(id in items) && newer(id) && !newer(id)!.deleted) items[id] = local
      return { nodes: { ...s.nodes, [nodeId]: { ...cur, items, budget, loaded: true, failed: false } } }
    }),
  applyChanged: (nodeId, ch) =>
    set((s) => {
      const cur = s.nodes[nodeId] ?? emptyNode()
      const items = { ...cur.items }
      const touched = { ...cur.touched, [ch.id]: { tick: s.tick + 1, deleted: !!ch.deleted } }
      if (ch.deleted) delete items[ch.id]
      else if (ch.rule && ch.rule.id === ch.id) {
        // `updated_ms` 严格递增：旧的通知不能盖掉新的（`last_injected_ms` 不动 `updated_ms`，所以同一个值也接受）
        const old = items[ch.id]
        if (old && old.updated_ms > ch.rule.updated_ms) return s
        items[ch.id] = ch.rule
      } else return s
      return { tick: s.tick + 1, nodes: { ...s.nodes, [nodeId]: { ...cur, items, touched } } }
    }),
  removeNode: (nodeId) =>
    set((s) => {
      const { [nodeId]: _gone, ...nodes } = s.nodes
      return { nodes }
    }),
  reset: () => set({ nodes: {}, tick: 0 }),
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
