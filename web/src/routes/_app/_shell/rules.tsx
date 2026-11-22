import { createFileRoute } from '@tanstack/react-router'
import { RulesPage } from '@/pages/RulesPage'

export const Route = createFileRoute('/_app/_shell/rules')({
  validateSearch: (s: Record<string, unknown>): { tab?: 'pending' } => s.tab === 'pending' ? { tab: 'pending' } : {},
  component: function Rules() {
    const { tab } = Route.useSearch()
    return <RulesPage initialTab={tab} />
  },
})
