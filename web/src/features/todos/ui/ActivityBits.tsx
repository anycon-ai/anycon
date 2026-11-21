import { Bot, User } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { SessionDot } from '@/components/StatusDot'
import { useSessions } from '@/stores/sessions'
import { Chip } from '@/ui/Misc'
import { activityOf, handlerOf, listGroupOf, wasDispatched } from '../activity'
import type { ViewTodo } from '../types'

/**
 * 谁在处理（我 / agent）：只在有东西在动的行上显示（等你决定、待验收、处理中）；还没开始和已关闭的行不显示。
 * `activity` 是 daemon 算的，这里只读它。
 */
export function HandlerChip({ item }: { item: Pick<ViewTodo, 'activity' | 'state'> }) {
  const { t } = useTranslation()
  const group = listGroupOf(item)
  if (group === 'open' || group === 'closed') return null
  const who = handlerOf(item)
  return (
    <Chip tone={who === 'agent' ? 'info' : 'neutral'} icon={who === 'agent' ? <Bot className="size-3" aria-hidden /> : <User className="size-3" aria-hidden />} testId="todo-handler" dataState={who}>
      {t(`todos.handler.${who}`)}
    </Chip>
  )
}

/**
 * 关联会话的实时状态点：取最近派发的那个还存在的会话（和 daemon 算 `activity` 时「说了算」的是同一个）。
 * 会话都不在了（被删了）就什么都不画。点旁边有文字（读屏用），不只靠颜色。
 */
export function LinkedStatus({ item }: { item: Pick<ViewTodo, 'sessions' | 'state'> }) {
  const { t } = useTranslation()
  const status = useSessions((s) => {
    const ids = item.sessions ?? []
    for (let i = ids.length - 1; i >= 0; i--) {
      const sess = s.list[ids[i]!]
      if (sess) return sess.status
    }
    return undefined
  })
  if (!wasDispatched(item) || status === undefined) return null
  return (
    <span className="inline-flex shrink-0 items-center gap-1 text-xs text-fg-muted" data-testid="todo-session-dot" data-status={status} title={t(`status.session.${status}`)}>
      <SessionDot status={status} />
      <span className="sr-only">{t(`status.session.${status}`)}</span>
    </span>
  )
}

/** 这一项现在的 `activity`（已知的三种之一）的文字，给详情和验收页的标签用。 */
export function activityLabelKey(item: Pick<ViewTodo, 'activity' | 'state'>): string | null {
  const a = activityOf(item)
  return a ? `todos.activity.${a}` : null
}
