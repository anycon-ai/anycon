import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Archive, ArchiveRestore, Bot, ChevronDown, Download, FolderOpen, Pencil, RotateCw, SquareTerminal, Trash2, type LucideIcon } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Collapsible } from 'radix-ui'
import type { TFunction } from 'i18next'
import type { SessionGitStatus, SessionStatus, SessionWorktree, UsageWindow, WorktreeDisposal } from '@/protocol'
import { SessionStatusChip } from '@/components/SessionListItem'
import { ArchiveWorktreeDialog, effectiveChoice, WorktreeDisposePicker, type DisposeChoice, type DisposeInfo } from '@/features/worktree/DisposeDialog'
import { compactNumber, formatCost, formatDateTime } from '@/lib/format'
import { usePrefs } from '@/stores/prefs'
import type { PendingPermission } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { cn } from '@/ui/cn'
import { Chip, CopyButton, type Tone } from '@/ui/Misc'
import type { ContextUsage, ToolBlock, UsageSum } from '../reducer'
import { isActive, isTurnActive, type SubagentActivity, type SubagentRow } from '../subagents'
import { toolCommand, toolDescription, toolLabel } from '../text'

export interface InfoPanelProps {
  agentLabel: string
  /** 会话（主 agent）的状态。 */
  status: SessionStatus
  /** 已加载的事件里的子 agent（`subagentRows`）。 */
  subagents: SubagentRow[]
  /** 已加载的事件里还在后台跑的命令（`backgroundCommands`）。 */
  backgroundCommands?: ToolBlock[]
  /** daemon 报的后台任务数（`SessionSummary.background_tasks`，含后台子 agent 和后台命令）：比已加载的事件里看到的多时，多出来的另起一行。 */
  backgroundTasks?: number
  workDir?: string
  project?: string | null
  /** 打开工作目录的文件浏览（`/n/:node/files/*`）；没有能打开的目录（项目目录不在了）时不传，工作目录就是普通文字。 */
  onOpenWorkDir?: () => void
  createdAt: number
  usage: UsageSum
  /** 最近一个 turn 结束时的上下文占用（`turn_done.usage.context_*`）；agent 不报就没有，不显示这一行。 */
  context?: ContextUsage | null
  /** 最近一次 `turn_done.usage.limits` 报的账号用量窗口（5 小时、周…）；agent 不报就没有这张卡片。 */
  limits?: UsageWindow[]
  /** 事件流里最新一条事件的序号。turn 进行中它每往前走一次，就当作 agent 又有了输出（token 在消耗），「Agent」卡片的背景动一下。 */
  activitySeq?: number
  /** daemon 汇总的累计（SessionSummary.usage）。`session.changed` 不带用量，这份可能落后于已收到的事件，所以和本地算的取较大值。 */
  usageTotals?: { turns: number; input_tokens: number; output_tokens: number; cost_usd?: number | null }
  pending: Array<Pick<PendingPermission, 'request_id' | 'title'>>
  onFocusPending: (requestId: string) => void
  archived: boolean
  /** 会话在运行或等审批：daemon 会拒绝归档（`busy`），按钮禁用并说明「先停止或答复」。取消归档不受影响。 */
  archiveBlocked?: boolean
  disabled?: boolean
  /** 会话在 anycon 建的 worktree 里工作时给出（ADR-34）：概览显示分支，归档 / 删除先弹确认框。 */
  worktree?: SessionWorktree | null
  gitStatus?: SessionGitStatus | null
  /** 打开归档 / 删除确认框时调用（刷新 git 状态，让「N 个未提交的改动」是新的）。 */
  onDisposeOpen?: () => void
  onRename: () => void
  /** 没有 worktree 的会话（或取消归档）不带参数，行为和以前一样。 */
  onArchive: (opts?: { worktree: WorktreeDisposal }) => void
  onDelete: (opts?: { worktree: WorktreeDisposal; deleteBranch: boolean }) => void
  onExport: () => void
  onRestart: () => void
}

const SUBAGENT_TONE: Record<SubagentActivity, Tone> = { running: 'accent', background: 'info', done: 'neutral', error: 'danger', stopped: 'neutral' }
/** 子 agent 多于这个数时，只列还在跑的和最近的这么多个，其余收起。 */
const SUBAGENT_LIMIT = 6
const INDENT = ['', 'pl-4', 'pl-8', 'pl-12']
/** 「Agent」卡片背景动画的最短间隔：和 `.token-sweep` 的时长一致，流式输出时一道接一道，不重叠。 */
const PULSE_GAP_MS = 1200

