import { createFileRoute } from '@tanstack/react-router'
import { TwinPage } from '@/pages/TwinPage'
export const Route = createFileRoute('/_app/_shell/twin/')({
  validateSearch: (s: Record<string, unknown>): { node?: string; run?: string } => ({
    node: typeof s.node === 'string' ? s.node : undefined,
    run: typeof s.run === 'string' ? s.run : undefined,
  }),
  component: function TwinRoute() {
    const { node, run } = Route.useSearch()
    return <TwinPage node={node} run={run} />
  },
})
