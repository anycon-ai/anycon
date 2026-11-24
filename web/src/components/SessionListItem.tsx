import { Link } from '@tanstack/react-router'
import { Flag, GitBranch } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { setFollowUp } from '@/features/session/actions'
import { isFollowUp, isUnread } from '@/features/session/marks'
import { describeMarkError } from '@/lib/errors'
import { formatRelative } from '@/lib/format'
import { useNodeParam } from '@/lib/nav'
import { useNodes } from '@/stores/nodes'
import type { SessionItem } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { cn } from '@/ui/cn'
import { Chip } from '@/ui/Misc'
import { SessionDot, sessionTone, UnreadDot } from './StatusDot'

export function SessionStatusChip({ status, testId, dotOnMobile }: { status: SessionItem['status']; testId?: string; dotOnMobile?: boolean }) {
  const { t } = useTranslation()
  return (
    <Chip tone={sessionTone(status)} testId={testId} dotOnMobile={dotOnMobile} pulse={status === 'running' || status === 'starting'} title={t(`status.session.${status}`)}>
      {t(`status.session.${status}`)}
    </Chip>
  )
}

/**
 * 会话列表的一行：标题、agent / 模型、最后一条消息摘要、时间、状态；标了「完成后提醒」的带一个 chip，
 * 停下来之后还没看的带未读小红点。最右边是「完成后提醒」开关（不放进链接里：按钮不能嵌在 `<a>` 里）。
 */
export function SessionListItem({ session, showProject }: { session: SessionItem; showProject?: boolean }) {
  const { t, i18n } = useTranslation()
  const nodeParam = useNodeParam(session.node_id)
  const offline = useNodes((n) => n.byId[session.node_id]?.conn !== 'ready')
  const marked = isFollowUp(session)
  const toggleLabel = marked ? t('followUp.unmark') : t('followUp.mark')
  return (
    <div className="flex items-stretch rounded-lg border border-border bg-surface hover:bg-bg-hover" data-follow-up={marked ? 'true' : undefined}>
      <Link
        to="/n/$node/s/$session"
        params={{ node: nodeParam, session: session.id }}
        className="flex min-w-0 flex-1 items-start gap-3 py-2.5 pl-3"
        data-testid="session-list-item"
      >
        <span className="mt-1.5">
          <SessionDot status={session.status} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2">
            <span className={cn('truncate', isUnread(session) ? 'font-semibold' : 'font-medium')}>{session.title || session.id}</span>
            {isUnread(session) ? <UnreadDot /> : null}
            {marked ? (
              <Chip tone="info" icon={<Flag className="size-3" aria-hidden />} testId="follow-up-chip-row">
                {t('followUp.chip')}
              </Chip>
            ) : null}
            {session.pending_permissions > 0 ? <Chip tone="warning">{t('session.pendingCount', { count: session.pending_permissions })}</Chip> : null}
            {session.archived ? <Chip>{t('session.archived')}</Chip> : null}
          </span>
          <span className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-fg-subtle">
            <span>{session.agent}</span>
            {session.settings.model ? <span>· {session.settings.model}</span> : null}
            {showProject && session.project ? <span>· {session.project}</span> : null}
            {session.worktree ? (
              <span className="inline-flex min-w-0 items-center gap-0.5 font-mono" data-testid="session-list-branch" title={session.worktree.branch}>
                · <GitBranch className="size-3 shrink-0" aria-hidden />
                <span className="truncate">{session.worktree.branch}</span>
              </span>
            ) : null}
            <span>· {formatRelative(session.updated_at_ms, i18n.language)}</span>
          </span>
          {session.preview ? <span className="mt-1 line-clamp-1 block text-sm text-fg-muted">{session.preview}</span> : null}
        </span>
        <SessionStatusChip status={session.status} />
      </Link>
      <button
        type="button"
        aria-label={toggleLabel}
        title={toggleLabel}
        aria-pressed={marked}
        disabled={offline}
        onClick={() => void setFollowUp(session.node_id, session.id, !marked).catch((e: unknown) => toast.error(describeMarkError(e, t)))}
        className={cn('shrink-0 self-start rounded-md p-2 pr-3 hover:text-fg disabled:opacity-40', marked ? 'text-info' : 'text-fg-subtle/60')}
        data-testid="session-follow-up-toggle"
      >
        <Flag className={cn('size-4', marked && 'fill-current')} aria-hidden />
      </button>
    </div>
  )
}
