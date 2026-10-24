import { createFileRoute } from '@tanstack/react-router'
import { LoginMethodsPage } from '@/pages/settings/LoginMethodsPage'

type LoginMethodsSearch = { linked?: string; oauth_error?: string; provider?: string }

const str = (v: unknown): string | undefined => (typeof v === 'string' && v !== '' ? v : undefined)

export const Route = createFileRoute('/_app/_shell/settings/login')({
  // hub 的 OAuth 关联回调重定向回来时带 `?linked=<p>` 或 `?oauth_error=<code>&provider=<p>`（页面处理完会把它们去掉）
  validateSearch: (s: Record<string, unknown>): LoginMethodsSearch => ({ linked: str(s.linked), oauth_error: str(s.oauth_error), provider: str(s.provider) }),
  component: function LoginMethods() {
    const { linked, oauth_error, provider } = Route.useSearch()
    return <LoginMethodsPage linked={linked} oauthError={oauth_error} provider={provider} />
  },
})
