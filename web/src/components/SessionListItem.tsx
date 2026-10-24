import { Link } from '@tanstack/react-router'
import { GitBranch } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { formatRelative } from '@/lib/format'
import { useNodeParam } from '@/lib/nav'
import type { SessionItem } from '@/stores/sessions'
import { Chip } from '@/ui/Misc'
import { SessionDot, sessionTone } from './StatusDot'

export function SessionStatusChip({ status, testId, dotOnMobile }: { status: SessionItem['status']; testId?: string; dotOnMobile?: boolean }) {
  const { t } = useTranslation()
  return (
    <Chip tone={sessionTone(status)} testId={testId} dotOnMobile={dotOnMobile} pulse={status === 'running' || status === 'starting'} title={t(`status.session.${status}`)}>
      {t(`status.session.${status}`)}
    </Chip>
  )
}

/** 会话列表的一行：标题、agent / 模型、最后一条消息摘要、时间、状态。 */
export function SessionListItem({ session, showProject }: { session: SessionItem; showProject?: boolean }) {
  const { t, i18n } = useTranslation()
  const nodeParam = useNodeParam(session.node_id)
  return (
    <Link
      to="/n/$node/s/$session"
      params={{ node: nodeParam, session: session.id }}
      className="flex items-start gap-3 rounded-lg border border-border bg-surface px-3 py-2.5 hover:bg-bg-hover"
      data-testid="session-list-item"
    >
      <span className="mt-1.5">
        <SessionDot status={session.status} />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2">
          <span className="truncate font-medium">{session.title || session.id}</span>
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
  )
}
