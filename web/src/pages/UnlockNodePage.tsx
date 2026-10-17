import { useCallback, useEffect, useState } from 'react'
import { Navigate, useNavigate } from '@tanstack/react-router'
import { Lock } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { CenterLayout } from '@/components/CenterLayout'
import { findAccountNode } from '@/features/unlock/nodes'
import { accountNodes } from '@/features/unlock/nodes'
import { UnlockPanel } from '@/features/unlock/UnlockPanel'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Card, Spinner } from '@/ui/Misc'
import { NotFoundPage } from './NotFoundPage'

/**
 * `/unlock/:node`（ADR-36，spec/web.md § 4.3.1）：账号下的一台节点、这个浏览器还没和它配对时，
 * 用扫码批准 / 请求批准 / 接入码 / 节点口令解锁，不需要短码。
 */
export function UnlockNodePage({ node: param }: { node: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  // 节点上线后自动恢复：页面开着期间定时刷新在线状态
  const devices = useDevices({ refetchMs: 8000 })
  const local = useNodes((s) => s.byId)
  const [hubSaysNotFound, setHubSaysNotFound] = useState(false)

  const nodes = accountNodes(devices.data)
  const target = findAccountNode(param, nodes)
  const entry = target ? local[target.id] : undefined

  const done = useCallback(
    (nodeId: string) => {
      const order = useNodes.getState().order
      void navigate({ to: '/n/$node', params: { node: shortNodeId(nodeId, order) }, replace: true })
    },
    [navigate],
  )
  const notFound = useCallback(() => setHubSaysNotFound(true), [])
  // 换了节点参数就重新来
  useEffect(() => setHubSaysNotFound(false), [param])

  if (devices.isPending) return <Loading />
  if (devices.isError) {
    return (
      <CenterLayout medium>
        <Card className="space-y-3 p-5 text-center" testId="unlock-load-failed">
          <p className="text-sm text-danger">{t('unlock.loadFailed')}</p>
          <Button onClick={() => void devices.refetch()}>{t('app.retry')}</Button>
        </Card>
      </CenterLayout>
    )
  }
  if (!target || hubSaysNotFound) return <NotFoundPage title={t('node.unknownTitle')} body={t('unlock.notYours')} />
  // 这个浏览器已经和它配对了、而且连得上：不用解锁（书签 / 旧链接）。连不上（noise_failed）才需要重新解锁
  if (entry && entry.conn !== 'noise_failed') return <Navigate to="/n/$node" params={{ node: shortNodeId(target.id, useNodes.getState().order) }} replace />

  const name = cleanHintText(target.name) || t('unlock.thisNode')
  return (
    <CenterLayout medium>
      <Card className="p-5" testId="unlock-node">
        <div className="mb-1 flex items-center gap-2">
          <Lock className="size-5 text-accent-text" aria-hidden />
          <h1 className="text-lg font-semibold" data-testid="unlock-title">
            {t('unlock.title', { node: name })}
          </h1>
        </div>
        <p className="mb-4 text-sm text-fg-muted">{t('unlock.subtitle')}</p>
        <UnlockPanel node={target} online={target.online} onDone={done} onNotFound={notFound} />
      </Card>
      <div className="mt-4 flex flex-col items-center gap-1">
        <Button variant="ghost" onClick={() => void navigate({ to: '/pair' })} data-testid="unlock-use-code">
          {t('unlock.usePairCode')}
        </Button>
        {nodes.length > 1 ? (
          <Button variant="ghost" onClick={() => void navigate({ to: '/unlock' })}>
            {t('unlock.otherNodes')}
          </Button>
        ) : null}
        {Object.keys(local).length > 0 ? (
          <Button variant="ghost" onClick={() => void navigate({ to: '/' })}>
            {t('common.cancel')}
          </Button>
        ) : null}
      </div>
    </CenterLayout>
  )
}

function Loading() {
  return (
    <div className="flex min-h-dvh items-center justify-center" aria-busy="true">
      <Spinner className="size-6" />
    </div>
  )
}
