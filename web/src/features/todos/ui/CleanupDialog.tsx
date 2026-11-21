import { useEffect, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { refreshGitStatus } from '@/features/worktree/git'
import { describeGitError } from '@/features/worktree/errors'
import { cleanHintText } from '@/lib/hint'
import { useGit, useGitEntry } from '@/stores/git'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { CheckboxField, RadioChoices } from '@/ui/Choice'
import { Dialog } from '@/ui/Overlay'
import { SAFE_CLEANUP, cleanUpWorktree, type CleanupChoice } from '../review'

type Kind = CleanupChoice['kind']
interface RowChoice {
  kind: Kind
  deleteBranch: boolean
  force: boolean
}
const DEFAULT_ROW: RowChoice = { kind: SAFE_CLEANUP.kind, deleteBranch: false, force: false }

const toChoice = (c: RowChoice): CleanupChoice => (c.kind === 'remove' ? { kind: 'remove', deleteBranch: c.deleteBranch, force: c.force } : { kind: c.kind })

/**
 * 「不要了」之后：这一项派出的每个会话的 worktree（和分支）怎么处理（spec/web.md § 4.12）。**会话和它的记录都留着**，这里只处理 worktree 和分支。
 * 每个 worktree 单独选：缺省是「自动」（没有任何东西会丢才删，有未提交的改动就留着）；「删除」要等读到 git 状态之后才能选，
 * 有未提交的改动必须明确勾「丢弃这 N 个文件的改动」才能删；同时删分支另有一个勾选，写明分支上会丢几个提交。
 */
export function CleanupDialog({ open, onOpenChange, nodeId, sessionIds }: { open: boolean; onOpenChange: (o: boolean) => void; nodeId: string; sessionIds: readonly string[] }) {
  const { t } = useTranslation()
  const [choices, setChoices] = useState<Record<string, RowChoice>>({})
  const [busy, setBusy] = useState(false)
  // 对话框开着期间只看打开时那几个会话：清掉了的（worktree 不在了）那一行自己消失，别的会话进来不改这里的选择
  const [ids] = useState(() => [...sessionIds])
  const list = useSessions((s) => s.list)
  const sessions = useMemo(() => ids.map((id) => list[id]).filter((s): s is SessionItem => !!s && !!s.worktree && (s.worktree.state === 'active' || s.worktree.state === 'kept')), [ids, list])
  useEffect(() => {
    if (!open) return
    setChoices({})
    for (const id of ids) void refreshGitStatus(nodeId, id)
  }, [open, nodeId, ids])
  const set = (id: string, patch: Partial<RowChoice>): void => setChoices((c) => ({ ...c, [id]: { ...(c[id] ?? DEFAULT_ROW), ...patch } }))
  const bySession = useGit((s) => s.bySession)
  // 选了「删除」而 git 状态还没读到、或有未提交的改动却没勾丢弃：不能确认
  const invalid = (id: string): boolean => {
    const c = choices[id]
    if (c?.kind !== 'remove') return false
    const st = bySession[id]?.status
    return !st || (st.uncommitted > 0 && !c.force)
  }
  const confirm = async (): Promise<void> => {
    setBusy(true)
    let failed = 0
    try {
      for (const s of sessions) {
        const c = choices[s.id] ?? DEFAULT_ROW
        try {
          await cleanUpWorktree(nodeId, s, toChoice(c))
        } catch (e) {
          failed++
          toast.error(describeGitError(e, t))
        }
      }
    } finally {
      setBusy(false)
    }
    if (failed === 0) onOpenChange(false)
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)} title={t('todos.reviewPage.cleanupTitle')} description={t('todos.reviewPage.cleanupBody')} closeLabel={t('common.close')} testId="review-cleanup-dialog" size="lg">
      <div className="space-y-4">
        {sessions.length === 0 ? <p className="text-sm text-fg-muted">{t('todos.reviewPage.cleanupNone')}</p> : null}
        <ul className="space-y-3">
          {sessions.map((s) => (
            <CleanupRow key={s.id} session={s} value={choices[s.id] ?? DEFAULT_ROW} onChange={(p) => set(s.id, p)} />
          ))}
        </ul>
        <div className="flex justify-end gap-2">
          <Button onClick={() => onOpenChange(false)} disabled={busy} data-testid="review-cleanup-keep">
            {t('todos.reviewPage.cleanupKeep')}
          </Button>
          <Button variant="danger" loading={busy} disabled={sessions.length === 0 || sessions.some((s) => invalid(s.id))} onClick={() => void confirm()} data-testid="review-cleanup-confirm">
            {t('todos.reviewPage.cleanupDo')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}

function CleanupRow({ session, value, onChange }: { session: SessionItem; value: RowChoice; onChange: (p: Partial<RowChoice>) => void }) {
  const { t } = useTranslation()
  const git = useGitEntry(session.id)
  const wt = session.worktree!
  const known = git.status !== null
  const uncommitted = git.status?.uncommitted ?? 0
  const ahead = git.status?.ahead ?? 0
  const unpushed = git.status?.unpushed ?? 0
  const lost = Math.max(ahead, unpushed)
  const removing = value.kind === 'remove'
  const id = `cleanup-${session.id}`
  return (
    <li className="space-y-2 rounded-md border border-border p-3" data-testid="review-cleanup-row" data-session={session.id}>
      <div className="min-w-0">
        <p className="truncate font-mono text-xs" data-testid="review-cleanup-branch">
          {cleanHintText(wt.branch)}
        </p>
        <p className="truncate text-sm text-fg-muted">{cleanHintText(session.title)}</p>
        <p className="text-xs" data-testid="review-cleanup-status">
          {known ? (
            uncommitted === 0 && lost === 0 ? (
              <span className="text-fg-muted">{t('todos.reviewPage.cleanupClean')}</span>
            ) : (
              <>
                {uncommitted > 0 ? <span className="font-medium text-warning">{t('todos.reviewPage.cleanupUncommitted', { count: uncommitted })} </span> : null}
                {ahead > 0 ? <span className="text-warning">{t('todos.reviewPage.cleanupAhead', { count: ahead })} </span> : null}
                {unpushed > 0 ? <span className="text-warning">{t('todos.reviewPage.cleanupUnpushed', { count: unpushed })}</span> : null}
              </>
            )
          ) : git.error ? (
            <span className="text-danger">{describeGitError(git.error, t)}</span>
          ) : (
            <span className="text-fg-subtle">{t('git.loading')}</span>
          )}
        </p>
      </div>
      <RadioChoices
        name={`${id}-kind`}
        ariaLabel={t('worktree.dispose.label')}
        value={value.kind}
        onValueChange={(kind) => onChange({ kind: kind as Kind, force: false, deleteBranch: false })}
        testId={`${id}-kind`}
        options={[
          { value: 'auto', label: t('todos.reviewPage.cleanupAuto'), hint: t('todos.reviewPage.cleanupAutoHint'), testId: 'review-cleanup-auto' },
          { value: 'keep', label: t('worktree.dispose.keep'), hint: t('worktree.dispose.keepHint'), testId: 'review-cleanup-option-keep' },
          { value: 'remove', label: t('todos.reviewPage.cleanupRemove'), hint: known ? t('todos.reviewPage.cleanupRemoveHint') : t('todos.reviewPage.cleanupRemoveWait'), testId: 'review-cleanup-option-remove' },
        ]}
      />
      {removing ? (
        <div className="space-y-2 pl-7">
          {uncommitted > 0 ? (
            <CheckboxField id={`${id}-force`} checked={value.force} onCheckedChange={(force) => onChange({ force })} testId="review-cleanup-force" label={t('todos.reviewPage.cleanupForce', { count: uncommitted })} />
          ) : null}
          <CheckboxField id={`${id}-branch`} checked={value.deleteBranch} onCheckedChange={(deleteBranch) => onChange({ deleteBranch })} testId="review-cleanup-delete-branch" label={t('worktree.dispose.deleteBranch', { branch: cleanHintText(wt.branch) })} />
          {value.deleteBranch && lost > 0 ? (
            <p className="text-xs font-medium text-danger" role="alert" data-testid="review-cleanup-commits-lost">
              {t('worktree.dispose.commitsLost', { count: lost })}
            </p>
          ) : null}
        </div>
      ) : null}
    </li>
  )
}
