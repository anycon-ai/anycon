import { createFileRoute } from '@tanstack/react-router'
import { LoginPage } from '@/pages/LoginPage'

export const Route = createFileRoute('/login')({
  validateSearch: (s: Record<string, unknown>): { redirect?: string } => ({ redirect: typeof s.redirect === 'string' ? s.redirect : undefined }),
  component: function Login() {
    const { redirect } = Route.useSearch()
    return <LoginPage redirect={redirect} />
  },
})
