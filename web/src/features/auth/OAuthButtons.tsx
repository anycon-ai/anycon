// GitHub / Google 登录入口。点击是整页导航到 hub 的 `/auth/oauth/<p>/start`（授权码流程在 hub 里走完，见 lib/oauth.ts），不是 fetch。
// 回调只到得了 `public_url`：从别的 origin（局域网 http 等）打开时按钮禁用，悬停说明要去哪个地址。
import type { ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import type { OAuthProvider } from '@/api/types'
import { oauthStartUrl, onPublicOrigin, providerLabel } from '@/lib/oauth'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'

/** 单色的品牌标记（用 `currentColor`，跟着主题走；不在组件里写色值）。 */
export function ProviderIcon({ provider, className = 'size-4' }: { provider: OAuthProvider; className?: string }) {
  return provider === 'github' ? (
    <svg viewBox="0 0 16 16" aria-hidden className={className} fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
    </svg>
  ) : (
    <svg viewBox="0 0 24 24" aria-hidden className={className} fill="currentColor">
      <path d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92c-.26 1.37-1.04 2.53-2.21 3.31v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.09z" />
      <path d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z" />
      <path d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18C1.43 8.55 1 10.22 1 12s.43 3.45 1.18 4.93l2.85-2.22.81-.62z" />
      <path d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z" />
    </svg>
  )
}

const linkBase = 'inline-flex shrink-0 select-none items-center justify-center rounded-md border border-border-strong font-medium whitespace-nowrap text-fg transition-colors hover:bg-bg-hover'

/**
 * 能点就是 `<a href>`（普通导航，可以新标签页打开），不能点就是禁用的按钮 + 悬停说明。
 * 禁用按钮收不到指针事件，所以说明挂在外面的 `span` 上。
 */
export function OAuthLink({ href, enabled, disabledReason, size = 'md', full, testId, describedBy, children }: { href: string; enabled: boolean; disabledReason?: string; size?: 'sm' | 'md'; full?: boolean; testId?: string; describedBy?: string; children: ReactNode }) {
  if (enabled)
    return (
      <a href={href} data-testid={testId} className={cn(linkBase, size === 'sm' ? 'h-8 gap-1.5 px-3 text-[0.8125rem]' : 'h-9 gap-2 px-4 text-sm', full && 'w-full')}>
        {children}
      </a>
    )
  return (
    <span title={disabledReason} className={cn('inline-flex', full && 'w-full')}>
      <Button variant="outline" size={size} disabled aria-describedby={describedBy} className={full ? 'w-full justify-center' : undefined} data-testid={testId}>
        {children}
      </Button>
    </span>
  )
}

/** 登录页和邀请页的一排「用 GitHub / Google 继续」。 */
export function OAuthButtons({ providers, publicUrl, next, invite }: { providers: OAuthProvider[]; publicUrl?: string; next?: string; invite?: string | null }) {
  const { t } = useTranslation()
  if (providers.length === 0) return null
  const enabled = onPublicOrigin(publicUrl)
  const noteId = 'oauth-origin-note'
  return (
    <div className="space-y-2" data-testid="oauth-buttons">
      {providers.map((p) => (
        <OAuthLink
          key={p}
          href={oauthStartUrl(p, { next, invite: invite ?? undefined })}
          enabled={enabled}
          disabledReason={t('oauth.wrongOrigin', { provider: providerLabel(t, p), url: publicUrl })}
          describedBy={noteId}
          full
          testId={`oauth-${p}`}
        >
          <ProviderIcon provider={p} />
          {t('oauth.continueWith', { provider: providerLabel(t, p) })}
        </OAuthLink>
      ))}
      {!enabled ? (
        <p id={noteId} className="text-xs text-fg-subtle" data-testid="oauth-origin-note">
          {t('oauth.wrongOriginNote', { url: publicUrl })}
        </p>
      ) : null}
    </div>
  )
}
