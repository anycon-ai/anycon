// 后台任务（`task` 事件，docs/protocol.md § 6）的展示辅助：有效状态、哪些在跑、workflow 里 agent 的分组。纯函数。
import type { SessionStatus, TaskAgent, TaskAgentState, TaskPhase, TaskStatus } from '@/protocol'
import type { Block, TaskSnapshot } from './reducer'
import { isTurnActive } from './subagents'

/** 界面上的任务状态：协议的四个状态，加上 `stopped`——快照停在 `running`、但已经不可能还有后文。 */
export type TaskDisplayStatus = TaskStatus | 'stopped'
/** 界面上 workflow 里一个 agent 的状态：在协议的五个之外多一个 `stopped`（任务已经不在跑了，它还停在 queued / running）。 */
export type AgentDisplayState = TaskAgentState | 'stopped'

/**
 * 会话里的后台任务还可能有进展：主 agent 在跑一个 turn（含等审批），或者 daemon 报还有后台任务（`SessionSummary.background_tasks`）。
 * 都没有时，历史里还停在 `running` 的快照永远不会再变（daemon 重启或被强杀后会留下这样的快照）。
 */
export function tasksLive(status: SessionStatus, backgroundTasks: number): boolean {
  return isTurnActive(status) || backgroundTasks > 0
}

export function taskDisplayStatus(task: Pick<TaskSnapshot, 'status'>, live: boolean): TaskDisplayStatus {
  return task.status === 'running' && !live ? 'stopped' : task.status
}

/** 任务已经不会再变（终态，或停了）。 */
export const isTaskSettled = (s: TaskDisplayStatus): boolean => s !== 'running'

/** 任务不在跑了，它的 agent 里还停在 queued / running 的也不会再有进展。 */
export function agentDisplayState(agent: Pick<TaskAgent, 'state'>, task: TaskDisplayStatus): AgentDisplayState {
  return (agent.state === 'queued' || agent.state === 'running') && isTaskSettled(task) ? 'stopped' : agent.state
}

/** 已加载的块里所有任务的最新快照，按第一次出现的顺序。 */
export function allTasks(blocks: readonly Block[]): TaskSnapshot[] {
  const out: TaskSnapshot[] = []
  for (const b of blocks) {
    if (b.type === 'task') out.push(b.task)
    else if (b.type === 'tool' && b.tasks) out.push(...b.tasks)
  }
  return out
}

/** 还在跑的任务（快照是 `running`，而且会话里确实还可能有进展）。 */
export function runningTasks(blocks: readonly Block[], live: boolean): TaskSnapshot[] {
  return live ? allTasks(blocks).filter((t) => t.status === 'running') : []
}

/**
 * 「N 个后台任务在运行」里的 N：已加载的历史里认出来的在跑的任务，和 daemon 报的后台任务数取较大值
 * （两边数的是同一批东西，缓存窗口不含的、旧 daemon 不报的都会让其中一边偏小）。
 */
export function backgroundTaskCount(running: number, backgroundTasks: number): number {
  return Math.max(running, backgroundTasks)
}

export interface AgentGroup {
  /** 这一组所属的阶段；没有阶段信息（或 `phase_index` 对不上）的 agent 在最后一个没有阶段的组里。 */
  phase?: TaskPhase
  agents: TaskAgent[]
}

/**
 * workflow 的 agent 按阶段分组：阶段按 `index` 升序（没有 agent 的阶段也留着，它还没轮到），组内 agent 按 `index` 升序，
 * 对不上任何阶段的放在最后一个没有标题的组里。没有阶段信息时就是一个没有标题的组。
 */
export function groupAgents(phases: readonly TaskPhase[], agents: readonly TaskAgent[]): AgentGroup[] {
  const byIndex = (a: { index: number }, b: { index: number }): number => a.index - b.index
  const sortedPhases = phases.slice().sort(byIndex)
  const known = new Set(sortedPhases.map((p) => p.index))
  const groups: AgentGroup[] = sortedPhases.map((phase) => ({ phase, agents: agents.filter((a) => a.phase_index === phase.index).sort(byIndex) }))
  const loose = agents.filter((a) => a.phase_index == null || !known.has(a.phase_index)).sort(byIndex)
  if (loose.length > 0) groups.push({ agents: loose })
  return groups
}

/** workflow 里各状态的 agent 数。 */
export function agentCounts(agents: readonly TaskAgent[]): Record<TaskAgentState, number> {
  const out: Record<TaskAgentState, number> = { queued: 0, running: 0, done: 0, failed: 0, skipped: 0 }
  for (const a of agents) out[a.state] += 1
  return out
}
