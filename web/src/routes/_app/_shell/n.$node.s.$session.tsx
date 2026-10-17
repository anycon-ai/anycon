import { createFileRoute } from '@tanstack/react-router'
import { SessionPage } from '@/pages/SessionPage'

export const Route = createFileRoute('/_app/_shell/n/$node/s/$session')({
  component: function Session() {
    const { session } = Route.useParams()
    return <SessionPage key={session} sessionId={session} />
  },
})
