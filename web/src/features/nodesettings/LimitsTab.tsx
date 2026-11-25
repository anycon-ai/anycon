import { useTranslation } from 'react-i18next'
import { useNodes } from '@/stores/nodes'
import { ConfigItemsForm, isNotTwinKey } from './ConfigItemsForm'

/**
 * 节点设置 →「限制」（docs/spec/web.md § 4.8，ADR-43）：表单本身是 `ConfigItemsForm`，和「Bot」标签共用。
 * 节点支持 Bot 时，`judge.*` / `twin.*` 归「Bot」标签；不支持的旧 daemon 没有那个标签，这里照旧显示，规则提炼的后端才改得了。
 */
export function LimitsTab({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const twin = useNodes((s) => s.byId[nodeId]?.info?.capabilities.twin === true)
  return (
    <div className="space-y-6">
      <p className="text-sm text-fg-muted">{t('nodeSettings.limits.hint')}</p>
      <ConfigItemsForm nodeId={nodeId} include={twin ? isNotTwinKey : all} prefix="limits" />
    </div>
  )
}

const all = (): boolean => true
