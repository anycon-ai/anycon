import { createFileRoute } from '@tanstack/react-router'
import { LoginPage } from '@/pages/LoginPage'

type LoginSearch = { redirect?: string; oauth_error?: string; provider?: string }

const str = (v: unknown): string | undefined => (typeof v === 'string' && v !== '' ? v : undefined)

export const Route = createFileRoute('/login')({
  // `oauth_error` / `provider`：hub 的 OAuth 回调失败后重定向回登录页时带的
  validateSearch: (s: Record<string, unknown>): LoginSearch => ({ redirect: str(s.redirect), oauth_error: str(s.oauth_error), provider: str(s.provider) }),
  component: function Login() {
    const { redirect, oauth_error, provider } = Route.useSearch()
    return <LoginPage redirect={redirect} oauthError={oauth_error} provider={provider} />
  },
})
