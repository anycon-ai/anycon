import { createFileRoute } from '@tanstack/react-router'
import { NodeHomePage } from '@/pages/NodeHomePage'

export const Route = createFileRoute('/_app/_shell/n/$node/')({
  component: NodeHomePage,
})
