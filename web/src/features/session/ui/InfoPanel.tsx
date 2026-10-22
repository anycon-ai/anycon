import { useState, type ReactNode } from 'react'
import { Archive, ArchiveRestore, Bot, CalendarClock, Download, Folder, GitBranch, Pencil, RotateCw, Trash2, type LucideIcon } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { SessionGitStatus, SessionStatus, SessionWorktree, WorktreeDisposal } from '@/protocol'
import { SessionStatusChip } from '@/components/SessionListItem'
import { ArchiveWorktreeDialog, effectiveChoice, WorktreeDisposePicker, type DisposeChoice, type DisposeInfo } from '@/features/worktree/DisposeDialog'
import { compactNumber, formatCost, formatDateTime } from '@/lib/format'
import type { PendingPermission } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { cn } from '@/ui/cn'
import { Chip, type Tone } from '@/ui/Misc'
import type { UsageSum } from '../reducer'
import { isActive, isTurnActive, type SubagentActivity, type SubagentRow } from '../subagents'
import { toolLabel } from '../text'

export interface InfoPanelProps {
  agentLabel: string
  /** 会话（主 agent）的状态。 */
  status: SessionStatus
  /** 已加载的事件里的子 agent（`subagentRows`）。 */
  subagents: SubagentRow[]
  workDir?: string
  project?: string | null
  createdAt: number
  usage: UsageSum
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

  // 正在干活的：主 agent 在跑一个 turn（含等审批），加上还没结束的子 agent
  const working = (isTurnActive(p.status) ? 1 : 0) + p.subagents.filter((r) => isActive(r.state)).length
  // 收起时保留还在跑的和最近的几个，顺序不变（子 agent 仍跟在父后面）
  const recent = p.subagents.length - SUBAGENT_LIMIT
  const shownSubagents = allSubagents ? p.subagents : p.subagents.filter((r, i) => i >= recent || isActive(r.state))
  const hiddenSubagents = p.subagents.length - shownSubagents.length

  const overview = (
    <div className="flex flex-col gap-3" data-testid="info-overview">
      <Card title={t('info.session')}>
        <IconRow icon={Folder} title={p.workDir}>
          {p.project ? (
            <>
              {p.project}
              {p.workDir ? (
                <>
                  <span className="text-fg-subtle"> · </span>
                  <span className="font-mono text-xs text-fg-subtle">{p.workDir}</span>
                </>
              ) : null}
            </>
          ) : (
            (p.workDir ?? t('info.scratch'))
          )}
        </IconRow>
        {p.worktree ? (
          <IconRow icon={GitBranch} title={p.worktree.branch} aside={`← ${p.worktree.base}`} testId="info-branch">
            <span className="font-mono text-[0.8125rem]">{p.worktree.branch}</span>
          </IconRow>
        ) : null}
        <IconRow icon={CalendarClock} aside={t('info.created')}>
          {formatDateTime(p.createdAt, i18n.language)}
        </IconRow>
      </Card>

      <Card title={t('info.agents')} titleAside={working > 0 ? t('info.working', { count: working }) : undefined} testId="info-agents">
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
      </Card>

      <Card>
        <ValueRow label={t('info.turns')}>{turns}</ValueRow>
        {cost != null ? <ValueRow label={t('info.cost')}>{formatCost(cost)}</ValueRow> : null}
      </Card>

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

/** 一张圆角卡片（信息面板的分组）：可选标题行，标题右侧可放计数之类的弱化文字。 */
function Card({ title, titleAside, testId, tone, children }: { title?: string; titleAside?: string; testId?: string; tone?: 'warning'; children: ReactNode }) {
  return (
    <section className={cn('rounded-xl border px-4 py-3', tone === 'warning' ? 'border-warning/40' : 'border-border')} data-testid={testId}>
      {title ? (
        <h3 className="flex items-center justify-between gap-2 pb-1.5 text-[0.9375rem] font-medium">
          <span className="truncate">{title}</span>
          {titleAside ? <span className="shrink-0 text-sm font-normal text-fg-subtle">{titleAside}</span> : null}
        </h3>
      ) : null}
      {children}
    </section>
  )
}

/** 图标 + 值（单行截断，悬停看全文），右侧可放弱化的补充说明。 */
function IconRow({ icon: Icon, title, aside, testId, children }: { icon: LucideIcon; title?: string; aside?: string; testId?: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 items-center gap-3 py-1.5 text-sm" data-testid={testId}>
      <Icon className="size-4 shrink-0 text-fg-muted" aria-hidden />
      <span className="min-w-0 flex-1 truncate" title={title}>
        {children}
      </span>
      {aside ? <span className="max-w-[45%] shrink-0 truncate text-fg-subtle" title={aside}>{aside}</span> : null}
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

/** 名称在左、数值在右的一行（用量卡片）。 */
function ValueRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3 py-1.5 text-sm">
      <span className="shrink-0 font-medium">{label}</span>
      <span className="min-w-0 truncate text-right text-fg-subtle">{children}</span>
    </div>
  )
}
