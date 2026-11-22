import { createFileRoute } from '@tanstack/react-router'
import { RulesPage } from '@/pages/RulesPage'

export const Route = createFileRoute('/_app/_shell/rules')({
  validateSearch: (s: Record<string, unknown>): { tab?: 'pending'; node?: string; rule?: string } => ({
    ...(s.tab === 'pending' ? { tab: 'pending' as const } : {}),
    ...(typeof s.node === 'string' && typeof s.rule === 'string' ? { node: s.node, rule: s.rule } : {}),
  }),
  component: function Rules() {
    const { tab, node, rule } = Route.useSearch()
    return <RulesPage initialTab={tab} focus={node && rule ? { node, rule } : undefined} />
  },
})
