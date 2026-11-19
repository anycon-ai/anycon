import { useTranslation } from 'react-i18next'
import { cn } from '@/ui/cn'
import { Dot } from '@/ui/Misc'
import type { SessionStatus } from '@/protocol'

export function sessionTone(status: SessionStatus) {
  switch (status) {
    case 'running':
    case 'starting':
      return 'accent' as const
    case 'waiting_permission':
      return 'warning' as const
    case 'error':
      return 'danger' as const
    case 'queued':
    case 'interrupted':
      return 'warning' as const
    default:
      return 'neutral' as const
  }
}

export function SessionDot({ status }: { status: SessionStatus }) {
  return <Dot tone={sessionTone(status)} pulse={status === 'running' || status === 'starting'} />
}

/** 未读小红点：会话停下来之后还有你没看过的输出（features/session/marks.ts 的 `isUnread`）。 */
export function UnreadDot({ className }: { className?: string }) {
  const { t } = useTranslation()
  return (
    <span role="img" aria-label={t('session.unread')} title={t('session.unread')} className={cn('inline-block size-2 shrink-0 rounded-full bg-danger', className)} data-testid="unread-dot" />
  )
}
