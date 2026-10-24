import { createFileRoute } from '@tanstack/react-router'
import { oauthErrorSearch, searchString, type OAuthErrorSearch } from '@/lib/oauth'
import { LoginMethodsPage } from '@/pages/settings/LoginMethodsPage'

type LoginMethodsSearch = { linked?: string } & OAuthErrorSearch

export const Route = createFileRoute('/_app/_shell/settings/login')({
  // hub 的 OAuth 关联回调重定向回来时带 `?linked=<p>` 或 `?oauth_error=<code>&provider=<p>`（页面处理完会把它们去掉）
  validateSearch: (s: Record<string, unknown>): LoginMethodsSearch => ({ linked: searchString(s.linked), ...oauthErrorSearch(s) }),
  component: function LoginMethods() {
    const { linked, oauth_error, provider } = Route.useSearch()
    return <LoginMethodsPage linked={linked} oauthError={oauth_error} provider={provider} />
  },
})
