import { useEffect, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { NodeInfo, NodeStats } from '@/protocol'
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
        <UpdateRow info={info} />
        <div className="flex flex-wrap gap-2 px-4 py-3">
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

/** 给用户看的发布页链接只能在这个前缀下（ADR-57）；daemon 已经按版本号拼好，这里再核对一次。 */
const RELEASE_PAGE_PREFIX = 'https://github.com/anycon-ai/anycon/releases/tag/v'

/**
 * `node.info.update_available` 里能显示的部分：`latest` 是 `X.Y.Z`，链接正好是那个版本的 GitHub 发布页。
 * 不合格（旧的或出了错的 daemon）就当作没有，不显示任何来自回应的文字。
 */
function validUpdate(info: NodeInfo | undefined): { latest: string; url: string } | null {
  const u = info?.update_available
  if (!u) return null
  if (!/^(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})$/.test(u.latest)) return null
  if (u.url !== `${RELEASE_PAGE_PREFIX}${u.latest}`) return null
  return { latest: u.latest, url: u.url }
}

/**
 * 「更新」一行：节点的更新检查（ADR-57）知道有更新的版本时，显示版本号和发布页链接。没有时不显示这一行：
 * `node.info` 不区分「已是最新」和「检查关着」，详情在节点的 `anycon doctor` 里。
 */
function UpdateRow({ info }: { info: NodeInfo | undefined }) {
  const { t } = useTranslation()
  const u = validUpdate(info)
  if (!u) return null
  return (
    <AboutRow label={t('nodeSettings.aboutUpdate')}>
      <span data-testid="node-update">
        {t('nodeSettings.updateAvailable', { version: u.latest })}{' '}
        <a href={u.url} target="_blank" rel="noopener noreferrer" className="font-medium text-accent-text hover:underline" data-testid="node-update-link">
          {t('nodeSettings.updateReleaseNotes')}
        </a>
      </span>
    </AboutRow>
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
