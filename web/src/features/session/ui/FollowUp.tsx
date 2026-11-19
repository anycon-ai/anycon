import { Flag } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'

/** 会话页顶栏的「待处理」开关：空心 = 没标，实心 = 已标。 */
export function FollowUpToggle({ on, onToggle, disabled }: { on: boolean; onToggle: () => void; disabled?: boolean }) {
  const { t } = useTranslation()
  const label = on ? t('followUp.unmark') : t('followUp.mark')
  return (
    <Button variant="ghost" size="icon" aria-label={label} title={label} aria-pressed={on} disabled={disabled} onClick={onToggle} className={cn(on && 'text-info')} data-testid="follow-up-toggle">
      <Flag className={cn('size-4', on && 'fill-current')} aria-hidden />
    </Button>
  )
}

/** 输入框上方靠右的小开关：发一个要等很久的任务时顺手标上。已经标了的由 `FollowUpBar` 负责。 */
export function FollowUpChip({ onMark, disabled }: { onMark: () => void; disabled?: boolean }) {
  const { t } = useTranslation()
  return (
    <button
      type="button"
      onClick={onMark}
      disabled={disabled}
      title={t('followUp.markHint')}
      className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs text-fg-subtle hover:bg-bg-hover hover:text-fg disabled:opacity-50"
      data-testid="follow-up-chip"
    >
      <Flag className="size-3" aria-hidden />
      {t('followUp.remindWhenDone')}
    </button>
  )
}

/**
 * 标了「待处理」的会话，输入区上方的一条：还在跑时说明「完成后会提醒你」，已经停下来时说明「在等你处理」，
 * 后者的按钮是「已处理」（清掉标记）。打开会话不会清掉标记，只有这里（或顶栏的开关）会。
 */
export function FollowUpBar({ ready, onClear, busy, disabled }: { ready: boolean; onClear: () => void; busy?: boolean; disabled?: boolean }) {
  const { t } = useTranslation()
  return (
    <div role="status" className="flex shrink-0 items-center gap-2 border-t border-info/30 bg-info-subtle px-3 py-2 text-sm text-info md:px-4" data-testid="follow-up-bar" data-ready={ready}>
      <Flag className="size-4 shrink-0 fill-current" aria-hidden />
      <span className="min-w-0 flex-1 truncate">{ready ? t('followUp.barReady') : t('followUp.barArmed')}</span>
      <Button size="sm" variant="outline" loading={busy} disabled={disabled} onClick={onClear} data-testid="follow-up-clear">
        {ready ? t('followUp.done') : t('followUp.cancel')}
      </Button>
    </div>
  )
}
