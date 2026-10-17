import { Navigate, Outlet, useNavigate } from '@tanstack/react-router'
import { ShieldAlert, Unplug } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { accountNodes, findAccountNode, unlockParam } from '@/features/unlock/nodes'
import { NotFoundPage } from '@/pages/NotFoundPage'
import { useCurrentNode } from '@/lib/nav'
import { describeError } from '@/lib/errors'
import { getClient } from '@/transport/instance'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { PROTOCOL_VERSION } from '@/lib/constants'
import { Button } from '@/ui/Button'
import { EmptyState, Spinner } from '@/ui/Misc'
import { PageHeader } from './PageHeader'

/**
 * `/n/:node/*` 的守卫：未知节点 → 404；协议版本不兼容 → 整页提示；
 * Noise 握手失败（daemon 不认识此设备）→ 整页提示重新配对。节点离线不拦截。
 */
export function NodeGuard() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { param, nodeId, entry, loaded } = useCurrentNode()
  const order = useNodes((s) => s.order)
  // 本账号的节点、只是这个浏览器还没配对（通知 / 书签 / 别的设备发来的链接）→ 去解锁页（ADR-36）。只在找不到本地节点时才问 hub
  const unknown = loaded && (!nodeId || !entry)
  const devices = useDevices({ enabled: unknown })

  if (!loaded)
    return (
      <div className="flex flex-1 items-center justify-center">
        <Spinner className="size-6" />
      </div>
    )
  if (!nodeId || !entry) {
    if (devices.isPending && devices.fetchStatus !== 'idle')
      return (
        <div className="flex flex-1 items-center justify-center" aria-busy="true">
          <Spinner className="size-6" />
        </div>
      )
    const nodes = accountNodes(devices.data)
    const hit = param ? findAccountNode(param, nodes) : undefined
    if (hit && !order.includes(hit.id)) return <Navigate to="/unlock/$node" params={{ node: unlockParam(hit, nodes) }} replace />
    return <NotFoundPage title={t('node.unknownTitle')} body={t('node.unknownBody')} />
  }

  if (entry.versionMismatch)
    return (
      <>
        <PageHeader crumbs={[{ label: entry.name }]} />
        <div className="flex flex-1 items-center justify-center" data-testid="version-mismatch">
          <EmptyState icon={<ShieldAlert className="size-10" />} title={t('node.versionTitle')} action={<Button onClick={() => location.reload()}>{t('banner.reload')}</Button>}>
            {t('node.versionBody', { daemon: entry.info?.protocol_version ?? '?', web: PROTOCOL_VERSION })}
          </EmptyState>
        </div>
      </>
    )

  if (entry.conn === 'noise_failed')
    return (
      <>
        <PageHeader crumbs={[{ label: entry.name }]} />
        <div className="flex flex-1 items-center justify-center" data-testid="noise-failed">
          <EmptyState
            icon={<Unplug className="size-10" />}
            title={t('node.noiseFailedTitle')}
            action={
              <div className="flex flex-wrap justify-center gap-2">
                <Button variant="primary" onClick={() => void navigate({ to: '/unlock/$node', params: { node: shortNodeId(nodeId, order) } })} data-testid="reunlock">
                  {t('node.reunlock')}
                </Button>
                <Button variant="outline" onClick={() => void navigate({ to: '/pair' })} data-testid="repair">
                  {t('node.repair')}
                </Button>
                <Button
                  variant="outline"
                  onClick={() => {
                    getClient()
                      .removeNode(nodeId)
                      .then(() => {
                        useNodes.getState().remove(nodeId)
                        void navigate({ to: '/' })
                      })
                      // 本地库删不掉（存储被拒绝 / 库被占用）：节点还在，要告诉用户，别静默
                      .catch((e: unknown) => toast.error(describeError(e, t)))
                  }}
                >
                  {t('node.forget')}
                </Button>
              </div>
            }
          >
            {t('node.noiseFailedBody')}
          </EmptyState>
        </div>
      </>
    )

  return <Outlet />
}
