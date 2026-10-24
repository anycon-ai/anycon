import { createFileRoute } from '@tanstack/react-router'
import { oauthErrorSearch, searchString, type OAuthErrorSearch } from '@/lib/oauth'
import { LoginPage } from '@/pages/LoginPage'

type LoginSearch = { redirect?: string } & OAuthErrorSearch

export const Route = createFileRoute('/login')({
  // `oauth_error` / `provider`：hub 的 OAuth 回调失败后重定向回登录页时带的
  validateSearch: (s: Record<string, unknown>): LoginSearch => ({ redirect: searchString(s.redirect), ...oauthErrorSearch(s) }),
  component: function Login() {
    const { redirect, oauth_error, provider } = Route.useSearch()
    return <LoginPage redirect={redirect} oauthError={oauth_error} provider={provider} />
  },
})
