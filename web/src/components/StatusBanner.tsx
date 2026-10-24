import { Link, useNavigate } from '@tanstack/react-router'
import { AlertTriangle, Bell, CloudOff, RefreshCw, ShieldQuestion, WifiOff } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { dismissNotificationsPrompt, useNotificationsPrompt } from '@/features/push/usePrompt'
import { countUnsent, flushUnsent } from '@/features/session/actions'
import { getClient } from '@/transport/instance'
import { useNow } from '@/lib/useNow'
import { cleanHintText } from '@/lib/hint'
import { pendingApprovals, useEnroll } from '@/stores/enroll'
import { useDrafts } from '@/stores/drafts'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import type { NodeConnState } from '@/transport/connection'
import { pickBanner, type BannerKind } from './banner'

const toneOf: Record<BannerKind, 'danger' | 'warning' | 'info'> = {
  hub_reconnecting: 'danger',
  hub_superseded: 'warning',
  hub_revoked: 'danger',
  hub_version: 'danger',
  hub_quota: 'danger',
  node_offline: 'warning',
  enroll_request: 'info',
  storage: 'warning',
  unsent: 'warning',
  notifications: 'info',
  update: 'info',
}

export function StatusBanner({ nodeId, nodeConn }: { nodeId: string | null; nodeConn: NodeConnState | null }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const hub = useHub((s) => s.snapshot)
  const started = useHub((s) => s.started)
  const updateReady = useUi((s) => s.updateReady)
  const applyUpdate = useUi((s) => s.applyUpdate)
  const storageError = useUi((s) => s.storageError)
  // 订阅这两个 store 让未发送数量变化时重新渲染
  useDrafts((s) => s.byKey)
  useSessions((s) => s.list)
  const unsent = countUnsent(nodeId)
  // 有设备请求接入某台节点（ADR-36）：任何页面都显示——请求只有 5 分钟，不能因为用户正在别的节点上就错过。当前节点的优先
  const enrollItems = useEnroll((s) => s.items)
  const enrollResolved = useEnroll((s) => s.resolved)
  const now = useNow(1000, Object.keys(enrollItems).length > 0)
  const pending = pendingApprovals({ items: enrollItems, resolved: enrollResolved }, now)
  const firstPending = pending.find((p) => p.nodeId === nodeId) ?? pending[0]
  const requestNodeName = useNodes((s) => (firstPending ? s.byId[firstPending.nodeId]?.name : undefined))

  const notificationsPrompt = useNotificationsPrompt()

  const kind = pickBanner({ hubPhase: hub.phase, hubAttempt: hub.attempt, hubHalt: hub.halt, started, nodeConn, unsent, storageError, notificationsPrompt, updateReady, enrollRequests: pending.length })
  if (!kind) return null

  const tone = toneOf[kind]
  const Icon = kind === 'node_offline' ? CloudOff : kind === 'hub_reconnecting' ? WifiOff : kind === 'update' ? RefreshCw : kind === 'enroll_request' ? ShieldQuestion : kind === 'notifications' ? Bell : AlertTriangle
  let action: React.ReactNode = null
  let text: string
  switch (kind) {
    case 'hub_reconnecting':
      text = t('banner.hubReconnecting')
      action = (
        <Button size="sm" variant="outline" onClick={() => getClient().forceReconnect()}>
          {t('banner.retryNow')}
        </Button>
      )
      break
    case 'hub_superseded':
      text = t('banner.superseded')
      action = (
        <Button size="sm" variant="outline" onClick={() => getClient().takeOver()}>
          {t('banner.useHere')}
        </Button>
      )
      break
    case 'hub_revoked':
      text = t('banner.revoked')
      action = (
        <Link to="/pair">
          <Button size="sm" variant="outline">
            {t('banner.pairAgain')}
          </Button>
        </Link>
      )
      break
    case 'hub_version':
      text = t('banner.version')
      action = (
        <Button size="sm" variant="outline" onClick={() => location.reload()}>
          {t('banner.reload')}
        </Button>
      )
      break
    case 'hub_quota':
      text = t('banner.quota')
      action = (
        <Button size="sm" variant="outline" onClick={() => void navigate({ to: '/settings/quota' })} data-testid="banner-quota-view">
          {t('banner.quotaView')}
        </Button>
      )
      break
    case 'node_offline':
      text = t('banner.nodeOffline')
      break
    case 'enroll_request':
      text = pending.length > 1 ? t('banner.enrollMany', { count: pending.length }) : t('banner.enrollOne', { node: cleanHintText(requestNodeName) || t('security.thisNode') })
      action = firstPending ? (
        <Button size="sm" variant="outline" onClick={() => useEnroll.getState().openDialog(firstPending.nodeId, firstPending.info.enroll_id)} data-testid="banner-enroll-review">
          {t('banner.review')}
        </Button>
      ) : null
      break
    case 'storage':
      text = t('banner.storage')
      action = (
        <Button size="sm" variant="outline" onClick={() => useUi.getState().setStorageError(false)} data-testid="banner-storage-dismiss">
          {t('common.dismiss')}
        </Button>
      )
      break
    case 'unsent':
      text = t('banner.unsent', { count: unsent })
      action = nodeId ? (
        <Button size="sm" variant="outline" onClick={() => void flushUnsent(nodeId)} data-testid="banner-resend">
          {t('banner.resendNow')}
        </Button>
      ) : null
      break
    case 'notifications':
      // 点「去开启」也算提示过了（只提示一次）；真开启之后本来就不会再显示
      text = t('banner.notifications')
      action = (
        <div className="flex shrink-0 items-center gap-2">
          <Button
            size="sm"
            variant="outline"
            data-testid="banner-notifications-enable"
            onClick={() => {
              dismissNotificationsPrompt()
              void navigate({ to: '/settings/notifications' })
            }}
          >
            {t('banner.notificationsEnable')}
          </Button>
          <Button size="sm" variant="ghost" onClick={dismissNotificationsPrompt} data-testid="banner-notifications-dismiss">
            {t('common.dismiss')}
          </Button>
        </div>
      )
      break
    case 'update':
      text = t('banner.update')
      action = (
        <Button size="sm" variant="outline" onClick={() => applyUpdate?.()}>
          {t('banner.refresh')}
        </Button>
      )
      break
  }

  return (
    <div
      role="status"
      data-testid="status-banner"
      data-kind={kind}
      className={cn(
        'flex items-center gap-3 border-b px-4 py-2 text-sm',
        tone === 'danger' && 'border-danger/30 bg-danger-subtle text-danger',
        tone === 'warning' && 'border-warning/30 bg-warning-subtle text-warning',
        tone === 'info' && 'border-info/30 bg-info-subtle text-info',
      )}
    >
      <Icon className="size-4 shrink-0" aria-hidden />
      <span className="min-w-0 flex-1">{text}</span>
      {action}
    </div>
  )
}
