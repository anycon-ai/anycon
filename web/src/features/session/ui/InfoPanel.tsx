import { useState, type ReactNode } from 'react'
import { Archive, ArchiveRestore, Bot, CalendarClock, Download, FileDiff, Folder, GitBranch, Pencil, RotateCw, Trash2, type LucideIcon } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { FileChangeKind, SessionGitStatus, SessionWorktree, WorktreeDisposal } from '@/protocol'
import { ArchiveWorktreeDialog, effectiveChoice, WorktreeDisposePicker, type DisposeChoice, type DisposeInfo } from '@/features/worktree/DisposeDialog'
import { compactNumber, formatCost, formatDateTime } from '@/lib/format'
import type { PendingPermission } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { cn } from '@/ui/cn'
import { Chip } from '@/ui/Misc'
import type { UsageSum } from '../reducer'

export interface InfoPanelProps {
  agentLabel: string
  settingsControls: ReactNode
  workDir?: string
  project?: string | null
  createdAt: number
  usage: UsageSum
  /** daemon 汇总的累计（SessionSummary.usage）。`session.changed` 不带用量，这份可能落后于已收到的事件，所以和本地算的取较大值。 */
  usageTotals?: { turns: number; input_tokens: number; output_tokens: number; cost_usd?: number | null }
  pending: Array<Pick<PendingPermission, 'request_id' | 'title'>>
  onFocusPending: (requestId: string) => void
  changedFiles: Record<string, FileChangeKind>
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

const KIND_TONE: Record<FileChangeKind, 'success' | 'warning' | 'danger'> = { created: 'success', modified: 'warning', deleted: 'danger' }

export function InfoPanel(p: InfoPanelProps) {
  const { t, i18n } = useTranslation()
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmArchive, setConfirmArchive] = useState(false)
  const [choice, setChoice] = useState<DisposeChoice>({ worktree: 'auto', deleteBranch: false })
  const files = Object.entries(p.changedFiles)
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

  const overview = (
    <div className="flex flex-col gap-3" data-testid="info-overview">
      <Card title={t('info.session')}>
        <IconRow icon={Bot}>{p.agentLabel}</IconRow>
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

      <Card>
        <ValueRow label={t('info.turns')}>{turns}</ValueRow>
        {input + output > 0 ? (
          <ValueRow label={t('info.tokens')}>
            {t('usage.input', { n: compactNumber(input) })} / {t('usage.output', { n: compactNumber(output) })}
          </ValueRow>
        ) : null}
        {cost != null ? <ValueRow label={t('info.cost')}>{formatCost(cost)}</ValueRow> : null}
      </Card>

      <Card title={t('info.options')}>
        <div className="pt-1">{p.settingsControls}</div>
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

      <Card title={t('info.changedFiles')} titleAside={files.length > 0 ? String(files.length) : undefined} testId="info-files">
        {files.length === 0 ? (
          <p className="py-1 text-sm text-fg-subtle">{t('info.noFiles')}</p>
        ) : (
          <ul className="space-y-0.5 pt-1">
            {files.map(([path, kind]) => (
              <li key={path} className="flex items-center gap-2.5 py-1 text-sm" data-testid="changed-file">
                <FileDiff className="size-4 shrink-0 text-fg-muted" aria-hidden />
                <span className="min-w-0 flex-1 truncate font-mono text-xs" title={path}>
                  {path}
                </span>
                <Chip tone={KIND_TONE[kind]}>{t(`info.file.${kind}`)}</Chip>
              </li>
            ))}
          </ul>
        )}
      </Card>

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

/** 名称在左、数值在右的一行（用量卡片）。 */
function ValueRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3 py-1.5 text-sm">
      <span className="shrink-0 font-medium">{label}</span>
      <span className="min-w-0 truncate text-right text-fg-subtle">{children}</span>
    </div>
  )
}
