// 后台任务（`task` 事件，spec/web.md § 4.6）的界面：挂在工具行下面或单独成块的进度，以及输入框上方的「N 个后台任务在运行」。
import { createContext, useContext, useMemo, useState } from 'react'
import { Popover } from 'radix-ui'
import { Check, ChevronRight, CircleDashed, CircleMinus, CircleSlash, Loader2, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import { compactNumber, formatDuration } from '@/lib/format'
import { cn } from '@/ui/cn'
import { Chip, type Tone } from '@/ui/Misc'
import type { TaskSnapshot } from '../reducer'
import { agentCounts, agentDisplayState, groupAgents, taskDisplayStatus, type AgentDisplayState, type TaskDisplayStatus } from '../tasks'

/**
 * 会话里的后台任务还可能有进展吗（`tasksLive`）。不可能时，快照停在 `running` 的显示成「已停止」。
 * 默认 true：没人提供时按快照本身显示。
 */
export const TaskLiveContext = createContext(true)

type IconState = TaskDisplayStatus | AgentDisplayState

const TONE: Record<IconState, Tone> = {
  running: 'accent',
  completed: 'success',
  done: 'success',
  failed: 'danger',
  cancelled: 'neutral',
  stopped: 'neutral',
  queued: 'neutral',
  skipped: 'neutral',
}

/** 状态图标：形状本身就区分状态，不只靠颜色。 */
function StateIcon({ state, className }: { state: IconState; className?: string }) {
  const cls = cn('size-3.5 shrink-0', className)
  switch (state) {
    case 'running':
      return <Loader2 className={cn(cls, 'animate-spin')} aria-hidden />
    case 'completed':
    case 'done':
      return <Check className={cn(cls, 'text-success')} aria-hidden />
    case 'failed':
      return <X className={cn(cls, 'text-danger')} aria-hidden />
    case 'queued':
      return <CircleDashed className={cn(cls, 'text-fg-subtle')} aria-hidden />
    case 'skipped':
      return <CircleMinus className={cn(cls, 'text-fg-subtle')} aria-hidden />
    case 'cancelled':
    case 'stopped':
      return <CircleSlash className={cn(cls, 'text-fg-subtle')} aria-hidden />
  }
}

/** token 数 / 工具调用数 / 用时，agent 没报的跳过。 */
function usageParts(t: TFunction, u: { tokens?: number | null; toolCalls?: number | null; durationMs?: number | null }): string[] {
  const out: string[] = []
  if (u.tokens != null) out.push(t('info.agentTokens', { n: compactNumber(u.tokens) }))
  if (u.toolCalls != null) out.push(t('tool.subagentCalls', { count: u.toolCalls }))
  if (u.durationMs != null && u.durationMs > 0) out.push(formatDuration(u.durationMs))
  return out
}

function taskMeta(t: TFunction, task: TaskSnapshot): string[] {
  const out: string[] = []
  if (task.agents.length > 0) out.push(t('task.agentsDone', { done: agentCounts(task.agents).done, total: task.agents.length }))
  out.push(...usageParts(t, { tokens: task.usage?.total_tokens, toolCalls: task.usage?.tool_calls, durationMs: task.usage?.duration_ms }))
  return out
}

/** 一个任务的一行标题：状态、名字、用量、最近调用的工具。 */
function Headline({ task, status }: { task: TaskSnapshot; status: TaskDisplayStatus }) {
  const { t } = useTranslation()
  const title = task.title.trim() || t('task.title')
  const meta = taskMeta(t, task)
  return (
    <>
      <Chip tone={TONE[status]} icon={<StateIcon state={status} className={status === 'running' ? 'text-accent-text' : undefined} />} testId="task-status" dataState={status}>
        {t(`task.status.${status}`)}
      </Chip>
      <span className="min-w-0 max-w-full truncate font-medium text-fg-muted" title={title} data-testid="task-title">
        {title}
      </span>
      {meta.length > 0 ? (
        <span className="text-xs" data-testid="task-meta">
          {meta.join(' · ')}
        </span>
      ) : null}
      {task.lastTool ? (
        <span className="min-w-0 max-w-full truncate font-mono text-xs" title={task.lastTool} data-testid="task-last-tool">
          {task.lastTool}
        </span>
      ) : null}
    </>
  )
}

function Summary({ text }: { text: string }) {
  return (
    <p className="mt-0.5 line-clamp-2 text-xs text-fg-muted [overflow-wrap:anywhere]" title={text} data-testid="task-summary">
      {text}
    </p>
  )
}

function AgentRow({ agent, taskStatus }: { agent: TaskSnapshot['agents'][number]; taskStatus: TaskDisplayStatus }) {
  const { t } = useTranslation()
  const state = agentDisplayState(agent, taskStatus)
  const usage = usageParts(t, { tokens: agent.tokens, toolCalls: agent.tool_calls, durationMs: agent.duration_ms })
  const label = agent.label.trim() || `#${agent.index}`
  return (
    <li className="flex items-start gap-2 px-3 py-1.5" data-testid="task-agent" data-state={state} data-index={agent.index}>
      <span className="mt-0.5 shrink-0" role="img" aria-label={t(`task.agentState.${state}`)} title={t(`task.agentState.${state}`)}>
        <StateIcon state={state} className={state === 'running' ? 'text-accent-text' : undefined} />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className={cn('min-w-0 max-w-full truncate', state === 'failed' ? 'text-danger' : 'text-fg')} title={agent.model ? `${label} · ${agent.model}` : label} data-testid="task-agent-label">
            {label}
          </span>
          {agent.attempt != null && agent.attempt > 1 ? (
            <span className="shrink-0 text-xs text-warning" data-testid="task-agent-attempt">
              {t('task.attempt', { n: agent.attempt })}
            </span>
          ) : null}
          {agent.cached ? (
            <Chip testId="task-agent-cached" className="shrink-0">
              {t('task.cached')}
            </Chip>
          ) : null}
        </div>
        {usage.length > 0 || agent.last_tool ? (
          <p className="flex min-w-0 flex-wrap gap-x-2 text-xs text-fg-subtle" data-testid="task-agent-meta">
            {usage.length > 0 ? <span>{usage.join(' · ')}</span> : null}
            {agent.last_tool ? <span className="min-w-0 max-w-full truncate font-mono">{agent.last_tool}</span> : null}
          </p>
        ) : null}
        {agent.error ? (
          <p className="mt-0.5 line-clamp-3 text-xs text-danger [overflow-wrap:anywhere]" title={agent.error} data-testid="task-agent-error">
            {agent.error}
          </p>
        ) : null}
      </div>
    </li>
  )
}

/**
 * 一个任务的进度。一行标题（状态、名字、用量、最近调用的工具）+ 摘要；workflow 展开后按阶段分组、每个 agent 一行。
 * 跑着的和失败的默认展开，其余收起；用户点过之后按用户的来。
 */
export function TaskPanel({ task }: { task: TaskSnapshot }) {
  const { t } = useTranslation()
  const live = useContext(TaskLiveContext)
  const status = taskDisplayStatus(task, live)
  const [userOpen, setUserOpen] = useState<boolean | null>(null)
  const groups = useMemo(() => groupAgents(task.phases, task.agents), [task.phases, task.agents])
  const expandable = task.agents.length > 0
  const open = expandable && (userOpen ?? (status === 'running' || status === 'failed'))
  const line = cn('flex max-w-full flex-wrap items-center gap-x-2 gap-y-0.5 py-0.5 text-left', status === 'failed' ? 'text-danger' : 'text-fg-subtle')
  return (
    <div className="text-sm" data-testid="task" data-kind={task.kind} data-status={status} data-task-id={task.id}>
      {expandable ? (
        <button type="button" className={cn(line, 'hover:text-fg-muted')} aria-expanded={open} onClick={() => setUserOpen(!open)}>
          <Headline task={task} status={status} />
          <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden />
        </button>
      ) : (
        <div className={line}>
          <Headline task={task} status={status} />
        </div>
      )}
      {task.summary ? <Summary text={task.summary} /> : null}
      {open ? (
        <div className="mt-1 overflow-hidden rounded-lg border border-border bg-bg" data-testid="task-agents">
          {groups.map((g, i) => (
            <section key={g.phase?.index ?? `loose-${i}`} className={cn(i > 0 && 'border-t border-border')} data-testid="task-phase" data-phase={g.phase?.index}>
              {g.phase ? <h4 className="px-3 pt-2 text-xs font-medium text-fg-muted [overflow-wrap:anywhere]">{g.phase.title || t('task.phase', { n: g.phase.index + 1 })}</h4> : null}
              {g.agents.length > 0 ? (
                <ul className="py-0.5">
                  {g.agents.map((a) => (
                    <AgentRow key={a.index} agent={a} taskStatus={status} />
                  ))}
                </ul>
              ) : (
                <div className="pb-2" />
              )}
            </section>
          ))}
        </div>
      ) : null}
    </div>
  )
}

/** 挂在工具行下面的任务（按第一次出现的顺序）：左边一条竖线，和子 agent 的缩进一致。 */
export function TaskList({ tasks }: { tasks: readonly TaskSnapshot[] }) {
  return (
    <div className="ml-1.5 mt-1 space-y-1.5 border-l-2 border-border pl-3" data-testid="task-list">
      {tasks.map((task) => (
        <TaskPanel key={task.id} task={task} />
      ))}
    </div>
  )
}

/** 输入框上方的「N 个后台任务在运行」：点开列出在跑的任务（名字、状态、用量）。 */
export function BackgroundTasksChip({ tasks, count, idle }: { tasks: readonly TaskSnapshot[]; count: number; idle: boolean }) {
  const { t } = useTranslation()
  // 认出来的不够数（事件不在已加载的历史里、旧 daemon 不报）：多出来的另起一行
  const unknown = Math.max(0, count - tasks.length)
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button
          type="button"
          className="inline-flex max-w-full items-center gap-1.5 rounded-full border border-border bg-bg-subtle px-2.5 py-1 text-xs text-fg-muted transition-colors hover:bg-bg-hover hover:text-fg"
          data-testid="background-tasks-chip"
          data-count={count}
        >
          <Loader2 className="size-3 shrink-0 animate-spin" aria-hidden />
          <span className="truncate">{t('task.chip', { count })}</span>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content side="top" align="start" sideOffset={6} collisionPadding={8} className="z-50 w-96 max-w-[92vw] rounded-lg border border-border bg-surface p-3 shadow-pop focus:outline-none" data-testid="background-tasks-list">
          <h3 className="pb-1.5 text-sm font-medium">{t('task.listTitle')}</h3>
          <ul className="scroll-thin max-h-72 space-y-2 overflow-y-auto">
            {tasks.map((task) => (
              <li key={task.id} data-testid="background-task" data-task-id={task.id}>
                <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-sm text-fg-subtle">
                  <Headline task={task} status="running" />
                </div>
                {task.summary ? <Summary text={task.summary} /> : null}
              </li>
            ))}
            {unknown > 0 ? (
              <li className="text-sm text-fg-subtle" data-testid="background-task-other">
                {t(tasks.length > 0 ? 'info.moreBackgroundTasks' : 'info.backgroundTasks', { count: unknown })}
              </li>
            ) : null}
          </ul>
          {idle ? <p className="mt-2 border-t border-border pt-2 text-xs text-fg-subtle">{t('task.willContinue')}</p> : null}
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  )
}
