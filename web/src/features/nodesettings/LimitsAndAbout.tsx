import { useEffect, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { NodeStats } from '@/protocol'
import { useRemoveNode } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { formatBytes, formatDateTime, formatDuration } from '@/lib/format'
import { warnOnError } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { LogsDialog } from './LogsDialog'

export function AboutTab({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const [stats, setStats] = useState<NodeStats | null>(null)
  const [logsOpen, setLogsOpen] = useState(false)
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
    <div className="space-y-6">
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
        <div className="flex flex-wrap gap-2 px-4 py-3">
          <Button variant="outline" size="sm" disabled title={t('nodeSettings.updatesLater')}>
            {t('nodeSettings.checkUpdates')}
          </Button>
          <Button variant="outline" size="sm" disabled={entry.conn !== 'ready'} onClick={() => setLogsOpen(true)} data-testid="logs-open">
            {t('nodeSettings.logs.open')}
          </Button>
        </div>
      </Card>
      {logsOpen ? <LogsDialog nodeId={nodeId} open={logsOpen} onOpenChange={setLogsOpen} /> : null}
      <RemoveNodeCard nodeId={nodeId} name={entry.name} />
    </div>
  )
}

/**
 * 「移除这台节点」：机器重装了、不再用了的节点，从账号里移除并在这个浏览器里忘记它。节点离线也能做（spec/web.md § 4.8）。
 * 只是吊销和忘记：节点机器上的项目和会话历史不会被删除。
 */
function RemoveNodeCard({ nodeId, name }: { nodeId: string; name: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const remove = useRemoveNode()
  const [open, setOpen] = useState(false)
  return (
    <Card className="space-y-3 border-danger/40 p-4" testId="node-remove">
      <div className="space-y-1">
        <p className="text-sm font-medium">{t('nodeSettings.removeTitle')}</p>
        <p className="text-sm text-fg-muted">{t('nodeSettings.removeHint')}</p>
      </div>
      <Button variant="outline-danger" size="sm" onClick={() => setOpen(true)} data-testid="node-remove-open">
        {t('nodeSettings.remove')}
      </Button>
      <Dialog open={open} onOpenChange={setOpen} title={t('nodeSettings.removeConfirmTitle', { name })} description={t('nodeSettings.removeConfirmBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setOpen(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="danger"
            loading={remove.isPending}
            data-testid="node-remove-confirm"
            // 用 mutateAsync：节点一忘记，这张卡片就跟着节点页一起卸载了，mutate 的回调不会再被调用
            onClick={() =>
              void remove.mutateAsync(nodeId).then(
                () => {
                  toast.success(t('nodeSettings.removed', { name }))
                  void navigate({ to: '/' })
                },
                // 失败要说出来，对话框留着可以重试
                (e: unknown) => toast.error(t('nodeSettings.removeFailed', { error: describeError(e, t) })),
              )
            }
          >
            {t('nodeSettings.remove')}
          </Button>
        </div>
      </Dialog>
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
