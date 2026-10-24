import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { RefreshCw, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PreviewInfo, PreviewListResult } from '@/protocol'
import { describePreviewError } from '@/features/preview/errors'
import { closePreview } from '@/features/preview/ticket'
import { describeError } from '@/lib/errors'
import { formatDateTime, formatRelative } from '@/lib/format'
import { useNodeParam } from '@/lib/nav'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card, Chip, SectionTitle, Spinner } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

type Load = { status: 'loading' } | { status: 'ready'; previews: PreviewInfo[] } | { status: 'error'; error: unknown }

/**
 * 节点设置 →「预览」（docs/spec/web.md § 4.8，ADR-39）：`preview.list` 的全部未过期票据——端口、所属会话、签发设备、过期时间、
 * 此刻的隧道数。每行「关闭」调 `preview.close`：同一个 label 的全部票据一起作废，别的设备上开着的预览会断开。
 * 下面一段只读地说明端口策略；策略在 daemon 的配置里，这里改不了。
 */
export function PreviewTab({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const sessions = useSessions((s) => s.list)
  const nodeParam = useNodeParam(nodeId)
  const offline = entry?.conn !== 'ready'
  const [load, setLoad] = useState<Load>({ status: 'loading' })
  const [reload, setReload] = useState(0)
  const [target, setTarget] = useState<PreviewInfo | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (offline) return
    let alive = true
    rpc()
      .request<PreviewListResult>(nodeId, 'preview.list', {})
      .then((res) => alive && setLoad({ status: 'ready', previews: [...res.previews].sort((a, b) => a.port - b.port || a.created_at_ms - b.created_at_ms) }))
      .catch((error: unknown) => {
        console.warn('anycon: could not list the previews', error)
        if (alive) setLoad({ status: 'error', error })
      })
    return () => {
      alive = false
    }
  }, [nodeId, offline, reload])

  const close = async (): Promise<void> => {
    if (!target) return
    setBusy(true)
    try {
      await closePreview(nodeId, target.label)
      toast.success(t('preview.stoppedToast', { port: target.port }))
      setTarget(null)
      setReload((n) => n + 1)
    } catch (e) {
      toast.error(describePreviewError(e, t))
    } finally {
      setBusy(false)
    }
  }

  const previews = load.status === 'ready' ? load.previews : []

  return (
    <div className="space-y-6" data-testid="preview-tab">
      <section>
        <SectionTitle
          action={
            <Button size="icon" variant="ghost" aria-label={t('git.refresh')} title={t('git.refresh')} onClick={() => setReload((n) => n + 1)} disabled={offline} data-testid="preview-list-refresh">
              <RefreshCw className="size-4" />
            </Button>
          }
        >
          {t('preview.list.title')}
        </SectionTitle>
        <p className="mb-3 text-sm text-fg-muted">{t('preview.list.hint')}</p>

        {offline ? (
          <p className="text-sm text-fg-subtle" data-testid="preview-list-offline">
            {t('preview.list.offline')}
          </p>
        ) : null}
        {load.status === 'loading' && !offline ? (
          <div className="flex items-center gap-2 text-sm text-fg-subtle">
            <Spinner /> {t('git.loading')}
          </div>
        ) : null}
        {load.status === 'error' && !offline ? (
          <p className="text-sm text-danger" data-testid="preview-list-error">
            {describeError(load.error, t)}
          </p>
        ) : null}
        {load.status === 'ready' && previews.length === 0 ? (
          <p className="text-sm text-fg-subtle" data-testid="preview-list-empty">
            {t('preview.list.empty')}
          </p>
        ) : null}

        <ul className="space-y-2">
          {previews.map((p) => {
            const session = p.session_id ? sessions[p.session_id] : undefined
            return (
              <li key={p.ticket_id} data-testid="preview-row" data-label={p.label} data-port={p.port}>
                <Card className="space-y-2 p-3">
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                    <span className="font-mono font-medium" data-testid="preview-row-port">
                      {p.port}
                    </span>
                    <Chip tone={p.tunnels > 0 ? 'success' : 'neutral'} testId="preview-row-tunnels">
                      {t('preview.list.tunnels', { count: p.tunnels })}
                    </Chip>
                  </div>
                  <p className="text-xs text-fg-subtle">
                    {p.session_id ? (
                      <Link to="/n/$node/s/$session" params={{ node: nodeParam, session: p.session_id }} className="text-accent-text hover:underline" data-testid="preview-row-session">
                        {session?.title || p.session_id}
                      </Link>
                    ) : (
                      <span data-testid="preview-row-no-session">{t('preview.list.noSession')}</span>
                    )}
                    {' · '}
                    <span data-testid="preview-row-device">{t('preview.list.openedBy', { device: p.created_by_name || p.created_by })}</span>
                    {' · '}
                    <span title={formatDateTime(p.expires_at_ms, i18n.language)} data-testid="preview-row-expires">
                      {t('preview.list.expires', { time: formatRelative(p.expires_at_ms, i18n.language) })}
                    </span>
                  </p>
                  <Button variant="outline-danger" icon={<X className="size-4" />} disabled={offline || busy} onClick={() => setTarget(p)} data-testid="preview-row-close">
                    {t('preview.list.close')}
                  </Button>
                </Card>
              </li>
            )
          })}
        </ul>
      </section>

      <section data-testid="preview-policy">
        <SectionTitle>{t('preview.policy.title')}</SectionTitle>
        <div className="space-y-2 text-sm text-fg-muted">
          <p>{t('preview.policy.default')}</p>
          <p>{t('preview.policy.allowPorts')}</p>
        </div>
      </section>

      <Dialog
        open={target !== null}
        onOpenChange={(open) => !open && !busy && setTarget(null)}
        title={t('preview.list.closeTitle', { port: target?.port ?? '' })}
        description={t('preview.stopBody')}
        closeLabel={t('common.close')}
        testId="preview-close-dialog"
      >
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setTarget(null)} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={busy} onClick={() => void close()} data-testid="preview-close-confirm">
            {t('preview.list.close')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}
