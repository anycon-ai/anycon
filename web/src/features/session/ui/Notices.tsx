// `notice`（agent 给用户的提示，不是错误）和 `activity`（turn 进行中的临时状态）的界面，spec/web.md § 4.6。
import { useState } from 'react'
import { AlertTriangle, ChevronRight, Info, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useNow } from '@/lib/useNow'
import { cn } from '@/ui/cn'
import { noticeText } from '../notice'
import type { ActivityInfo, NoticeBlock } from '../reducer'

/** `command_output`（`/context`、`/cost` 这类本地命令的输出）：等宽的块，可折叠。 */
function CommandOutput({ block }: { block: NoticeBlock }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(true)
  const text = block.message.replace(/\s+$/, '')
  return (
    <div className="text-sm" data-testid="notice-command-output" data-code={block.code} data-level={block.level}>
      <button type="button" className="inline-flex max-w-full items-center gap-1.5 py-0.5 text-left text-fg-subtle hover:text-fg-muted disabled:hover:text-fg-subtle" aria-expanded={open} onClick={() => setOpen((v) => !v)} disabled={!text}>
        <span className="truncate">{t('notice.commandOutput')}</span>
        {text ? <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden /> : null}
      </button>
      {open && text ? (
        <pre className="scroll-thin mt-1 max-h-96 overflow-auto rounded-lg border border-border bg-bg px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap [overflow-wrap:anywhere]" data-testid="notice-output">
          {text}
        </pre>
      ) : null}
    </div>
  )
}

/**
 * 消息流里的一行提示：`info` 灰字，`warning` 黄字。认识的 code 用界面语言的文案，agent 的原文跟在后面（点开看全文）；
 * 不认识的 code 直接显示原文。不画红块、没有「重试」——这不是错误。
 */
export function NoticeLine({ block, modelLabel }: { block: NoticeBlock; modelLabel: (id: string) => string }) {
  const { t, i18n } = useTranslation()
  const [open, setOpen] = useState(false)
  if (block.code === 'command_output') return <CommandOutput block={block} />
  const { summary, detail } = noticeText(t, i18n.language, block, modelLabel)
  const warning = block.level === 'warning'
  const Icon = warning ? AlertTriangle : Info
  // 原文的第一行跟在文案后面，让「为什么」不用点开也看得到；多行的点开看全文
  const first = detail?.split('\n', 1)[0] ?? ''
  const line = (
    <>
      <Icon className="mt-0.5 size-3.5 shrink-0 self-start" aria-hidden />
      <span className="min-w-0 truncate">
        {summary}
        {first ? <span className="opacity-70"> — {first}</span> : null}
      </span>
      {detail ? <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden /> : null}
    </>
  )
  const tone = warning ? 'text-warning' : 'text-fg-subtle'
  return (
    <div className={cn('text-sm', tone)} role="status" data-testid="notice" data-code={block.code} data-level={block.level}>
      {detail ? (
        <button type="button" className="flex max-w-full items-center gap-1.5 py-0.5 text-left hover:opacity-80" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
          {line}
        </button>
      ) : (
        <p className="flex max-w-full items-center gap-1.5 py-0.5">{line}</p>
      )}
      {open && detail ? (
        <p className="ml-1.5 mt-1 max-h-60 overflow-auto border-l-2 border-border pl-3 text-xs whitespace-pre-wrap [overflow-wrap:anywhere] opacity-90" data-testid="notice-detail">
          {detail}
        </p>
      ) : null}
    </div>
  )
}

/** 重试还剩几秒（从 `activity` 事件的时间起算）。没有倒计时（没报 `delay_ms`）或已经到点是 null。 */
export function retryCountdown(a: Pick<ActivityInfo, 'state' | 'delayMs' | 'ts'>, now: number): number | null {
  if (a.state !== 'retrying' || !a.delayMs || a.delayMs <= 0) return null
  const left = Math.ceil((a.ts + a.delayMs - now) / 1000)
  // 两台机器的时钟对不上时别显示离谱的数：最多是 daemon 报的那个延迟
  return left > 0 ? Math.min(left, Math.ceil(a.delayMs / 1000)) : null
}

/**
 * 消息流最底下的一行临时状态（turn 进行中）：「正在重试（2/10），5 秒后…」（秒数实时倒数）、「正在压缩上下文…」。
 * 读屏只读不带秒数的那句，免得每秒播一次。
 */
export function ActivityLine({ activity }: { activity: ActivityInfo }) {
  const { t } = useTranslation()
  const retrying = activity.state === 'retrying'
  const now = useNow(1000, retrying && !!activity.delayMs)
  const seconds = retryCountdown(activity, now)
  const counted = activity.attempt != null && activity.maxAttempts != null
  const params = { attempt: activity.attempt, max: activity.maxAttempts, seconds }
  const plain = retrying ? t(counted ? 'activity.retryingAttempt' : 'activity.retrying', params) : t('activity.compacting')
  const shown = retrying && seconds !== null ? t(counted ? 'activity.retryingAttemptIn' : 'activity.retryingIn', params) : plain
  return (
    <div className="flex min-w-0 items-center gap-2 py-0.5 text-sm text-fg-subtle" role="status" data-testid="activity" data-state={activity.state}>
      <Loader2 className="size-3.5 shrink-0 animate-spin" aria-hidden />
      <span aria-hidden className="shrink-0">
        {shown}
      </span>
      <span className="sr-only">{plain}</span>
      {retrying && activity.reason ? (
        <span className="min-w-0 truncate text-xs opacity-70" title={activity.reason} data-testid="activity-reason">
          {activity.reason}
        </span>
      ) : null}
    </div>
  )
}
