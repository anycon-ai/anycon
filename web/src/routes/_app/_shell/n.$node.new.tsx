import { createFileRoute } from '@tanstack/react-router'
import { NewSessionPage } from '@/pages/NewSessionPage'

export const Route = createFileRoute('/_app/_shell/n/$node/new')({
  validateSearch: (s: Record<string, unknown>): { project?: string; agent?: string } => ({
    project: typeof s.project === 'string' && s.project ? s.project : undefined,
    agent: typeof s.agent === 'string' && s.agent ? s.agent : undefined,
  }),
  component: function New() {
    const { project, agent } = Route.useSearch()
    return <NewSessionPage project={project} agent={agent} />
  },
})
