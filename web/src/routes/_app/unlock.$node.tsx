import { createFileRoute } from '@tanstack/react-router'
import { UnlockNodePage } from '@/pages/UnlockNodePage'

export const Route = createFileRoute('/_app/unlock/$node')({
  component: function Unlock() {
    const { node } = Route.useParams()
    return <UnlockNodePage node={node} />
  },
})
