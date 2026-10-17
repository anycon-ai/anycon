import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { AgentTab } from '@/features/nodesettings/AgentTab'
import { DevicesTab } from '@/features/nodesettings/DevicesTab'
import { EnrollTab } from '@/features/nodesettings/EnrollTab'
import { AboutTab, LimitsTab } from '@/features/nodesettings/LimitsAndAbout'
import { WorkspaceTab } from '@/features/nodesettings/WorkspaceTab'
import { useCurrentNode, useNodeParam } from '@/lib/nav'
import { parseSettingsTab, type SettingsTab } from '@/lib/settingsTabs'
import { Tabs } from '@/ui/Misc'

/** `tab`：当前标签（来自 URL）；`review`：从 `enroll_requested` 推送点进来，「接入」标签自动弹出待批准请求的对话框。 */
export function NodeSettingsPage({ tab, review }: { tab?: SettingsTab; review?: boolean }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId, entry } = useCurrentNode()
  const nodeParam = useNodeParam(nodeId)
  if (!nodeId || !entry) return null
  return (
    <>
      <PageHeader crumbs={[{ label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } }, { label: t('node.settings') }]} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl p-4 md:p-6">
          <h1 className="mb-4 text-xl font-semibold">{t('node.settings')}</h1>
          <Tabs
            value={tab ?? 'agent'}
            onValueChange={(v) => void navigate({ to: '/n/$node/settings', params: { node: nodeParam }, search: { tab: parseSettingsTab(v) }, replace: true })}
            items={[
              { value: 'agent', label: t('nodeSettings.tabAgent'), testId: 'tab-agent', content: <AgentTab nodeId={nodeId} /> },
              { value: 'devices', label: t('nodeSettings.tabDevices'), testId: 'tab-devices', content: <DevicesTab nodeId={nodeId} /> },
              { value: 'enroll', label: t('nodeSettings.tabEnroll'), testId: 'tab-enroll', content: <EnrollTab nodeId={nodeId} autoReview={review} /> },
              { value: 'workspace', label: t('nodeSettings.tabWorkspace'), testId: 'tab-workspace', content: <WorkspaceTab nodeId={nodeId} /> },
              { value: 'limits', label: t('nodeSettings.tabLimits'), testId: 'tab-limits', content: <LimitsTab /> },
              { value: 'about', label: t('nodeSettings.tabAbout'), testId: 'tab-about', content: <AboutTab nodeId={nodeId} /> },
            ]}
          />
        </div>
      </div>
    </>
  )
}
