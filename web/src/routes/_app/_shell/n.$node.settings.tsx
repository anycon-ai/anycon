import { createFileRoute } from '@tanstack/react-router'
import { parseSettingsTab } from '@/lib/settingsTabs'
import { NodeSettingsPage } from '@/pages/NodeSettingsPage'

export const Route = createFileRoute('/_app/_shell/n/$node/settings')({
  // `?tab=enroll`：安全提醒 / 接入请求的推送点进来直达「接入」标签；`review=1`：自动弹出待批准请求
  validateSearch: (s: Record<string, unknown>): { tab?: ReturnType<typeof parseSettingsTab>; review?: boolean } => ({
    tab: parseSettingsTab(s.tab),
    review: s.review === '1' || s.review === 1 || s.review === true ? true : undefined,
  }),
  component: function NodeSettings() {
    const { tab, review } = Route.useSearch()
    return <NodeSettingsPage tab={tab} review={review} />
  },
})