export function InfoPanel(p: InfoPanelProps) {
  const { t, i18n } = useTranslation()
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmArchive, setConfirmArchive] = useState(false)
  const [choice, setChoice] = useState<DisposeChoice>({ worktree: 'auto', deleteBranch: false })
  const [allSubagents, setAllSubagents] = useState(false)
  const archiveBlocked = !!p.archiveBlocked && !p.archived
  // 目录已经没有了（removed）或根本没建成（failed）的 worktree 没什么可处理的：照旧
  const wt = p.worktree && p.worktree.state !== 'removed' && p.worktree.state !== 'failed' ? p.worktree : null
  const disposeInfo: DisposeInfo | null = wt
    ? { branch: wt.branch, uncommitted: p.gitStatus?.uncommitted ?? 0, ahead: p.gitStatus?.ahead ?? 0, unpushed: p.gitStatus?.unpushed ?? 0 }
    : null
  const onArchiveClick = (): void => {
    if (!p.archived && disposeInfo) {
      p.onDisposeOpen?.()
      setConfirmArchive(true)
    } else p.onArchive()
  }
  const onDeleteClick = (): void => {
    if (disposeInfo) {
      p.onDisposeOpen?.()
      setChoice({ worktree: 'auto', deleteBranch: false })
    }
    setConfirmDelete(true)
  }
  // 累计量只增不减：daemon 的汇总可能落后（只在 session.list / get 时更新），本地从缓存窗口算的可能不全，取较大值。
  const turns = Math.max(p.usageTotals?.turns ?? 0, p.usage.turns)
  const input = Math.max(p.usageTotals?.input_tokens ?? 0, p.usage.inputTokens)
  const output = Math.max(p.usageTotals?.output_tokens ?? 0, p.usage.outputTokens)
  const cost =
    p.usageTotals?.cost_usd == null && p.usage.costUsd == null ? null : Math.max(p.usageTotals?.cost_usd ?? 0, p.usage.costUsd ?? 0)

  const commands = p.backgroundCommands ?? []
  // daemon 数到的后台任务比事件里认出来的多（事件不在缓存窗口里、daemon 旧了没标后台命令）：多出来的只给一个数
  const knownBackground = p.subagents.filter((r) => r.state === 'background').length + commands.length
  const otherBackground = Math.max(0, (p.backgroundTasks ?? 0) - knownBackground)
  // 正在干活的：主 agent 在跑一个 turn（含等审批），加上还没结束的子 agent 和后台命令
  const working = (isTurnActive(p.status) ? 1 : 0) + p.subagents.filter((r) => isActive(r.state)).length + commands.length + otherBackground
  // 收起时保留还在跑的和最近的几个，顺序不变（子 agent 仍跟在父后面）
  const recent = p.subagents.length - SUBAGENT_LIMIT
  const shownSubagents = allSubagents ? p.subagents : p.subagents.filter((r, i) => i >= recent || isActive(r.state))
  const hiddenSubagents = p.subagents.length - shownSubagents.length

  // token 数变了（主 agent 的累计、子 agent 报的），或者 turn 进行中事件流往前走了：背景动一下。
  // 主 agent 的 token 只在 turn 结束时才有新数，所以 turn 进行中拿「有新输出」当作 token 在消耗
  const tokenSignal = [input, output, ...p.subagents.map((r) => r.tokens ?? 0)].join(':')
  const pulse = usePulse(isTurnActive(p.status) ? `${tokenSignal}@${p.activitySeq ?? 0}` : tokenSignal, PULSE_GAP_MS)

  const now = Date.now()
  // 恢复时间已经过了的窗口：那个数是上一个窗口的，现在是多少不知道，不显示
  const limits = (p.limits ?? []).filter((w) => w.resets_at_ms == null || w.resets_at_ms > now)

  const openWorkDir = p.onOpenWorkDir ? (
    <button
      type="button"
      onClick={p.onOpenWorkDir}
      title={t('files.openWorkdir')}
      aria-label={t('files.openWorkdir')}
      className="inline-flex items-center rounded-md px-1.5 py-1 text-fg-subtle hover:bg-bg-hover hover:text-fg"
      data-testid="info-workdir-link"
    >
      <FolderOpen className="size-3.5" />
    </button>
  ) : null

  const overview = (
    <div className="flex flex-col gap-3" data-testid="info-overview">
      <Card title={t('info.session')} collapseKey="session" testId="info-session">
        {p.project ? (
          <Field label={t('info.project')} actions={p.workDir ? null : openWorkDir} testId="info-project">
            {p.project}
          </Field>
        ) : null}
        {p.workDir ? (
          <Field
            label={t('info.workdir')}
            actions={
              <>
                {openWorkDir}
                <CopyButton text={p.workDir} iconOnly testId="info-workdir-copy" />
              </>
            }
            testId="info-workdir"
          >
            <span className="font-mono text-[0.8125rem] break-all">{p.workDir}</span>
          </Field>
        ) : p.project ? null : (
          <Field label={t('info.workdir')} actions={openWorkDir} testId="info-workdir">
            {t('info.scratch')}
          </Field>
        )}
        {p.worktree ? (
          <Field label={t('info.branch')} actions={<CopyButton text={p.worktree.branch} iconOnly testId="info-branch-copy" />} testId="info-branch">
            <span className="font-mono text-[0.8125rem] break-all">{p.worktree.branch}</span>
            <span className="text-fg-subtle"> ← {p.worktree.base}</span>
          </Field>
        ) : null}
        <Field label={t('info.created')}>{formatDateTime(p.createdAt, i18n.language)}</Field>
      </Card>

      <Card title={t('info.agents')} titleAside={working > 0 ? t('info.working', { count: working }) : undefined} testId="info-agents" pulse={pulse} collapseKey="agents">
        <AgentRow
          icon={Bot}
          label={p.agentLabel}
          meta={input + output > 0 ? `${t('usage.input', { n: compactNumber(input) })} / ${t('usage.output', { n: compactNumber(output) })}` : undefined}
          aside={<SessionStatusChip status={p.status} />}
          testId="info-agent-main"
        />
        {hiddenSubagents > 0 ? (
          <button type="button" className="py-1 pl-4 text-xs text-fg-subtle hover:text-fg-muted" onClick={() => setAllSubagents(true)} data-testid="info-subagents-more">
            {t('info.showEarlierAgents', { count: hiddenSubagents })}
          </button>
        ) : null}
        {shownSubagents.map((r) => (
          <AgentRow
            key={r.block.key}
            depth={r.depth + 1}
            label={toolLabel(r.block)}
            meta={[r.tokens != null ? t('info.agentTokens', { n: compactNumber(r.tokens) }) : null, r.calls > 0 ? t('tool.subagentCalls', { count: r.calls }) : null].filter(Boolean).join(' · ') || undefined}
            aside={<Chip tone={SUBAGENT_TONE[r.state]}>{t(`info.subagentState.${r.state}`)}</Chip>}
            testId="info-subagent"
            state={r.state}
          />
        ))}
        {commands.map((b) => (
          <AgentRow
            key={b.key}
            icon={SquareTerminal}
            depth={1}
            label={b.category === 'shell' ? (toolDescription(b) ?? `$ ${toolCommand(b)}`) : toolLabel(b)}
            aside={<Chip tone={SUBAGENT_TONE.background}>{t('info.subagentState.background')}</Chip>}
            testId="info-background-command"
            state="background"
          />
        ))}
        {otherBackground > 0 ? (
          <AgentRow
            icon={SquareTerminal}
            depth={1}
            label={t(knownBackground > 0 ? 'info.moreBackgroundTasks' : 'info.backgroundTasks', { count: otherBackground })}
            aside={<Chip tone={SUBAGENT_TONE.background}>{t('info.subagentState.background')}</Chip>}
            testId="info-background-other"
            state="background"
          />
        ) : null}
      </Card>

      <Card>
        <ValueRow label={t('info.turns')}>{turns}</ValueRow>
        {cost != null ? <ValueRow label={t('info.cost')}>{formatCost(cost)}</ValueRow> : null}
        {p.context && p.context.window > 0 ? <ContextRow context={p.context} /> : null}
      </Card>

      {limits.length > 0 ? (
        <Card title={t('info.limits')} titleAside={p.agentLabel} testId="info-limits" collapseKey="limits">
          {limits.map((w) => (
            <LimitRow key={`${w.model ?? ''}:${w.window_minutes ?? ''}`} limit={w} now={now} />
          ))}
        </Card>
      ) : null}

      {p.pending.length > 0 ? (
        <Card title={t('info.pending', { count: p.pending.length })} testId="info-pending" tone="warning">
          <ul className="space-y-1 pt-1">
            {p.pending.map((r) => (
              <li key={r.request_id}>
                <button type="button" className="w-full truncate rounded-md border border-warning/40 bg-warning-subtle px-2.5 py-1.5 text-left text-sm hover:opacity-90" onClick={() => p.onFocusPending(r.request_id)}>
                  {r.title}
                </button>
              </li>
            ))}
          </ul>
        </Card>
      ) : null}

      <div className="px-1 pt-1">
        <div className="flex flex-wrap gap-1.5">
          <Button size="sm" variant="ghost" icon={<Pencil className="size-3.5" />} onClick={p.onRename} disabled={p.disabled}>
            {t('info.rename')}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            icon={p.archived ? <ArchiveRestore className="size-3.5" /> : <Archive className="size-3.5" />}
            onClick={onArchiveClick}
            disabled={p.disabled || archiveBlocked}
            title={archiveBlocked ? t('info.archiveBusy') : undefined}
            aria-describedby={archiveBlocked ? 'session-archive-blocked' : undefined}
            data-testid="session-archive"
          >
            {p.archived ? t('info.unarchive') : t('info.archive')}
          </Button>
          <Button size="sm" variant="ghost" icon={<Download className="size-3.5" />} onClick={p.onExport}>
            {t('info.export')}
          </Button>
          <Button size="sm" variant="ghost" icon={<RotateCw className="size-3.5" />} onClick={p.onRestart} disabled={p.disabled}>
            {t('info.restart')}
          </Button>
          <Button size="sm" variant="ghost" className="text-danger hover:bg-danger-subtle hover:text-danger" icon={<Trash2 className="size-3.5" />} onClick={onDeleteClick} disabled={p.disabled} data-testid="session-delete">
            {t('info.delete')}
          </Button>
        </div>
        {archiveBlocked ? (
          <p id="session-archive-blocked" className="mt-2 px-2 text-xs text-fg-subtle" data-testid="session-archive-blocked">
            {t('info.archiveBusy')}
          </p>
        ) : null}
      </div>
    </div>
  )

  return (
    <div className="scroll-thin flex h-full flex-col overflow-y-auto p-4" data-testid="info-panel">
      {overview}

      <Dialog open={confirmDelete} onOpenChange={setConfirmDelete} title={t('info.deleteTitle')} description={t('info.deleteBody')} closeLabel={t('common.close')} testId="delete-dialog">
        {disposeInfo ? (
          <div className="mb-4">
            <WorktreeDisposePicker idPrefix="delete" info={disposeInfo} value={choice} onChange={setChoice} withBranch />
          </div>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setConfirmDelete(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="danger"
            data-testid="session-delete-confirm"
            onClick={() => {
              setConfirmDelete(false)
              p.onDelete(disposeInfo ? effectiveChoice(choice, disposeInfo) : undefined)
            }}
          >
            {t('info.deleteConfirm')}
          </Button>
        </div>
      </Dialog>

      {disposeInfo ? (
        <ArchiveWorktreeDialog
          open={confirmArchive}
          onOpenChange={setConfirmArchive}
          info={disposeInfo}
          onConfirm={(c) => {
            setConfirmArchive(false)
            p.onArchive({ worktree: c.worktree })
          }}
        />
      ) : null}
    </div>
  )
}

/**
 * `signal` 每变一次，返回的计数加一（挂载时的值不算）。两次之间至少隔 `gapMs`：间隔里的变化合并成间隔结束时的一次，
 * 所以连续变化时是一次接一次，停下来之后最多再有一次。
 */
function usePulse(signal: string, gapMs: number): number {
  const [count, setCount] = useState(0)
  const state = useRef<{ signal: string; at: number; timer: ReturnType<typeof setTimeout> | null }>({ signal, at: 0, timer: null })
  useEffect(() => {
    const s = state.current
    if (s.signal === signal) return
    s.signal = signal
    if (s.timer !== null) return
    s.timer = setTimeout(
      () => {
        s.timer = null
        s.at = Date.now()
        setCount((n) => n + 1)
      },
      Math.max(0, s.at + gapMs - Date.now()),
    )
  }, [signal, gapMs])
  useEffect(() => {
    const s = state.current
    return () => {
      if (s.timer !== null) clearTimeout(s.timer)
      s.timer = null
    }
  }, [])
  return count
}

/**
 * 一张圆角卡片（信息面板的分组）：可选标题行，标题右侧可放计数之类的弱化文字。
 * `pulse` 每换一个非零的值，卡片背景扫过一道光（`.token-sweep`，换 key 重新挂载来重放）。
 * 有 `collapseKey` 的带标题卡片可以收起：整个标题行是开关，收起后只剩标题行（右侧文字照常显示），
 * 开合记在偏好里，所有会话共用。
 */
function Card({
  title,
  titleAside,
  testId,
  tone,
  pulse,
  collapseKey,
  children,
}: {
  title?: string
  titleAside?: string
  testId?: string
  tone?: 'warning'
  pulse?: number
  collapseKey?: string
  children: ReactNode
}) {
  const collapsed = usePrefs((s) => (collapseKey ? s.infoCollapsed.includes(collapseKey) : false))
  const setCollapsed = usePrefs((s) => s.setInfoCollapsed)
  const className = cn('rounded-xl border px-4 py-3', tone === 'warning' ? 'border-warning/40' : 'border-border', pulse !== undefined && 'relative isolate overflow-hidden')
  const sweep = pulse ? <span key={pulse} className="token-sweep" aria-hidden /> : null
  const aside = titleAside ? <span className="shrink-0 text-sm font-normal text-fg-subtle">{titleAside}</span> : null
  if (title && collapseKey) {
    return (
      <Collapsible.Root asChild open={!collapsed} onOpenChange={(open) => setCollapsed(collapseKey, !open)}>
        <section className={className} data-testid={testId} data-pulse={pulse} data-collapsed={collapsed || undefined}>
          {sweep}
          <h3 className={cn('text-[0.9375rem] font-medium', !collapsed && 'pb-1.5')}>
            <Collapsible.Trigger className="group -mx-1 flex w-[calc(100%+0.5rem)] items-center gap-2 rounded-md px-1 text-left outline-none focus-visible:ring-2 focus-visible:ring-accent/40" data-testid={testId ? `${testId}-toggle` : undefined}>
              <span className="min-w-0 flex-1 truncate">{title}</span>
              {aside}
              <ChevronDown className="size-4 shrink-0 text-fg-subtle transition-transform group-hover:text-fg-muted group-data-[state=closed]:-rotate-90" aria-hidden />
            </Collapsible.Trigger>
          </h3>
          <Collapsible.Content>{children}</Collapsible.Content>
        </section>
      </Collapsible.Root>
    )
  }
  return (
    <section className={className} data-testid={testId} data-pulse={pulse}>
      {sweep}
      {title ? (
        <h3 className="flex items-center justify-between gap-2 pb-1.5 text-[0.9375rem] font-medium">
          <span className="truncate">{title}</span>
          {aside}
        </h3>
      ) : null}
      {children}
    </section>
  )
}

/** 一行名称、一行值。值整段显示（长路径折行，不截断），可以选中；名称右侧放这个值的操作（复制、打开）。 */
function Field({ label, actions, testId, children }: { label: string; actions?: ReactNode; testId?: string; children: ReactNode }) {
  return (
    <div className="py-1.5 text-sm" data-testid={testId}>
      <div className="flex min-h-6 items-center justify-between gap-2">
        <span className="text-xs text-fg-subtle">{label}</span>
        {actions ? <span className="-mr-1.5 flex shrink-0 items-center">{actions}</span> : null}
      </div>
      <div className="[overflow-wrap:anywhere] select-text">{children}</div>
    </div>
  )
}

/** 一个 agent：名称（单行截断）和下面一行弱化的用量，右侧是状态。子 agent 按嵌套层数缩进、用 └ 引出。 */
function AgentRow({ icon: Icon, depth = 0, label, meta, aside, testId, state }: { icon?: LucideIcon; depth?: number; label: string; meta?: string; aside: ReactNode; testId?: string; state?: string }) {
  return (
    <div className={cn('flex min-w-0 items-start gap-2.5 py-1.5 text-sm', INDENT[Math.min(depth, INDENT.length - 1)])} data-testid={testId} data-state={state}>
      {Icon ? <Icon className="mt-0.5 size-4 shrink-0 text-fg-muted" aria-hidden /> : <span className="shrink-0 text-fg-subtle" aria-hidden>└</span>}
      <span className="min-w-0 flex-1">
        <span className="block truncate" title={label}>
          {label}
        </span>
        {meta ? <span className="block truncate text-xs text-fg-subtle">{meta}</span> : null}
      </span>
      <span className="shrink-0">{aside}</span>
    </div>
  )
}

/** 上下文占用：一条进度条加「百分比 · 已用 / 窗口」。`tokens` 是 turn 结束时窗口里的 token 数，不是累计用量。 */
export function contextPercent(c: ContextUsage): number {
  return c.window > 0 ? Math.max(0, Math.round((c.tokens / c.window) * 100)) : 0
}

function ContextRow({ context }: { context: ContextUsage }) {
  const { t } = useTranslation()
  const percent = contextPercent(context)
  return (
    <MeterRow label={t('info.context')} percent={percent} testId="info-context">
      {t('info.contextValue', { percent, used: compactNumber(context.tokens), window: compactNumber(context.window) })}
    </MeterRow>
  )
}

/** 用量窗口的名字：5 小时和一周有固定的叫法，其它长度按天 / 小时 / 分钟写，不知道长度的只说「用量额度」；按模型的窗口后面带模型名。 */
export function limitLabel(t: TFunction, w: Pick<UsageWindow, 'window_minutes' | 'model'>): string {
  const m = w.window_minutes
  const name =
    m == null
      ? t('info.limitUnknown')
      : m === 300
        ? t('info.limit5h')
        : m === 10_080
          ? t('info.limitWeekly')
          : m % 1440 === 0
            ? t('info.limitDays', { count: m / 1440 })
            : m % 60 === 0
              ? t('info.limitHours', { count: m / 60 })
              : t('info.limitMinutes', { count: m })
  return w.model ? `${name} · ${w.model}` : name
}

/** 恢复时间：当天只写时间，其它日子带月日（不带年份，窗口最长一周）。 */
function formatLimitReset(ms: number, lang: string, now: number): string {
  const d = new Date(ms)
  const sameDay = d.toDateString() === new Date(now).toDateString()
  return sameDay ? d.toLocaleTimeString(lang, { timeStyle: 'short' }) : d.toLocaleString(lang, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' })
}

/** 一个账号用量窗口：进度条加「百分比 · 恢复时间」。 */
function LimitRow({ limit, now }: { limit: UsageWindow; now: number }) {
  const { t, i18n } = useTranslation()
  const percent = Math.max(0, Math.round(limit.used_percent))
  return (
    <MeterRow label={limitLabel(t, limit)} percent={percent} testId="info-limit">
      {limit.resets_at_ms != null ? t('info.limitValueResets', { percent, time: formatLimitReset(limit.resets_at_ms, i18n.language, now) }) : t('info.limitValue', { percent })}
    </MeterRow>
  )
}

/** 名称在左、数值在右，下面一条进度条（75% 起警告色，90% 起危险色；超过 100% 画满）。放不下时数值折到下一行。 */
function MeterRow({ label, percent, testId, children }: { label: string; percent: number; testId: string; children: ReactNode }) {
  return (
    <div className="py-1.5 text-sm" data-testid={testId} data-percent={percent}>
      <div className="flex flex-wrap items-center justify-between gap-x-3">
        <span className="font-medium">{label}</span>
        <span className="ml-auto text-right text-fg-subtle tabular-nums" data-testid={`${testId}-value`}>
          {children}
        </span>
      </div>
      <div className="mt-1.5 h-1.5 overflow-hidden rounded-full bg-bg-muted" role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.min(100, percent)}>
        <div className={cn('h-full rounded-full', percent >= 90 ? 'bg-danger' : percent >= 75 ? 'bg-warning' : 'bg-accent')} style={{ width: `${Math.min(100, percent)}%` }} />
      </div>
    </div>
  )
}

/** 名称在左、数值在右的一行（用量卡片）。 */
function ValueRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3 py-1.5 text-sm">
      <span className="shrink-0 font-medium">{label}</span>
      <span className="min-w-0 truncate text-right text-fg-subtle">{children}</span>
    </div>
  )
}
