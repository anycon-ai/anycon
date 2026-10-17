import type { ReactNode } from 'react'
import { AlertTriangle, GitBranch, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { SessionGitStatus, SessionWorktree } from '@/protocol'
import { cn } from '@/ui/cn'
import { tones, type Tone } from '@/ui/Misc'

/**
 * 会话页标题行里的分支 chip（web.md § 4.6）：`⑂ anycon/fix-login ← main  ↑3 ↓1  · 5 个未提交`。
 * 数据来自 `SessionSummary.worktree`（状态）和 `session.git_status`（计数）；没有的部分不显示。
 * 在项目目录本身工作的 git 会话只显示分支名和未提交数，不显示 base。点击切到主区的「改动」视图。
 * 状态不只靠颜色：`creating` 带转圈和文字，`missing` / `failed` 带图标和文字。
 */
export function BranchChip({ worktree, status, onOpen, className }: { worktree?: SessionWorktree | null; status?: SessionGitStatus | null; onOpen: () => void; className?: string }) {
  const { t } = useTranslation()
  const state = worktree?.state

  let tone: Tone = 'neutral'
  let body: ReactNode
  let label: string
  if (state === 'creating') {
    tone = 'info'
    label = t('git.creating')
    body = (
      <>
        <Loader2 className="size-3 shrink-0 animate-spin" aria-hidden />
        <span className="truncate">{label}</span>
      </>
    )
  } else if (state === 'missing' || state === 'failed') {
    tone = state === 'missing' ? 'warning' : 'danger'
    label = state === 'missing' ? t('git.worktreeMissing') : t('git.worktreeFailed')
    body = (
      <>
        <AlertTriangle className="size-3 shrink-0" aria-hidden />
        <span className="truncate">{label}</span>
      </>
    )
  } else {
    const branch = worktree?.branch ?? status?.branch ?? (status ? t('git.detached', { sha: status.head_sha.slice(0, 7) }) : null)
    if (!branch) return null
    const base = worktree?.base
    const ahead = status?.ahead ?? 0
    const behind = status?.behind ?? 0
    const uncommitted = status?.uncommitted ?? 0
    const merging = !!status?.merging || (status?.conflicts ?? 0) > 0
    if (merging) tone = 'warning'
    const parts: string[] = []
    if (base) parts.push(t('git.chipBase', { base }))
    if (ahead > 0) parts.push(t('git.chipAhead', { count: ahead }))
    if (behind > 0) parts.push(t('git.chipBehind', { count: behind }))
    if (merging) parts.push(t('git.chipMerging'))
    if (uncommitted > 0) parts.push(t('git.chipUncommitted', { count: uncommitted }))
    if (state === 'kept' || state === 'removed') parts.push(t(`worktree.state.${state}`))
    label = [branch, ...parts].join(' ')
    body = (
      <>
        {merging ? <AlertTriangle className="size-3 shrink-0" aria-hidden /> : <GitBranch className="size-3 shrink-0" aria-hidden />}
        <span className="min-w-0 truncate font-mono" data-testid="session-branch-name">
          {branch}
        </span>
        {base ? (
          <span className="shrink-0 font-mono text-fg-muted" data-testid="session-branch-base">
            ← {base}
          </span>
        ) : null}
        {ahead > 0 ? (
          <span className="shrink-0" data-testid="session-branch-ahead" title={t('git.aheadTitle', { count: ahead })}>
            ↑{ahead}
          </span>
        ) : null}
        {behind > 0 ? (
          <span className="shrink-0" data-testid="session-branch-behind" title={t('git.behindTitle', { count: behind })}>
            ↓{behind}
          </span>
        ) : null}
        {merging ? <span className="shrink-0">{t('git.chipMerging')}</span> : null}
        {uncommitted > 0 ? (
          <span className="shrink-0" data-testid="session-branch-uncommitted">
            · {t('git.chipUncommitted', { count: uncommitted })}
          </span>
        ) : null}
        {state === 'kept' || state === 'removed' ? <span className="shrink-0">· {t(`worktree.state.${state}`)}</span> : null}
      </>
    )
  }

  return (
    // 按钮本身 ≥ 36px 高，里面的胶囊保持 chip 的大小
    <button
      type="button"
      onClick={onOpen}
      aria-label={t('git.openChanges', { label })}
      title={label}
      data-testid="session-branch"
      data-state={state ?? 'local'}
      className={cn('inline-flex min-h-9 max-w-[min(100%,28rem)] min-w-0 shrink items-center rounded-md px-0.5 hover:opacity-80 focus-visible:ring-2 focus-visible:ring-accent/40', className)}
    >
      <span className={cn('inline-flex min-w-0 max-w-full items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium whitespace-nowrap', tones[tone])}>{body}</span>
    </button>
  )
}
