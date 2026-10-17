import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { WorktreeDisposal } from '@/protocol'
import { Button } from '@/ui/Button'
import { CheckboxField, RadioChoices } from '@/ui/Choice'
import { Dialog } from '@/ui/Overlay'

export interface DisposeChoice {
  worktree: WorktreeDisposal
  /** 只有删除会话时有意义；worktree 被保留时不会生效。 */
  deleteBranch: boolean
}

export interface DisposeInfo {
  branch: string
  /** `git_status.uncommitted`（还不知道时 0）。 */
  uncommitted: number
  /** 分支上自己的提交数（`git_status.ahead`）。 */
  ahead: number
  /** 没推送的提交数（`git_status.unpushed`）。 */
  unpushed: number
}

/** 实际发给 daemon 的选择：worktree 会被保留时，「同时删除分支」不生效（也不发）。 */
export function effectiveChoice(choice: DisposeChoice, info: DisposeInfo): DisposeChoice {
  const blocked = choice.worktree === 'keep' || (choice.worktree === 'auto' && info.uncommitted > 0)
  return { worktree: choice.worktree, deleteBranch: choice.deleteBranch && !blocked }
}

/** 归档 / 删除时怎么处理 worktree：自动 / 保留 / 删除；删除会话时再多一项「同时删除分支」。 */
export function WorktreeDisposePicker({ idPrefix, info, value, onChange, withBranch }: { idPrefix: string; info: DisposeInfo; value: DisposeChoice; onChange: (v: DisposeChoice) => void; withBranch: boolean }) {
  const { t } = useTranslation()
  const lost = Math.max(info.ahead, info.unpushed)
  // 保留 worktree 时分支留着；自动处理而又有未提交改动时 worktree 会被保留，分支同样删不掉
  const branchBlocked = value.worktree === 'keep' || (value.worktree === 'auto' && info.uncommitted > 0)
  return (
    <div className="space-y-3" data-testid={`${idPrefix}-picker`}>
      <RadioChoices
        name={`${idPrefix}-worktree`}
        ariaLabel={t('worktree.dispose.label')}
        value={value.worktree}
        onValueChange={(worktree) => onChange({ ...value, worktree })}
        testId={`${idPrefix}-worktree`}
        options={[
          {
            value: 'auto',
            label: t('worktree.dispose.auto'),
            hint: (
              <>
                {t('worktree.dispose.autoHint')}
                {info.uncommitted > 0 ? (
                  <span className="mt-0.5 block font-medium text-warning" data-testid={`${idPrefix}-auto-keeps`}>
                    {t('worktree.dispose.autoKeeps', { count: info.uncommitted })}
                  </span>
                ) : null}
              </>
            ),
            testId: `${idPrefix}-option-auto`,
          },
          { value: 'keep', label: t('worktree.dispose.keep'), hint: t('worktree.dispose.keepHint'), testId: `${idPrefix}-option-keep` },
          {
            value: 'remove',
            label: info.uncommitted > 0 ? t('worktree.dispose.removeDiscard', { count: info.uncommitted }) : t('worktree.dispose.remove'),
            hint: t('worktree.dispose.removeHint'),
            testId: `${idPrefix}-option-remove`,
          },
        ]}
      />
      {withBranch ? (
        <div>
          <CheckboxField
            id={`${idPrefix}-delete-branch`}
            checked={value.deleteBranch && !branchBlocked}
            onCheckedChange={(deleteBranch) => onChange({ ...value, deleteBranch })}
            disabled={branchBlocked}
            testId={`${idPrefix}-delete-branch`}
            label={t('worktree.dispose.deleteBranch', { branch: info.branch })}
            hint={branchBlocked ? t('worktree.dispose.branchKept') : undefined}
          />
          {lost > 0 && !branchBlocked ? (
            <p className={value.deleteBranch ? 'ml-7 text-xs font-medium text-danger' : 'ml-7 text-xs text-warning'} role={value.deleteBranch ? 'alert' : undefined} data-testid={`${idPrefix}-commits-lost`}>
              {t('worktree.dispose.commitsLost', { count: lost })}
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}

/** 有 worktree 的会话归档前的确认框。 */
export function ArchiveWorktreeDialog({ open, onOpenChange, info, busy, onConfirm }: { open: boolean; onOpenChange: (open: boolean) => void; info: DisposeInfo; busy?: boolean; onConfirm: (choice: DisposeChoice) => void }) {
  const { t } = useTranslation()
  const [choice, setChoice] = useState<DisposeChoice>({ worktree: 'auto', deleteBranch: false })
  useEffect(() => {
    if (open) setChoice({ worktree: 'auto', deleteBranch: false })
  }, [open])
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('worktree.archiveTitle')} description={t('worktree.archiveBody', { branch: info.branch })} closeLabel={t('common.close')} testId="archive-worktree-dialog">
      <div className="space-y-4">
        <WorktreeDisposePicker idPrefix="archive" info={info} value={choice} onChange={setChoice} withBranch={false} />
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t('common.cancel')}
          </Button>
          <Button variant={choice.worktree === 'remove' && info.uncommitted > 0 ? 'danger' : 'primary'} loading={busy} onClick={() => onConfirm(choice)} data-testid="archive-confirm">
            {t('info.archive')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
