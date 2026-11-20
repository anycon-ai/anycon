import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { SessionsPanel } from '@/components/Sidebar'
import { useCurrentNode } from '@/lib/nav'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'

/**
 * 手机「会话」标签页（`/sessions`，spec/web.md § 14.2）：原来左侧抽屉里的东西——节点过滤、新建项目、搜索、Recent、
 * 项目树、临时会话——铺满一屏。桌面上这些就在左栏里，这个页面只给手机用。
 */
export function SessionsPage() {
  const { t } = useTranslation()
  const { nodeId } = useCurrentNode()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const lastNode = usePrefs((s) => s.lastNode)
  const setPalette = useUi((s) => s.setPalette)
  const activeNode = nodeId ?? (lastNode && byId[lastNode] ? lastNode : order[0])
  return (
    <div className="flex min-h-0 flex-1 flex-col bg-bg-subtle" data-testid="sessions-page">
      <PageHeader root crumbs={[{ label: t('tabs.sessions') }]} showNodeStatus={false} spaceSwitcher={false} />
      <div className="flex min-h-0 flex-1 flex-col pt-3">
        <SessionsPanel nodeId={activeNode} onSearch={() => setPalette(true)} />
      </div>
    </div>
  )
}
