// 子 agent 的调用归到启动它的那次调用下面（`tool_call.parent_id`，docs/protocol.md § 6）。
// 只认排在父调用之后的子调用：父调用不在已加载的块里（缓存窗口起点之后才看到）的照常平铺，指向自己或成环的也不会被藏起来。
import type { Block, ToolBlock } from './reducer'

export interface ToolTree {
  /** 父调用的 `toolId` → 它直接启动的调用（按时间顺序）。 */
  children: ReadonlyMap<string, ToolBlock[]>
  /** 收进父调用块里、不再单独成行的块的 `key`。 */
  nested: ReadonlySet<string>
}

export function toolTree(blocks: readonly Block[]): ToolTree {
  const seen = new Set<string>()
  const children = new Map<string, ToolBlock[]>()
  const nested = new Set<string>()
  for (const b of blocks) {
    if (b.type !== 'tool') continue
    if (b.parentId && seen.has(b.parentId)) {
      const list = children.get(b.parentId)
      if (list) list.push(b)
      else children.set(b.parentId, [b])
      nested.add(b.key)
    }
    seen.add(b.toolId)
  }
  return { children, nested }
}

/** 一个子 agent 一共调了多少次工具（含它再启动的子 agent 里面的）。 */
export function countCalls(toolId: string, tree: ToolTree): number {
  let n = 0
  for (const c of tree.children.get(toolId) ?? []) n += 1 + countCalls(c.toolId, tree)
  return n
}

/**
 * 子 agent 块的状态。后台启动的子 agent（`tool_result.detail.background`）调用本身早就返回了，
 * 要等结束时补的那条结果才算完。
 */
export type SubagentState = 'running' | 'background' | 'error' | 'done'

export function subagentState(b: ToolBlock): SubagentState {
  if (!b.result) return 'running'
  if (b.result.detail?.background) return 'background'
  return b.result.isError ? 'error' : 'done'
}
