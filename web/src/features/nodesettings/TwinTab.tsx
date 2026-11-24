// 节点设置 →「Bot」（docs/spec/web.md § 4.8）：本节点 Bot 的全部配置集中在这里——判断后端、范围与预算、用别的节点、
// 被别的节点用；Bot 页只看运行状态。标签只在节点支持 Bot 时才有（`NodeSettingsPage`）。
import { useEffect } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { syncTwin } from '@/features/twin/service'
import { missingTriage, twinHead, twinHeadLabel } from '@/features/twin/state'
import { useTwin } from '@/features/twin/store'
import { TwinLinks } from '@/features/twin/TwinLinks'
import { Button } from '@/ui/Button'
import { Card } from '@/ui/Misc'
import { ConfigItemsForm, isTwinKey } from './ConfigItemsForm'
import { TwinScopeSection } from './TwinScopeSection'
import { TwinUsedBy } from './TwinUsedBy'

export function TwinTab({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const node = useNodes((s) => s.byId[nodeId])
  const order = useNodes((s) => s.order)
  const entry = useTwin((s) => s.nodes[nodeId])
  const online = node?.conn === 'ready'
  // 直接打开这个标签时 Bot 状态可能还没读过；连接恢复后也补读一次
  useEffect(() => {
    if (online) void syncTwin(nodeId)
  }, [online, nodeId])
  if (!node) return null
  const value = entry?.value
  const state = twinHeadLabel(t, twinHead(node, entry))

  return (
    <div className="space-y-6" data-testid="twin-tab">
      <Card className="space-y-2 p-4" data-testid="twin-tab-status">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-sm font-medium" data-testid="twin-tab-state">
            {state}
          </p>
          <Link to="/twin/$node" params={{ node: shortNodeId(nodeId, order) }} className="text-sm text-accent-text" data-testid="twin-tab-runs-link">
            {t('nodeSettings.twin.viewRuns')}
          </Link>
        </div>
        {online && entry?.failed ? (
          <p role="alert" className="flex flex-wrap items-center gap-2 text-sm text-danger">
            {t('twin.loadFailed')}
            <Button size="lg" onClick={() => void syncTwin(nodeId)}>
              {t('todos.retry')}
            </Button>
          </p>
        ) : null}
        {online && value && !value.configured ? (
          <ul className="list-inside list-disc text-sm text-fg-muted" data-testid="twin-tab-missing">
            {!value.scope.projects.length ? <li>{t('twin.noProjects')}</li> : null}
            {missingTriage(node) ? <li>{t('twin.noTriage')}</li> : null}
          </ul>
        ) : null}
      </Card>

      <div className="space-y-2">
        <p className="text-sm text-fg-muted">{t('nodeSettings.twin.judgeHint')}</p>
        <ConfigItemsForm nodeId={nodeId} include={isTwinKey} prefix="twin-config" />
      </div>

      <section id="twin-scope" data-testid="twin-tab-scope">
        <TwinScopeSection node={node} />
      </section>

      <Card className="p-4" data-testid="twin-tab-links">
        <TwinLinks node={node} />
      </Card>

      <TwinUsedBy node={node} />
    </div>
  )
}
