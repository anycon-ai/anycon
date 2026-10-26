import { Navigate, Outlet, useMatchRoute, useNavigate } from '@tanstack/react-router'
import { ServerOff, Settings, ShieldAlert, Unplug } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { forgetNode } from '@/features/devices/removeNode'
import { accountNodes, findAccountNode, removedNodes, unlockParam } from '@/features/unlock/nodes'
import { NotFoundPage } from '@/pages/NotFoundPage'
import { useCurrentNode } from '@/lib/nav'
import { describeError } from '@/lib/errors'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { PROTOCOL_VERSION } from '@/lib/constants'
import { Button } from '@/ui/Button'
import { EmptyState, Spinner } from '@/ui/Misc'
import { PageHeader } from './PageHeader'

/**
 * `/n/:node/*` 的守卫：未知节点 → 404；协议版本不兼容 → 整页提示；
 * Noise 握手失败（daemon 不认识此设备）→ 整页提示重新配对；账号下已经没有这台节点（在别的设备上被移除了）→ 整页提示，可以忘记它。
 * 节点离线不拦截。版本不兼容和握手失败的整页提示不挡节点设置页：节点可能再也连不上，「关于」里的「从账号移除」得能用。
 */
export function NodeGuard() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { param, nodeId, entry, loaded } = useCurrentNode()
  const order = useNodes((s) => s.order)
  const matchRoute = useMatchRoute()
  const onSettings = !!matchRoute({ to: '/n/$node/settings' })
  // 本账号的节点、只是这个浏览器还没配对（通知 / 书签 / 别的设备发来的链接）→ 去解锁页（ADR-36）。只在找不到本地节点时才问 hub
  const unknown = loaded && (!nodeId || !entry)
  // 已配对的节点不为它去问 hub，但左栏已经拉过的设备列表照样拿来用（关掉的查询也读缓存）
  const devices = useDevices({ enabled: unknown })
  const forget = (id: string): void => {
    forgetNode(id)
      .then(() => void navigate({ to: '/' }))
      // 本地库删不掉（存储被拒绝 / 库被占用）：节点还在，要告诉用户，别静默
      .catch((e: unknown) => toast.error(describeError(e, t)))
  }

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

  if (removedNodes(devices.data, devices.dataUpdatedAt ?? 0, [entry]).length > 0)
    return (
      <>
        <PageHeader crumbs={[{ label: entry.name }]} />
        <div className="flex flex-1 items-center justify-center" data-testid="node-removed">
          <EmptyState
            icon={<ServerOff className="size-10" />}
            title={t('node.removedTitle')}
            action={
              <div className="flex flex-wrap justify-center gap-2">
                <Button variant="primary" onClick={() => forget(nodeId)} data-testid="node-removed-forget">
                  {t('node.forget')}
                </Button>
                <Button variant="outline" onClick={() => void navigate({ to: '/pair' })}>
                  {t('sidebar.addNode')}
                </Button>
              </div>
            }
          >
            {t('node.removedBody')}
          </EmptyState>
        </div>
      </>
    )

  const settingsButton = (
    <Button variant="outline" icon={<Settings className="size-4" />} onClick={() => void navigate({ to: '/n/$node/settings', params: { node: shortNodeId(nodeId, order) } })} data-testid="guard-node-settings">
      {t('node.settings')}
    </Button>
  )

  if (entry.versionMismatch && !onSettings)
    return (
      <>
        <PageHeader crumbs={[{ label: entry.name }]} />
        <div className="flex flex-1 items-center justify-center" data-testid="version-mismatch">
          <EmptyState
            icon={<ShieldAlert className="size-10" />}
            title={t('node.versionTitle')}
            action={
              <div className="flex flex-wrap justify-center gap-2">
                <Button onClick={() => location.reload()}>{t('banner.reload')}</Button>
                {settingsButton}
              </div>
            }
          >
            {t('node.versionBody', { daemon: entry.info?.protocol_version ?? '?', web: PROTOCOL_VERSION })}
          </EmptyState>
        </div>
      </>
    )

  if (entry.conn === 'noise_failed' && !onSettings)
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
                <Button variant="outline" onClick={() => forget(nodeId)}>
                  {t('node.forget')}
                </Button>
                {settingsButton}
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
