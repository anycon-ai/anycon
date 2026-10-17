import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { NodeStats } from '@/protocol'
import { formatBytes, formatDateTime, formatDuration } from '@/lib/format'
import { warnOnError } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Card } from '@/ui/Misc'

export function LimitsTab() {
  const { t } = useTranslation()
  return (
    <Card className="space-y-3 p-4">
      <p className="text-sm">{t('nodeSettings.limitsBody')}</p>
      <ul className="list-inside list-disc space-y-1 text-sm text-fg-muted">
        <li>{t('nodeSettings.limitConcurrent')}</li>
        <li>{t('nodeSettings.limitIdle')}</li>
        <li>{t('nodeSettings.limitDangerous')}</li>
      </ul>
      <code className="block rounded-md bg-bg-muted px-3 py-2 font-mono text-xs">anycon config path</code>
      <p className="text-xs text-fg-subtle">{t('nodeSettings.limitsLater')}</p>
    </Card>
  )
}

export function AboutTab({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const [stats, setStats] = useState<NodeStats | null>(null)
  useEffect(() => {
    if (entry?.conn !== 'ready') return
    let cancelled = false
    void rpc()
      .request<NodeStats>(nodeId, 'node.stats', {})
      .then((s) => !cancelled && setStats(s))
      .catch(warnOnError('read the node stats'))
    return () => {
      cancelled = true
    }
  }, [nodeId, entry?.conn])
  if (!entry) return null
  const info = entry.info
  return (
    <Card className="divide-y divide-border" testId="node-about">
      <AboutRow label={t('nodeSettings.aboutName')}>{entry.name}</AboutRow>
      <AboutRow label={t('nodeSettings.aboutVersion')}>{info ? `v${info.version}` : '—'}</AboutRow>
      <AboutRow label={t('nodeSettings.aboutProtocol')}>{info?.protocol_version ?? '—'}</AboutRow>
      <AboutRow label={t('nodeSettings.aboutPlatform')}>{info?.platform ?? '—'}</AboutRow>
      <AboutRow label={t('nodeSettings.aboutNodeId')}>
        <span className="break-all font-mono text-xs">{entry.id}</span>
      </AboutRow>
      {info ? <AboutRow label={t('nodeSettings.aboutStarted')}>{formatDateTime(info.started_at_ms, i18n.language)}</AboutRow> : null}
      {stats ? (
        <>
          <AboutRow label={t('nodeSettings.aboutUptime')}>{formatDuration(stats.uptime_ms)}</AboutRow>
          <AboutRow label={t('nodeSettings.aboutDb')}>{formatBytes(stats.db_bytes)}</AboutRow>
          <AboutRow label={t('nodeSettings.aboutDisk')}>{formatBytes(stats.disk_free_bytes)}</AboutRow>
        </>
      ) : null}
      <div className="px-4 py-3">
        <Button variant="outline" size="sm" disabled title={t('nodeSettings.updatesLater')}>
          {t('nodeSettings.checkUpdates')}
        </Button>
      </div>
    </Card>
  )
}

function AboutRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 px-4 py-3 text-sm">
      <span className="text-fg-muted">{label}</span>
      <span className="min-w-0 text-right">{children}</span>
    </div>
  )
}
