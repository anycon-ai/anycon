import { createFileRoute } from '@tanstack/react-router'
import { PairPage } from '@/pages/PairPage'

export const Route = createFileRoute('/_app/pair')({
  validateSearch: (s: Record<string, unknown>): { code?: string } => ({ code: typeof s.code === 'string' ? s.code : undefined }),
  component: function Pair() {
    const { code } = Route.useSearch()
    return <PairPage code={code} />
  },
})
