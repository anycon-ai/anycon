// 子 agent 的调用归到启动它的那次调用下面（`tool_call.parent_id`，docs/protocol.md § 6）。
// 只认排在父调用之后的子调用：父调用不在已加载的块里（缓存窗口起点之后才看到）的照常平铺，指向自己或成环的也不会被藏起来。
import type { SessionStatus } from '@/protocol'
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
 * 一次工具调用的状态（子 agent 块、工具行共用）。`stopped`：没有结果、或任务停在 `running`，但已经不可能还有下文。
 *
 * 调用本身早就返回了、真正的工作还在后台跑的，有两种表现：
 * - 挂着任务（`ToolBlock.tasks`，`task.tool_id` 指向这次调用）：以任务为准——有任务在 `running` 就是 `background`，
 *   `live` 为 false（会话没在跑、daemon 也没报后台任务，见 `tasksLive`）时是 `stopped`；任务都结束了，有失败的就是 `error`，否则 `done`。
 *   Claude Code 的 `Workflow` 和 Codex 的 `spawnAgent` 的调用结果都是立刻返回、不带 `background`，只能靠任务判断。
 * - 没有任务、结果带 `detail.background`（后台命令、旧 daemon 的后台子 agent）：要等结束时补的那条结果才算完。
 */
export type SubagentActivity = 'running' | 'background' | 'error' | 'done' | 'stopped'

export function subagentState(b: ToolBlock, live = true): SubagentActivity {
  if (!b.result) return 'running'
  if (b.result.isError) return 'error'
  if (b.tasks?.length) {
    if (b.tasks.some((t) => t.status === 'running')) return live ? 'background' : 'stopped'
    return b.tasks.some((t) => t.status === 'failed') ? 'error' : 'done'
  }
  return b.result.detail?.background ? 'background' : 'done'
}

export interface SubagentRow {
  block: ToolBlock
  state: SubagentActivity
  /** 嵌套层数：主 agent 直接启动的是 0。 */
  depth: number
  /** 它一共调了多少次工具（含嵌套的）。 */
  calls: number
  /** agent 报的它用掉的 token（`tool_result.detail.total_tokens`），结束前和 agent 不报时没有。 */
  tokens?: number
}

/** 主 agent 正在跑一个 turn（含等审批）：前台子 agent 只能活在 turn 里。 */
const TURN_ACTIVE: readonly SessionStatus[] = ['running', 'starting', 'waiting_permission']
export const isTurnActive = (s: SessionStatus): boolean => TURN_ACTIVE.includes(s)
/** agent 进程已经不在了：后台子 agent 也跟着没了。 */
const PROCESS_GONE: readonly SessionStatus[] = ['ended', 'error', 'interrupted']

export const isActive = (s: SubagentActivity): boolean => s === 'running' || s === 'background'

/**
 * 已加载的块里所有子 agent，父在前、子紧跟其后（按启动顺序）。
 * 没有结果的子 agent 只有在主 agent 的 turn 还在跑（或者启动它的子 agent 还在跑）时才算运行中，
 * 否则是 `stopped`（turn 被取消、进程退出时不会再有结果）；后台子 agent 在进程退出后同样算 `stopped`。
 * `tasksMayProgress`：会话里的后台任务还可能有进展吗（`tasksLive`），挂着任务的子 agent 据此判断任务是不是停了。
 */
export function subagentRows(blocks: readonly Block[], status: SessionStatus, tasksMayProgress: boolean = isTurnActive(status)): SubagentRow[] {
  const tree = toolTree(blocks)
  const gone = PROCESS_GONE.includes(status)
  const rows: SubagentRow[] = []
  const visit = (b: ToolBlock, depth: number, parentLive: boolean): void => {
    let live = parentLive
    const isAgent = b.category === 'subagent'
    if (isAgent) {
      const base = subagentState(b, tasksMayProgress)
      const state: SubagentActivity = (base === 'running' && !parentLive) || (base === 'background' && gone) ? 'stopped' : base
      rows.push({ block: b, state, depth, calls: countCalls(b.toolId, tree), tokens: b.result?.detail?.total_tokens ?? undefined })
      live = isActive(state)
    }
    for (const c of tree.children.get(b.toolId) ?? []) visit(c, isAgent ? depth + 1 : depth, live)
  }
  for (const b of blocks) if (b.type === 'tool' && !tree.nested.has(b.key)) visit(b, 0, isTurnActive(status))
  return rows
}

/**
 * 还在后台跑的调用（Claude Code 的后台 Bash、`Workflow` 等；子 agent 在 `subagentRows` 里）：`subagentState` 判为 `background` 的非子 agent 调用，
 * 即挂着在跑的任务，或者结果带 `detail.background`。结束时同一个调用会补一条不带 `background` 的结果、任务也会进终态；
 * agent 进程不在了的，后台命令也跟着没了，不再算。
 */
export function backgroundCommands(blocks: readonly Block[], status: SessionStatus, live = true): ToolBlock[] {
  if (PROCESS_GONE.includes(status)) return []
  return blocks.filter((b): b is ToolBlock => b.type === 'tool' && b.category !== 'subagent' && subagentState(b, live) === 'background')
}
