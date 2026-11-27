import { useTranslation } from 'react-i18next'
import { useNodes } from '@/stores/nodes'
import { ConfigItemsForm, isLimitsKey, isLimitsKeyBesideTwin } from './ConfigItemsForm'

/**
 * 节点设置 →「限制」（docs/spec/web.md § 4.8，ADR-43）：表单本身是 `ConfigItemsForm`，和「Twin」标签共用。
 * 节点支持 Twin 时，`judge.*` / `twin.*` 归「Twin」标签；不支持的旧 daemon 没有那个标签，这里照旧显示，规则提炼的后端才改得了。
 * 按 agent 的键（`agents.<id>.*`）在「Agent」标签各自的卡片里，不在这里。
 */
export function LimitsTab({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const twin = useNodes((s) => s.byId[nodeId]?.info?.capabilities.twin === true)
  return (
    <div className="space-y-6">
      <p className="text-sm text-fg-muted">{t('nodeSettings.limits.hint')}</p>
      <ConfigItemsForm nodeId={nodeId} include={twin ? isLimitsKeyBesideTwin : isLimitsKey} prefix="limits" />
    </div>
  )
}
