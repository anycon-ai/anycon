import { Monitor } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useBrowserSessions, useRevokeBrowserSession } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { formatRelative } from '@/lib/format'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card, Chip, Skeleton } from '@/ui/Misc'

export function BrowserSessionsPage() {
  const { t, i18n } = useTranslation()
  const sessions = useBrowserSessions()
  const revoke = useRevokeBrowserSession()
  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('settings.sessions')}</h1>
      <p className="text-sm text-fg-muted">{t('settings.sessionsHint')}</p>
      {sessions.isLoading ? <Skeleton className="h-16" /> : null}
      <ul className="space-y-2" data-testid="browser-session-list">
        {(sessions.data ?? []).map((s) => (
          <li key={s.id}>
            <Card className="flex items-center gap-3 p-3">
              <Monitor className="size-5 shrink-0 text-fg-subtle" aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="flex flex-wrap items-center gap-2 text-sm font-medium">
                  <span className="truncate">{s.user_agent || t('settings.unknownBrowser')}</span>
                  {s.current ? <Chip tone="accent">{t('settings.currentSession')}</Chip> : null}
                </p>
                <p className="text-xs text-fg-subtle">
                  {s.ip_prefix} · {t('settings.lastActive', { when: formatRelative(s.last_seen_ms, i18n.language) })}
                </p>
              </div>
              {!s.current ? (
                <Button size="sm" variant="outline" loading={revoke.isPending && revoke.variables === s.id} onClick={() => revoke.mutate(s.id, { onError: (e) => toast.error(describeError(e, t)) })}>
                  {t('settings.signOutSession')}
                </Button>
              ) : null}
            </Card>
          </li>
        ))}
      </ul>
    </div>
  )
}
