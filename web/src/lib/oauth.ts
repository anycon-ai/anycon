// OAuth 登录（GitHub / Google）的前端辅助：哪些登录方是 OAuth、跳转地址怎么拼、`?oauth_error=` 怎么说、当前 origin 能不能发起。
//
// 登录流程全在 hub 里（授权码 + PKCE + state）：前端只做整页导航到 `/auth/oauth/<p>/start`，
// hub 302 去提供方，回调到 `${public_url}/auth/callback/<p>`，最后 302 回 `next`（或带 `oauth_error` 回 `/login`、`/settings/login`）。
import type { TFunction } from 'i18next'
import type { ApiConfig, OAuthProvider } from '@/api/types'

export const OAUTH_PROVIDERS: readonly OAuthProvider[] = ['github', 'google']

export function isOAuthProvider(v: unknown): v is OAuthProvider {
  return typeof v === 'string' && (OAUTH_PROVIDERS as readonly string[]).includes(v)
}

/** hub 上配置了的 OAuth 登录方，保持 `login_methods` 里的顺序。 */
export function configuredProviders(methods: readonly string[] | undefined): OAuthProvider[] {
  return (methods ?? []).filter(isOAuthProvider)
}

/** `multi`：开放注册、有配额，没有邀请和 token 登录。认不出的模式按 `single` 处理。 */
export function isMultiMode(config: Pick<ApiConfig, 'mode'> | undefined): boolean {
  return config?.mode === 'multi'
}

/**
 * 当前页面是不是在 hub 的公开地址上。OAuth 回调只到得了 `public_url`，别的 origin（局域网 http、IP 直连）发起也走不完，
 * 所以那时按钮要禁用。hub 没给 `public_url`（或不是合法 URL）时没法比较，当作可以。
 */
export function onPublicOrigin(publicUrl: string | undefined, currentOrigin: string = location.origin): boolean {
  if (!publicUrl) return true
  try {
    return new URL(publicUrl).origin === currentOrigin
  } catch {
    return true
  }
}

export interface OAuthStartOptions {
  /** 登录成功后回到哪里（hub 只认以单个 `/` 开头的路径）。 */
  next?: string
  /** `single` 的邀请码：新用户用它在 OAuth 登录时建账号。 */
  invite?: string
  /** 把这个 OAuth 身份关联到当前已登录的账号（设置页用）。 */
  link?: boolean
}

export function oauthStartUrl(provider: OAuthProvider, opts: OAuthStartOptions = {}): string {
  const q = new URLSearchParams()
  if (opts.next && opts.next !== '/') q.set('next', opts.next)
  if (opts.invite) q.set('invite', opts.invite)
  if (opts.link) q.set('link', '1')
  const qs = q.toString()
  return `/auth/oauth/${provider}/start${qs ? `?${qs}` : ''}`
}

/** hub 在 OAuth 失败的重定向里放的 `oauth_error` 码（稳定，用来做 i18n）。 */
export const OAUTH_ERROR_CODES = ['cancelled', 'provider_error', 'invalid_state', 'registration_closed', 'invite_invalid', 'invite_expired', 'invite_used', 'identity_taken', 'already_linked', 'not_signed_in', 'server_error'] as const
export type OAuthErrorCode = (typeof OAUTH_ERROR_CODES)[number]

/** 路由 search 参数里的字符串：非空字符串原样给，别的类型、空串当没有。 */
export function searchString(v: unknown): string | undefined {
  return typeof v === 'string' && v !== '' ? v : undefined
}

/** hub 的 OAuth 回调失败后重定向回来的 `?oauth_error=<code>&provider=<p>`（`/login` 和 `/settings/login` 共用）。 */
export interface OAuthErrorSearch {
  oauth_error?: string
  provider?: string
}

/** 路由 `validateSearch` 里取 OAuth 失败的两个参数；其余参数各路由自己取。 */
export function oauthErrorSearch(s: Record<string, unknown>): OAuthErrorSearch {
  return { oauth_error: searchString(s.oauth_error), provider: searchString(s.provider) }
}

export function providerLabel(t: TFunction, provider: string | undefined): string {
  return isOAuthProvider(provider) ? t(`oauth.provider.${provider}`) : t('oauth.provider.unknown')
}

/** `?oauth_error=<code>&provider=<p>` → 给用户看的一句话；认不出的码给一句通用的。 */
export function oauthErrorText(t: TFunction, code: string, provider: string | undefined): string {
  const known = (OAUTH_ERROR_CODES as readonly string[]).includes(code)
  return t(known ? `oauth.errors.${code}` : 'oauth.errors.generic', { provider: providerLabel(t, provider) })
}
