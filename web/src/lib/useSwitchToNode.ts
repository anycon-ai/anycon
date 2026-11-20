import { useNodeFilter } from './nodeFilter'
import { spaceLabelOf } from './spaces'

/**
 * 从通知 / 链接打开、但不在当前选择里的内容（spec/web.md § 4.13）：节点有分组就说它属于哪个空间、给「切换到这个空间」；
 * 没有分组就是原来的「显示这个节点」（把它勾上）。都不自动切换，由用户点。
 */
export function useSwitchToNode(nodeId: string): { spaceLabel: string; switchTo: () => void } {
  const { spaces, selectSpace, toggle } = useNodeFilter()
  const hit = spaces.spaces.find((s) => s.nodeIds.includes(nodeId))
  return {
    spaceLabel: hit ? spaceLabelOf(spaces, nodeId) : '',
    switchTo: () => (hit ? selectSpace(hit.key) : toggle(nodeId, true)),
  }
}
