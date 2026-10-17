import { createFileRoute } from '@tanstack/react-router'
import { ProjectPage } from '@/pages/ProjectPage'

export const Route = createFileRoute('/_app/_shell/n/$node/p/$project')({
  component: function Project() {
    const { project } = Route.useParams()
    return <ProjectPage project={project} />
  },
})
