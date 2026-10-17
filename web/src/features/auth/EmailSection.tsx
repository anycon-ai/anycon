// 账号设置里的邮箱（spec/web.md § 4.9，ADR-37）：绑定 / 更换（发码 → 输入 6 位码）、解绑。
// 绑定这一步同时证明邮箱归属：只有绑定过的邮箱才能用来登录（single 模式不因为邮箱创建账号）。
import { useRef, useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { ApiError } from '@/api/client'
import { normalizeEmail, useMe, useRequestEmailCode, useUnbindEmail, useVerifyEmail } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { rateLimitedText } from './rateLimit'
import { isPlausibleEmail } from '@/lib/email'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { CodeInput } from '@/features/unlock/CodeInput'
import { CODE_LENGTH } from './code'
import { RESEND_SECONDS, useResendCountdown } from './useResendCountdown'

type Flow = { step: 'idle' } | { step: 'email' } | { step: 'code'; email: string; sentAt: number; cooldown: number }

export function EmailSection() {
  const { t } = useTranslation()
  const me = useMe()
  const requestCode = useRequestEmailCode()
  const verify = useVerifyEmail()
  const unbind = useUnbindEmail()
  const [flow, setFlow] = useState<Flow>({ step: 'idle' })
  const [input, setInput] = useState('')
  const [code, setCode] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [confirmUnbind, setConfirmUnbind] = useState(false)
  const submitting = useRef(false)
  const left = useResendCountdown(flow.step === 'code' ? flow.sentAt : null, flow.step === 'code' ? flow.cooldown : RESEND_SECONDS)

  const current = me.data?.email ?? null
  // 这是不是账号最后一种登录方式：除了邮箱之外一种都没有
  const methods = me.data?.login_methods ?? []
  const onlyEmail = current !== null && methods.length > 0 && methods.every((m) => m === 'email')

  const explain = (e: unknown): string => (e instanceof ApiError && e.status === 429 ? rateLimitedText(t, e.retryAfter ?? 60) : describeError(e, t))

  const cancel = (): void => {
    setFlow({ step: 'idle' })
    setInput('')
    setCode('')
    setError(null)
  }

  const address = normalizeEmail(input)
  const unchanged = current !== null && address === normalizeEmail(current)

  const send = (to: string): void => {
    if (!isPlausibleEmail(to) || submitting.current) return
    submitting.current = true
    setError(null)
    requestCode.mutate(to, {
      onSuccess: (wait) => {
        setFlow({ step: 'code', email: to, sentAt: Date.now(), cooldown: wait })
        setCode('')
      },
      onError: (e) => {
        setError(explain(e))
        if (e instanceof ApiError && e.status === 429 && flow.step === 'code') setFlow({ ...flow, sentAt: Date.now(), cooldown: e.retryAfter ?? 60 })
      },
      onSettled: () => {
        submitting.current = false
      },
    })
  }

  const check = (digits: string): void => {
    if (flow.step !== 'code' || digits.length !== CODE_LENGTH || submitting.current) return
    submitting.current = true
    setError(null)
    verify.mutate(
      { email: flow.email, code: digits },
      {
        onSuccess: () => {
          cancel()
          toast.success(t('settings.emailLinked'))
        },
        onError: (e) => {
          setCode('')
          setError(e instanceof ApiError && (e.code === 'invalid_code' || e.status === 401) ? t('login.codeInvalid') : explain(e))
        },
        onSettled: () => {
          submitting.current = false
        },
      },
    )
  }

  const doUnbind = (): void => {
    unbind.mutate(undefined, {
      onSuccess: () => {
        setConfirmUnbind(false)
        toast.success(t('settings.emailUnlinked'))
      },
      onError: (e) => {
        setConfirmUnbind(false)
        toast.error(describeError(e, t))
      },
    })
  }

  return (
    <div className="space-y-2" data-testid="email-section">
      <span className="block text-[0.8125rem] font-medium text-fg">{t('settings.email')}</span>

      {flow.step === 'idle' ? (
        current ? (
          <div className="space-y-2">
            <p className="text-sm break-all" data-testid="email-current">
              {current}
            </p>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" variant="outline" onClick={() => setFlow({ step: 'email' })} data-testid="email-change">
                {t('settings.emailChange')}
              </Button>
              <Button size="sm" variant="outline-danger" disabled={onlyEmail} onClick={() => setConfirmUnbind(true)} data-testid="email-unlink">
                {t('settings.emailUnlink')}
              </Button>
            </div>
            {onlyEmail ? (
              <p className="text-xs text-fg-subtle" data-testid="email-last-method">
                {t('settings.emailLastMethod')}
              </p>
            ) : null}
          </div>
        ) : (
          <div className="space-y-2">
            <p className="text-xs text-fg-subtle">{t('settings.emailIntro')}</p>
            <Button size="sm" variant="outline" onClick={() => setFlow({ step: 'email' })} data-testid="email-link">
              {t('settings.emailLink')}
            </Button>
          </div>
        )
      ) : null}

      {flow.step === 'email' ? (
        <form
          className="space-y-2"
          onSubmit={(e: FormEvent) => {
            e.preventDefault()
            if (!unchanged) send(address)
          }}
        >
          <Field label={t('settings.emailAddress')} htmlFor="email-input" error={unchanged ? t('settings.emailSame') : error}>
            <Input id="email-input" data-testid="email-input" type="email" inputMode="email" autoComplete="email" autoCapitalize="off" autoCorrect="off" spellCheck={false} autoFocus value={input} onChange={(e) => setInput(e.target.value)} placeholder={t('login.emailPlaceholder')} />
          </Field>
          <div className="flex gap-2">
            <Button type="submit" size="sm" variant="primary" loading={requestCode.isPending} disabled={!isPlausibleEmail(address) || unchanged} data-testid="email-send">
              {t('settings.emailSendCode')}
            </Button>
            <Button size="sm" variant="ghost" onClick={cancel} data-testid="email-cancel">
              {t('common.cancel')}
            </Button>
          </div>
        </form>
      ) : null}

      {flow.step === 'code' ? (
        <form
          className="space-y-2"
          onSubmit={(e: FormEvent) => {
            e.preventDefault()
            check(code)
          }}
        >
          <p role="status" className="text-sm text-fg-muted" data-testid="email-sent">
            {t('settings.emailSentTo', { email: flow.email })}
          </p>
          <Field label={t('settings.emailCode')} htmlFor="email-code" error={error}>
            <CodeInput id="email-code" label={t('settings.emailCode')} testId="email-code" value={code} onChange={setCode} onComplete={check} disabled={verify.isPending} invalid={error !== null} autoFocus />
          </Field>
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" size="sm" variant="primary" loading={verify.isPending} disabled={code.length !== CODE_LENGTH} data-testid="email-verify">
              {t('settings.emailVerify')}
            </Button>
            <Button size="sm" variant="ghost" disabled={left > 0 || requestCode.isPending} onClick={() => send(flow.email)} data-testid="email-resend" data-seconds={left}>
              {left > 0 ? t('settings.emailResendIn', { seconds: left }) : t('settings.emailResend')}
            </Button>
            <Button size="sm" variant="ghost" onClick={cancel} data-testid="email-cancel">
              {t('common.cancel')}
            </Button>
          </div>
        </form>
      ) : null}

      <Dialog open={confirmUnbind} onOpenChange={setConfirmUnbind} title={t('settings.emailUnlinkTitle', { email: current ?? '' })} description={t('settings.emailUnlinkBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setConfirmUnbind(false)}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={unbind.isPending} onClick={doUnbind} data-testid="email-unlink-confirm">
            {t('settings.emailUnlink')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}
