import { createFileRoute } from '@tanstack/react-router'
import { SetupPage } from '@/pages/SetupPage'

export const Route = createFileRoute('/_app/setup')({
  validateSearch: (s: Record<string, unknown>): { code?: string } => ({ code: typeof s.code === 'string' ? s.code : undefined }),
  component: function Setup() {
    const { code } = Route.useSearch()
    return <SetupPage code={code} />
  },
})
