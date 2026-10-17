import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { useRouter } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { KeyRound, Ticket } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { ApiError } from '@/api/client'
import { keys, loginWithInvite, loginWithToken, useConfig, useMe } from '@/api/queries'
import { CenterLayout } from '@/components/CenterLayout'
import { EmailLogin } from '@/features/auth/EmailLogin'
import { clearPendingPairCode } from '@/lib/loginRedirect'
import { rateLimitedText } from '@/features/auth/rateLimit'
import { safeRedirect } from '@/lib/nav'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Card, Skeleton } from '@/ui/Misc'

const INVITE_KEY = 'invite'

function readInvite(): string | null {
  try {
    return sessionStorage.getItem(INVITE_KEY)
  } catch {
    return null
  }
}
function clearInvite(): void {
  try {
    sessionStorage.removeItem(INVITE_KEY)
  } catch {
    /* ignore */
  }
}

export function LoginPage({ redirect }: { redirect?: string }) {
  const { t } = useTranslation()
  const router = useRouter()
  const qc = useQueryClient()
  const config = useConfig()
  const me = useMe()
  const [token, setToken] = useState('')
  const [tokenOpen, setTokenOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const invite = readInvite()

  // 登录后回到 redirect（只有路径：配对链接的短码不在 URL 里，由 /pair 自己从 sessionStorage 取，见 loginRedirect.ts）
  const goBack = useCallback((): void => {
    const target = safeRedirect(redirect)
    // 暂存的短码只对 /pair、/setup 有用；去别处就别留着
    if (!/^\/(?:pair|setup)(?:[?#]|$)/.test(target)) clearPendingPairCode()
    router.history.replace(target)
  }, [router, redirect])

  // 已登录（比如在另一个标签页登录过）：直接进去
  useEffect(() => {
    if (me.data) goBack()
  }, [me.data, goBack])

  const methods = config.data?.login_methods ?? []
  const tokenEnabled = methods.includes('token')
  const emailEnabled = methods.includes('email')
  // 两种都开时，邮箱在前、token 折叠在「使用 token」后面（spec/web.md § 4.1）；只开了 token 就直接是 token 表单
  const tokenFolded = tokenEnabled && emailEnabled && !tokenOpen

  const fail = (e: unknown): void => {
    if (e instanceof ApiError) {
      if (e.status === 429) setError(rateLimitedText(t, e.retryAfter ?? 60))
      else if (e.code === 'invite_invalid' || e.code === 'invite_expired' || e.code === 'invite_used') setError(t(`login.${e.code}`))
      else if (e.code === 'invalid_token' || e.status === 401) setError(t('login.invalidToken'))
      else if (e.status === 0) setError(t('login.network'))
      else setError(e.message)
    } else setError(t('login.network'))
  }

  const done = async (): Promise<void> => {
    clearInvite()
    await qc.invalidateQueries({ queryKey: keys.me })
    goBack()
  }

  const submit = async (e: FormEvent): Promise<void> => {
    e.preventDefault()
    if (!token.trim()) return
    setBusy(true)
    setError(null)
    try {
      await loginWithToken(token.trim())
      await done()
    } catch (err) {
      fail(err)
    } finally {
      setBusy(false)
    }
  }

  const acceptInvite = async (): Promise<void> => {
    if (!invite) return
    setBusy(true)
    setError(null)
    try {
      await loginWithInvite(invite)
      await done()
    } catch (err) {
      fail(err)
    } finally {
      setBusy(false)
    }
  }

  return (
    <CenterLayout>
      <Card className="p-5">
        <h1 className="text-lg font-semibold">{t('login.title')}</h1>
        <p className="mt-1 text-sm text-fg-muted">{emailEnabled ? t(tokenEnabled ? 'login.subtitleEmail' : 'login.subtitleEmailOnly') : t('login.subtitle')}</p>

        {config.isLoading ? (
          <div className="mt-5 space-y-3">
            <Skeleton className="h-9" />
            <Skeleton className="h-9" />
          </div>
        ) : config.isError ? (
          <p role="alert" className="mt-5 text-sm text-danger">
            {t('login.configError')}
          </p>
        ) : (
          <div className="mt-5 space-y-5">
            {invite ? (
              <div className="rounded-md border border-accent/40 bg-accent-subtle p-3 text-sm">
                <p className="flex items-center gap-2 font-medium">
                  <Ticket className="size-4" /> {t('login.invited')}
                </p>
                <Button variant="primary" className="mt-3 w-full" loading={busy} onClick={() => void acceptInvite()} data-testid="login-invite-accept">
                  {t('login.acceptInvite')}
                </Button>
              </div>
            ) : null}

            {emailEnabled ? <EmailLogin onSignedIn={done} autoFocus={!invite} /> : null}

            {tokenEnabled && !tokenFolded ? (
              <form onSubmit={(e) => void submit(e)} className="space-y-3">
                <Field label={t('login.tokenLabel')} htmlFor="login-token" hint={t('login.tokenHint')} error={error}>
                  <Input
                    id="login-token"
                    data-testid="login-token-input"
                    type="password"
                    autoComplete="off"
                    autoCapitalize="off"
                    autoCorrect="off"
                    spellCheck={false}
                    autoFocus={!invite && (!emailEnabled || tokenOpen)}
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                    placeholder="anycon_…"
                    aria-invalid={error ? true : undefined}
                  />
                </Field>
                <Button type="submit" variant="primary" className="w-full" loading={busy} disabled={!token.trim()} icon={<KeyRound className="size-4" />} data-testid="login-submit">
                  {t('login.submit')}
                </Button>
              </form>
            ) : null}
            {tokenFolded ? (
              <button type="button" onClick={() => setTokenOpen(true)} className="text-sm text-accent-text underline-offset-2 hover:underline" data-testid="login-use-token">
                {t('login.useToken')}
              </button>
            ) : null}
            {!tokenEnabled && !emailEnabled ? <p className="text-sm text-fg-muted">{t('login.noMethods')}</p> : null}
            {(!tokenEnabled || tokenFolded) && error ? <p className="text-sm text-danger">{error}</p> : null}
          </div>
        )}
      </Card>
    </CenterLayout>
  )
}
