// 登录页底部的「继续即表示同意服务条款 / 已阅读隐私政策」（spec/web.md § 4.1）。地址来自 /api/config 的
// `terms_url` / `privacy_url`（hub 的 ANYCON_TERMS_URL / ANYCON_PRIVACY_URL），两个都没配就什么都不显示（自建的默认）。
import type { ReactElement, ReactNode } from 'react'
import { Trans, useTranslation } from 'react-i18next'

/** 新标签页打开文档：不带 opener 和 referrer，文档站拿不到登录页的地址。链接文字由 `Trans` 从文案里的标签填进来。 */
function DocLink({ href, children }: { href: string; children?: ReactNode }): ReactElement {
  return (
    <a href={href} target="_blank" rel="noopener noreferrer" className="text-fg-muted underline underline-offset-2 hover:text-fg">
      {children}
    </a>
  )
}

export function LegalNotice({ termsUrl, privacyUrl }: { termsUrl?: string | null; privacyUrl?: string | null }) {
  const { t } = useTranslation()
  if (!termsUrl && !privacyUrl) return null
  // 只配了一个就只提那一个；文案里的 <terms> / <privacy> 标签换成链接
  const key = termsUrl && privacyUrl ? 'login.legal.both' : termsUrl ? 'login.legal.terms' : 'login.legal.privacy'
  const components: Record<string, ReactElement> = {}
  if (termsUrl) components.terms = <DocLink href={termsUrl} />
  if (privacyUrl) components.privacy = <DocLink href={privacyUrl} />
  return (
    <p className="text-xs leading-relaxed text-fg-subtle" data-testid="login-legal">
      <Trans t={t} i18nKey={key} components={components} />
    </p>
  )
}
