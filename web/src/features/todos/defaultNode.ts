// 记一项缺省记到哪个节点（spec/web.md § 4.13、§ 4.12）：选了一个空间时是这个空间里上次用过的节点（每个空间各记一个），
// 选「全部」或手点节点时是上次记到的那个；上次的不在可选的节点里（没选中、不支持清单）就是第一个。
import { currentSpace } from '@/lib/nodeFilter'
import { groupOf } from '@/lib/spaces'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

/** `supporting`：选中的、支持清单的节点（按顺序）；`space`：选中的空间（key）或 null。 */
export function pickDefaultNode(supporting: readonly string[], space: string | null, last: string | null, lastBySpace: Readonly<Record<string, string>>): string | null {
  const remembered = space !== null ? lastBySpace[space] : undefined
  if (remembered && supporting.includes(remembered)) return remembered
  if (last && supporting.includes(last)) return last
  return supporting[0] ?? null
}

/** 记住这次记到了哪个节点：全局的「上次」，和这个节点所在空间（有分组的话）的「上次」。 */
export function rememberTodoNode(nodeId: string): void {
  const space = groupOf(useNodes.getState().byId[nodeId]?.info)
  const { set, lastTodoNodeBySpace } = usePrefs.getState()
  set({ lastTodoNode: nodeId, ...(space ? { lastTodoNodeBySpace: { ...lastTodoNodeBySpace, [space]: nodeId } } : {}) })
}

export { currentSpace }
