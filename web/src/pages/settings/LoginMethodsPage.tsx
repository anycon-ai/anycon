import { useEffect, useRef, useState } from 'react'
import { Link, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { AlertTriangle, Plus, Trash2, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { accountKeys, useIdentities, useUnlinkIdentity } from '@/api/accounts'
import { keys, useConfig, useCreateToken, useDeleteToken, useMe, useTokens } from '@/api/queries'
import type { OAuthProvider } from '@/api/types'
import { OAuthLink, ProviderIcon } from '@/features/auth/OAuthButtons'
import { describeError } from '@/lib/errors'
import { formatDateTime, formatRelative } from '@/lib/format'
import { configuredProviders, isMultiMode, isOAuthProvider, oauthErrorText, oauthStartUrl, onPublicOrigin, providerLabel } from '@/lib/oauth'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Card, CopyButton, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

/**
 * 登录方式（spec/web.md § 4.9）：已关联的 GitHub / Google（关联 / 解除），以及 `single` 里的访问 token。
 * 账号只剩一种登录方式时那一项不能移除，按钮禁用并说明；这个判断用 `/api/me.login_methods`（hub 给的，已按配置过滤）。
 * `linked` / `oauthError` / `provider`：OAuth 关联回调重定向带回来的 `?linked=` / `?oauth_error=&provider=`，处理完就从地址栏去掉。
 */
export function LoginMethodsPage({ linked, oauthError, provider }: { linked?: string; oauthError?: string; provider?: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const qc = useQueryClient()
  const config = useConfig()
  const me = useMe()
  const [notice, setNotice] = useState<string | null>(null)

  // 回调的结果只处理一次（StrictMode 会跑两遍 effect），然后把查询串去掉：刷新页面不再重复提示
  const handled = useRef('')
  useEffect(() => {
    if (!linked && !oauthError) return
    const sig = `${linked ?? ''}|${oauthError ?? ''}|${provider ?? ''}`
    if (handled.current === sig) return
    handled.current = sig
    if (linked && isOAuthProvider(linked)) {
      toast.success(t('settings.identityLinkedToast', { provider: providerLabel(t, linked) }))
      void qc.invalidateQueries({ queryKey: accountKeys.identities })
      void qc.invalidateQueries({ queryKey: keys.me })
    }
    if (oauthError) setNotice(oauthErrorText(t, oauthError, provider))
    void navigate({ to: '/settings/login', search: {}, replace: true })
  }, [linked, oauthError, provider, t, qc, navigate])

  // 配置没到之前不知道有没有 token / 哪些提供方
  const cfg = config.data
  if (!cfg)
    return (
      <div className="space-y-6">
        <h1 className="text-xl font-semibold">{t('settings.login')}</h1>
        {notice ? <Notice text={notice} onDismiss={() => setNotice(null)} /> : null}
        <Skeleton className="h-14" />
      </div>
    )
  const providers = configuredProviders(cfg.login_methods)
  const tokensEnabled = !isMultiMode(cfg)
  return (
    <div className="space-y-6">
      <h1 className="text-xl font-semibold">{t('settings.login')}</h1>
      {notice ? <Notice text={notice} onDismiss={() => setNotice(null)} /> : null}
      {providers.length > 0 ? <LinkedAccounts providers={providers} publicUrl={cfg.public_url} methods={me.data?.login_methods} methodsKnown={me.data !== undefined} /> : null}
      {tokensEnabled ? <TokensSection methods={me.data?.login_methods} methodsKnown={me.data !== undefined} /> : null}
      {!tokensEnabled && providers.length === 0 ? (
        <p className="text-sm text-fg-muted" data-testid="login-email-only">
          {t('settings.loginEmailOnly')}{' '}
          <Link to="/settings" className="text-accent-text hover:underline">
            {t('settings.profile')}
          </Link>
        </p>
      ) : null}
    </div>
  )
}

function Notice({ text, onDismiss }: { text: string; onDismiss: () => void }) {
  const { t } = useTranslation()
  return (
    <div role="alert" className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" data-testid="login-methods-notice">
      <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
      <span className="min-w-0 flex-1">{text}</span>
      <button type="button" onClick={onDismiss} aria-label={t('common.dismiss')} className="rounded p-0.5 hover:bg-danger/10">
        <X className="size-4" />
      </button>
    </div>
  )
}

/** 配置了的 GitHub / Google：已关联的可以解除，没关联的跳去 hub 的授权流程（`link=1`，回调会回到本页并带 `?linked=`）。 */
function LinkedAccounts({ providers, publicUrl, methods, methodsKnown }: { providers: OAuthProvider[]; publicUrl?: string; methods?: string[]; methodsKnown: boolean }) {
  const { t, i18n } = useTranslation()
  const identities = useIdentities()
  const unlink = useUnlinkIdentity()
  const [pending, setPending] = useState<OAuthProvider | null>(null)
  const originOk = onPublicOrigin(publicUrl)
  const only = methods?.length === 1 ? methods[0] : null

  return (
    <section data-testid="linked-accounts">
      <SectionTitle>{t('settings.linkedAccounts')}</SectionTitle>
      <p className="mb-3 text-sm text-fg-muted">{t('settings.linkedAccountsHint')}</p>
      {identities.isLoading ? <Skeleton className="h-14" /> : null}
      {identities.isError ? <p className="text-sm text-danger">{describeError(identities.error, t)}</p> : null}
      <ul className="space-y-2">
        {identities.data
          ? providers.map((p) => {
              const row = identities.data.find((i) => i.provider === p)
              // 这是账号唯一的登录方式：解除之后就进不来了。me 还没到时先禁用
              const lastOne = row !== undefined && (!methodsKnown || only === p)
              const label = providerLabel(t, p)
              return (
                <li key={p} data-testid={`identity-${p}`} data-linked={row ? 'true' : 'false'}>
                  <Card className="flex items-center gap-3 p-3">
                    <ProviderIcon provider={p} className="size-5 shrink-0" />
                    <div className="min-w-0 flex-1">
                      <p className="truncate font-medium">{label}</p>
                      <p className="text-xs text-fg-subtle">{row ? t('settings.identityLinkedSince', { when: formatDateTime(row.created_at_ms, i18n.language) }) : t('settings.identityNotLinked')}</p>
                    </div>
                    {row ? (
                      <Button
                        size="sm"
                        variant="outline-danger"
                        loading={pending === p && unlink.isPending}
                        disabled={lastOne || unlink.isPending}
                        title={lastOne ? t('settings.identityLastMethod', { provider: label }) : undefined}
                        data-testid={`identity-unlink-${p}`}
                        onClick={() => {
                          setPending(p)
                          unlink.mutate(p, {
                            onSuccess: () => toast.success(t('settings.identityUnlinkedToast', { provider: label })),
                            onError: (e) => toast.error(describeError(e, t)),
                          })
                        }}
                      >
                        {t('settings.identityUnlink')}
                      </Button>
                    ) : (
                      <OAuthLink href={oauthStartUrl(p, { link: true })} enabled={originOk} disabledReason={t('oauth.wrongOrigin', { provider: label, url: publicUrl })} size="sm" testId={`identity-link-${p}`}>
                        {t('settings.identityLink')}
                      </OAuthLink>
                    )}
                  </Card>
                  {lastOne && methodsKnown ? (
                    <p className="mt-1 text-xs text-fg-subtle" data-testid={`identity-last-${p}`}>
                      {t('settings.identityLastMethod', { provider: label })}
                    </p>
                  ) : null}
                </li>
              )
            })
          : null}
      </ul>
      {!originOk ? (
        <p className="mt-2 text-xs text-fg-subtle" data-testid="oauth-origin-note">
          {t('oauth.wrongOriginNote', { url: publicUrl })}
        </p>
      ) : null}
    </section>
  )
}

/** 访问 token（只有 `single`）。 */
function TokensSection({ methods, methodsKnown }: { methods?: string[]; methodsKnown: boolean }) {
  const { t, i18n } = useTranslation()
  const tokens = useTokens()
  const create = useCreateToken()
  const del = useDeleteToken()
  const [open, setOpen] = useState(false)
  const [label, setLabel] = useState('')
  const [created, setCreated] = useState<string | null>(null)
  // 删到最后一个 token、而 token 又是账号唯一的登录方式时，账号就进不来了。me 还没到时先禁用
  const count = tokens.data?.length ?? 0
  const onlyTokens = (methods?.length ?? 0) > 0 && (methods ?? []).every((m) => m === 'token')
  const lastToken = onlyTokens && count <= 1
  const locked = !methodsKnown || lastToken

  return (
    <>
      <section data-testid="tokens-section">
        <SectionTitle
          action={
            <Button size="sm" variant="primary" icon={<Plus className="size-4" />} onClick={() => { setLabel(''); setCreated(null); setOpen(true) }} data-testid="token-new">
              {t('settings.tokenNew')}
            </Button>
          }
        >
          {t('settings.tokens')}
        </SectionTitle>
        <p className="mb-3 text-sm text-fg-muted">{t('settings.tokensHint')}</p>
        {tokens.isLoading ? <Skeleton className="h-14" /> : null}
        <ul className="space-y-2" data-testid="token-list">
          {(tokens.data ?? []).map((tok) => (
            <li key={tok.id}>
              <Card className="flex items-center gap-3 p-3">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-medium">{tok.label || tok.id}</p>
                  <p className="text-xs text-fg-subtle">
                    {t('settings.tokenCreated', { when: formatDateTime(tok.created_at_ms, i18n.language) })}
                    {tok.last_used_ms ? ` · ${t('settings.tokenUsed', { when: formatRelative(tok.last_used_ms, i18n.language) })}` : ` · ${t('settings.tokenNeverUsed')}`}
                  </p>
                </div>
                <Button
                  size="sm"
                  variant="outline-danger"
                  icon={<Trash2 className="size-4" />}
                  disabled={locked}
                  title={lastToken ? t('settings.lastMethod') : undefined}
                  onClick={() => del.mutate(tok.id, { onError: (e) => toast.error(describeError(e, t)) })}
                  aria-label={t('settings.tokenDelete')}
                />
              </Card>
            </li>
          ))}
        </ul>
        {lastToken && count === 1 ? <p className="mt-2 text-xs text-fg-subtle">{t('settings.lastMethod')}</p> : null}
      </section>

      <Dialog open={open} onOpenChange={setOpen} title={created ? t('settings.tokenCreatedTitle') : t('settings.tokenNew')} closeLabel={t('common.close')}>
        {created ? (
          <div className="space-y-3" data-testid="token-created">
            <p className="text-sm text-warning">{t('settings.tokenOnce')}</p>
            <code className="block break-all rounded-md bg-bg-muted px-3 py-2 font-mono text-sm" data-testid="token-value">
              {created}
            </code>
            <div className="flex justify-between">
              <CopyButton text={created} />
              <Button variant="primary" onClick={() => setOpen(false)}>
                {t('common.done')}
              </Button>
            </div>
          </div>
        ) : (
          <form
            className="space-y-4"
            onSubmit={(e) => {
              e.preventDefault()
              create.mutate(label.trim() || 'token', {
                onSuccess: (r) => setCreated(r.token),
                onError: (err) => toast.error(describeError(err, t)),
              })
            }}
          >
            <Field label={t('settings.tokenLabel')} htmlFor="token-label">
              <Input id="token-label" value={label} onChange={(e) => setLabel(e.target.value)} placeholder={t('settings.tokenLabelPlaceholder')} autoFocus maxLength={80} />
            </Field>
            <div className="flex justify-end">
              <Button type="submit" variant="primary" loading={create.isPending}>
                {t('settings.tokenCreate')}
              </Button>
            </div>
          </form>
        )}
      </Dialog>
    </>
  )
}
