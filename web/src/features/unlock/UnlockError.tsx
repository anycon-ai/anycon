import { ShieldAlert } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { formatCountdown } from '@/lib/expiry'
import { useNow } from '@/lib/useNow'
import type { EnrollError } from '@/transport/enroll'
import { Button } from '@/ui/Button'
import { isRestartable, unlockErrorKey, type UnlockEntry } from './errors'

/** 退避期还剩多久（毫秒）；不是 `backoff` 或已经过了返回 0。 */
export function backoffLeft(error: EnrollError | null, errorAt: number | null, now: number): number {
  if (!error || error.code !== 'backoff' || errorAt === null) return 0
  return Math.max(0, errorAt + (error.retryAfterMs ?? 0) - now)
}

/** 解锁失败的说明（spec/web.md § 4.3.1 的失败分支）。`onRestart`：能「重新开始」的原因（超时 / 过期 / 被拒 / 码用完）带一个按钮。 */
export function UnlockError({ error, errorAt, entry, onRestart }: { error: EnrollError; errorAt: number | null; entry: UnlockEntry; onRestart?: () => void }) {
  const { t } = useTranslation()
  const now = useNow(1000, error.code === 'backoff')
  const left = backoffLeft(error, errorAt, now)
  const exhausted = error.code === 'wrong_secret' && error.attemptsLeft === 0 && entry !== 'password' && entry !== 'scan'
  let text: string
  if (error.code === 'backoff') text = left > 0 ? t('unlock.errors.backoff', { time: formatCountdown(left) }) : t('unlock.errors.backoffOver')
  else text = t(unlockErrorKey(error, entry))
  const restart = onRestart && (isRestartable(error.code) || exhausted)
  return (
    <div className="space-y-2 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" role="alert" data-testid="unlock-error" data-code={error.code}>
      <p className="flex items-start gap-2">
        <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
        <span>{text}</span>
      </p>
      {error.code === 'locked' ? <code className="block rounded bg-bg-muted px-2 py-1 font-mono text-xs text-fg">anycon passwd --enable</code> : null}
      {restart ? (
        <Button size="sm" variant="outline" onClick={onRestart} data-testid="unlock-restart">
          {t('unlock.restart')}
        </Button>
      ) : null}
    </div>
  )
}
