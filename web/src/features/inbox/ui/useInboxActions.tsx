// 行上的「已处理」「归档」「查看」：复用会话页的动作（`setFollowUp`、`archiveSession`），不另写一套。
// 有 worktree 的会话归档前要先问 worktree 怎么处理（和会话页的确认框同一个）。
import { useMemo, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { archiveSession, setFollowUp } from '@/features/session/actions'
import { describeError } from '@/lib/errors'
import { ArchiveWorktreeDialog, effectiveChoice, type DisposeInfo } from '@/features/worktree/DisposeDialog'
import { describeArchiveError } from '@/features/worktree/errors'
import { useEnroll } from '@/stores/enroll'
import { useGit } from '@/stores/git'
import type { SessionItem } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import type { InboxActions } from './InboxItems'

/** 目录已经没有了（removed）或根本没建成（failed）的 worktree 没什么可处理的：归档不弹框。 */
function disposeInfo(s: SessionItem): DisposeInfo | null {
  const wt = s.worktree
  if (!wt || wt.state === 'removed' || wt.state === 'failed') return null
  const git = useGit.getState().bySession[s.id]?.status
  return { branch: wt.branch, uncommitted: git?.uncommitted ?? 0, ahead: git?.ahead ?? 0, unpushed: git?.unpushed ?? 0 }
}

export function useInboxActions(): { actions: InboxActions; dialogs: ReactNode } {
  const { t } = useTranslation()
  const [archiving, setArchiving] = useState<{ session: SessionItem; info: DisposeInfo } | null>(null)
  const [busy, setBusy] = useState(false)

  const actions = useMemo<InboxActions>(
    () => ({
      markDone: (s) => {
        // 先改本地（条目马上消失、焦点落到下一条），请求失败 `setFollowUp` 会改回去，这里只提示
        setFollowUp(s.node_id, s.id, false).catch((e: unknown) => toast.error(t('inbox.markDoneFailed', { error: describeError(e, t) })))
      },
      archive: (s) => {
        const info = disposeInfo(s)
        if (info) {
          setArchiving({ session: s, info })
          return
        }
        archiveSession(s.node_id, s.id, true).catch((e: unknown) => toast.error(describeArchiveError(e, t)))
      },
      reviewEnroll: (e) => useEnroll.getState().openDialog(e.nodeId, e.info.enroll_id),
    }),
    [t],
  )

  const dialogs = archiving ? (
    <ArchiveWorktreeDialog
      open
      onOpenChange={(open) => {
        if (!open) setArchiving(null)
      }}
      info={archiving.info}
      busy={busy}
      onConfirm={(choice) => {
        const { session, info } = archiving
        setBusy(true)
        archiveSession(session.node_id, session.id, true, { worktree: effectiveChoice(choice, info).worktree })
          .then(() => setArchiving(null))
          .catch((e: unknown) => toast.error(describeArchiveError(e, t)))
          .finally(() => setBusy(false))
      }}
    />
  ) : null

  return { actions, dialogs }
}
