import { createFileRoute } from '@tanstack/react-router'
import { NodeGuard } from '@/components/NodeGuard'

export const Route = createFileRoute('/_app/_shell/n/$node')({
  component: NodeGuard,
})
