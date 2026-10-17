// `git` 事件（daemon 自己做的 git 操作，ADR-34）→ 一行系统提示的文字。纯函数，走 i18n。
import type { TFunction } from 'i18next'
import type { GitSystemBlock } from '@/features/session/reducer'

/** 冲突文件在提示里最多列几个，其余用「等 N 个」带过。 */
const MAX_FILES_SHOWN = 4

/** `worktree_removed` 的 `note` 里约定的两个值（docs/protocol.md § 6 / docs/spec/daemon.md § 10.1）：其它值只说「已删除」。 */
export const NOTE_BRANCH_DELETED = 'branch_deleted'
export const NOTE_BRANCH_KEPT = 'branch_kept'

export function gitEventText(t: TFunction, b: GitSystemBlock): string {
  const base = b.base ?? t('git.event.unknownBase')
  switch (b.action) {
    case 'worktree_created':
      return b.count && b.count > 0 ? t('git.event.worktree_created_files', { branch: b.branch, base, count: b.count }) : t('git.event.worktree_created', { branch: b.branch, base })
    case 'worktree_failed':
      return b.note ? t('git.event.worktree_failed', { branch: b.branch, note: b.note }) : t('git.event.worktree_failed_plain', { branch: b.branch })
    case 'worktree_removed':
      if (b.note === NOTE_BRANCH_DELETED) return t('git.event.worktree_removed_branch_deleted', { branch: b.branch })
      if (b.note === NOTE_BRANCH_KEPT) return t('git.event.worktree_removed_branch_kept', { branch: b.branch })
      return t('git.event.worktree_removed', { branch: b.branch })
    case 'worktree_kept':
      return t('git.event.worktree_kept', { branch: b.branch })
    case 'worktree_restored':
      return t('git.event.worktree_restored', { branch: b.branch })
    case 'worktree_missing':
      return t('git.event.worktree_missing', { branch: b.branch })
    case 'sync_up_to_date':
      return t('git.event.sync_up_to_date', { base })
    case 'sync_merged':
      return t('git.event.sync_merged', { base, count: b.count ?? 0 })
    case 'sync_conflicts': {
      const shown = b.files.slice(0, MAX_FILES_SHOWN)
      const rest = b.files.length - shown.length
      const sep = t('git.listSeparator')
      const files = rest > 0 ? `${shown.join(sep)} ${t('git.event.andMore', { count: rest })}` : shown.join(sep)
      return t('git.event.sync_conflicts', { base, count: b.files.length, files })
    }
    case 'sync_aborted':
      return t('git.event.sync_aborted', { base })
  }
}
