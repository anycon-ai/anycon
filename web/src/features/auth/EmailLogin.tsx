// 登录页的邮箱两步（spec/web.md § 4.1，ADR-37）：输入邮箱 → 发码 → 输入 6 位码（输满自动提交）。
// hub 不告诉我们这个邮箱有没有绑定账号（`/auth/email/send` 总是 200），所以发码后说的是「如果绑定了，已经发出」。
import { useRef, useState, type FormEvent } from 'react'
import { Mail } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { ApiError } from '@/api/client'
import { loginWithEmail, normalizeEmail, sendLoginCode } from '@/api/queries'
import { isPlausibleEmail } from '@/lib/email'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { CodeInput } from '@/features/unlock/CodeInput'
import { CODE_LENGTH } from './code'
import { rateLimitedText } from './rateLimit'
import { RESEND_SECONDS, useResendCountdown } from './useResendCountdown'

/**
 * `signUp`：`multi` 模式。发码给任何有效邮箱，验证通过后没有账号就建一个，所以文案说的是「登录或创建账号」，
 * 不再说「邮箱要先绑定」（`single` 里只有绑定过的邮箱才收得到码）。
 */
export function EmailLogin({ onSignedIn, autoFocus, signUp }: { onSignedIn: () => Promise<void>; autoFocus?: boolean; signUp?: boolean }) {
  const { t } = useTranslation()
  const [email, setEmail] = useState('')
  /** 码发到了哪个邮箱（规范化后的）；null = 还在第一步。 */
  const [sentTo, setSentTo] = useState<string | null>(null)
  const [sentAt, setSentAt] = useState<number | null>(null)
  const [cooldown, setCooldown] = useState(RESEND_SECONDS)
  const [code, setCode] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<{ kind: 'email' | 'code'; text: string } | null>(null)
  // 自动提交和手动点按钮可能同时到：同一时刻只让一次提交在途
  const submitting = useRef(false)
  const left = useResendCountdown(sentAt, cooldown)

  const fail = (kind: 'email' | 'code', e: unknown): void => {
    if (e instanceof ApiError) {
      if (e.status === 429) {
        const seconds = e.retryAfter ?? 60
        setError({ kind, text: rateLimitedText(t, seconds) })
        if (kind === 'email') {
          setSentAt(Date.now())
          setCooldown(seconds)
        }
        return
      }
      if (e.status === 0) return setError({ kind, text: t('login.network') })
      if (e.code === 'invalid_email') return setError({ kind, text: t('login.emailInvalid') })
      if (kind === 'code' && (e.code === 'invalid_code' || e.status === 401)) return setError({ kind, text: t('login.codeInvalid') })
      return setError({ kind, text: e.message })
    }
    setError({ kind, text: t('login.network') })
  }

  const send = async (e?: FormEvent): Promise<void> => {
    e?.preventDefault()
    const address = normalizeEmail(email)
    if (!isPlausibleEmail(address) || submitting.current) return
    submitting.current = true
    setBusy(true)
    setError(null)
    try {
      const wait = await sendLoginCode(address)
      setSentTo(address)
      setSentAt(Date.now())
      setCooldown(wait)
      setCode('')
    } catch (err) {
      fail('email', err)
    } finally {
      submitting.current = false
      setBusy(false)
    }
  }

  const verify = async (digits: string): Promise<void> => {
    if (sentTo === null || digits.length !== CODE_LENGTH || submitting.current) return
    submitting.current = true
    setBusy(true)
    setError(null)
    try {
      await loginWithEmail(sentTo, digits)
      await onSignedIn()
    } catch (err) {
      setCode('')
      fail('code', err)
    } finally {
      submitting.current = false
      setBusy(false)
    }
  }

  if (sentTo === null) {
    return (
      <form onSubmit={(e) => void send(e)} className="space-y-3" data-testid="login-email-form">
        <Field label={t('login.emailLabel')} htmlFor="login-email" hint={t(signUp ? 'login.emailHintSignUp' : 'login.emailHint')} error={error?.kind === 'email' ? error.text : null}>
          <Input
            id="login-email"
            data-testid="login-email-input"
            type="email"
            inputMode="email"
            autoComplete="email"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            autoFocus={autoFocus}
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder={t('login.emailPlaceholder')}
            aria-invalid={error?.kind === 'email' ? true : undefined}
          />
        </Field>
        <Button type="submit" variant="primary" className="w-full" loading={busy} disabled={!isPlausibleEmail(normalizeEmail(email))} icon={<Mail className="size-4" />} data-testid="login-email-send">
          {t('login.emailSend')}
        </Button>
      </form>
    )
  }

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault()
        void verify(code)
      }}
      className="space-y-3"
      data-testid="login-code-form"
    >
      <div role="status" className="rounded-md border border-info/30 bg-info-subtle p-3 text-sm text-info" data-testid="login-email-sent">
        <p>{t(signUp ? 'login.emailSentSignUp' : 'login.emailSent')}</p>
        <p className="mt-1 font-medium break-all">{sentTo}</p>
      </div>
      <Field label={t('login.codeLabel')} htmlFor="login-code" error={error?.kind === 'code' || error?.kind === 'email' ? error.text : null}>
        <CodeInput id="login-code" label={t('login.codeLabel')} testId="login-code-input" value={code} onChange={setCode} onComplete={(d) => void verify(d)} disabled={busy} invalid={error?.kind === 'code'} autoFocus />
      </Field>
      <Button type="submit" variant="primary" className="w-full" loading={busy} disabled={code.length !== CODE_LENGTH} data-testid="login-code-submit">
        {t(signUp ? 'login.emailVerifySignUp' : 'login.emailVerify')}
      </Button>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Button variant="ghost" size="sm" disabled={busy || left > 0} onClick={() => void send()} data-testid="login-code-resend" data-seconds={left}>
          {left > 0 ? t('login.codeResendIn', { seconds: left }) : t('login.codeResend')}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          disabled={busy}
          onClick={() => {
            setSentTo(null)
            setCode('')
            setError(null)
          }}
          data-testid="login-email-change"
        >
          {t('login.emailChange')}
        </Button>
      </div>
    </form>
  )
}
