import { Flag, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'

/** 会话页顶栏的「完成后提醒」开关：空心 = 没标，实心 = 已标。 */
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
 * 标了「完成后提醒」的会话，输入区上方的轻量提示（和后台任务 chip 同一行）：实心旗帜 + 一句说明 + 叉号。会话在跑（或还有后台任务）时说
 * 「结束后会通知你」，停着时说「下一轮结束后会通知你」。点旗帜或叉号都是取消提醒。没有「就绪」态：完成之后 daemon
 * 提醒一次并清掉标记，这一行随之消失。
 */
export function FollowUpBar({ running, onClear, busy, disabled }: { running: boolean; onClear: () => void; busy?: boolean; disabled?: boolean }) {
  const { t } = useTranslation()
  const cancel = t('followUp.cancel')
  const off = disabled || busy
  return (
    <div className="inline-flex min-w-0 max-w-full items-center gap-1 text-xs text-info" data-testid="follow-up-bar" data-running={running}>
        <button type="button" onClick={onClear} disabled={off} title={cancel} aria-label={cancel} className="shrink-0 rounded p-0.5 hover:bg-info-subtle disabled:opacity-50">
          <Flag className="size-3.5 fill-current" aria-hidden />
        </button>
        <span role="status" className="min-w-0 truncate">
          {running ? t('followUp.barRunning') : t('followUp.barIdle')}
        </span>
        <button
          type="button"
          onClick={onClear}
          disabled={off}
          title={cancel}
          aria-label={cancel}
          className="shrink-0 rounded p-0.5 text-fg-subtle hover:bg-bg-hover hover:text-fg disabled:opacity-50"
          data-testid="follow-up-clear"
        >
          <X className="size-3.5" aria-hidden />
        </button>
    </div>
  )
}
