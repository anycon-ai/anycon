import { useEffect } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { homeTarget } from '@/features/unlock/nodes'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'

/**
 * `/`：这个浏览器有已配对的节点 → 上次打开的 `/n/:node`；没有，但账号下有节点 → `/unlock`；账号下也没有 → `/setup`
 * （spec/web.md § 2；判定在 `homeTarget`）。
 */
export function HomeRedirect() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const order = useNodes((s) => s.order)
  const loaded = useNodes((s) => s.loaded)
  const lastNode = usePrefs((s) => s.lastNode)
  // 有已配对的节点时不需要问 hub 账号下有什么
  const needDevices = loaded && order.length === 0
  const devices = useDevices({ enabled: needDevices })
  const target = homeTarget({ nodesLoaded: loaded, localOrder: order, lastNode, devices: devices.data, devicesFailed: devices.isError })

  const nodeTarget = target.to === 'node' ? target.node : null
  useEffect(() => {
    switch (target.to) {
      case 'node':
        if (nodeTarget) void navigate({ to: '/n/$node', params: { node: shortNodeId(nodeTarget, order) }, replace: true })
        return
      case 'unlock':
        void navigate({ to: '/unlock', replace: true })
        return
      case 'setup':
        void navigate({ to: '/setup', replace: true })
        return
      case 'wait':
        return
    }
  }, [target.to, nodeTarget, order, navigate])

  return (
    <div className="flex min-h-dvh items-center justify-center" data-testid="home-redirect" data-target={target.to}>
      {target.to === 'wait' && devices.isError ? (
        <Button onClick={() => void devices.refetch()}>{t('app.retry')}</Button>
      ) : (
        <Spinner className="size-6" />
      )}
    </div>
  )
}
