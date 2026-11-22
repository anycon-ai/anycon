import { createFileRoute } from '@tanstack/react-router'
import { TwinPage } from '@/pages/TwinPage'
export const Route = createFileRoute('/_app/_shell/twin/$node')({
  validateSearch: (s: Record<string, unknown>): { run?: string } => ({ run: typeof s.run === 'string' ? s.run : undefined }),
  component: function TwinNodeRoute() {
    const { node } = Route.useParams()
    const { run } = Route.useSearch()
    return <TwinPage node={node} run={run} />
  },
})
