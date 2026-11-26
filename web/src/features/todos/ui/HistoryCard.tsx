// 「经过」时间线的一格：左边是轨道上的图标，右边一张卡片——标题和时间一行，下面是内容，最底下是链接。
// 清单的事件（`TodoHistory`）和 Bot 的事件（`TwinHistoryEvent`）都用它，所以每一条都是分开的一块，不挤成一段。
import type { ReactNode } from 'react'
import type { LucideIcon } from 'lucide-react'
import { cn } from '@/ui/cn'

export type HistoryTone = 'neutral' | 'accent' | 'success' | 'warning'

const iconTone: Record<HistoryTone, string> = {
  neutral: 'text-fg-muted',
  accent: 'border-accent/40 text-accent-text',
  success: 'border-success/40 text-success',
  warning: 'border-warning/50 text-warning',
}

/** 时间线：竖线画在 `<ol>` 上，图标的白底把它盖住。 */
export const historyListClass = 'relative space-y-3 text-sm before:absolute before:top-3 before:bottom-3 before:left-[0.8125rem] before:w-px before:bg-border'

/** 一条事件的外壳：`<li>` 自己由调用方给（带 testid），这里画图标和卡片。 */
export function HistoryCard({ icon: Icon, tone = 'neutral', title, time, note, actions, children, testId }: { icon: LucideIcon; tone?: HistoryTone; title: ReactNode; time?: string; note?: ReactNode; actions?: ReactNode; children?: ReactNode; testId?: string }) {
  return (
    <div className="relative pl-10" data-testid={testId}>
      <span aria-hidden className={cn('absolute top-2 left-0 flex size-7 items-center justify-center rounded-full border border-border bg-surface', iconTone[tone])}>
        <Icon className="size-3.5" />
      </span>
      <div className="min-w-0 space-y-2 rounded-lg border border-border bg-surface px-3.5 py-2.5">
        <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
          <p className="min-w-0 font-medium break-words">{title}</p>
          {time ? <span className="shrink-0 text-xs text-fg-subtle tabular-nums">{time}</span> : null}
        </div>
        {note ? <p className="text-xs text-fg-subtle">{note}</p> : null}
        {children}
        {actions ? <div className="flex flex-wrap items-center gap-x-4 gap-y-1">{actions}</div> : null}
      </div>
    </div>
  )
}

/** agent 写的一段报告：在卡片里换一层浅底，太长就在框里滚动。 */
export const historyQuoteClass = 'scroll-thin max-h-64 overflow-auto rounded-md bg-bg-subtle px-3 py-2'
