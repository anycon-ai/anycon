// 「这个浏览器的推送」卡片：状态、开启、关闭、发测试推送（spec/web.md § 4.9）。
import { Bell, BellOff, BellRing } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { Card, Chip, Skeleton, type Tone } from '@/ui/Misc'
import { usePush, type PushStatus } from './usePush'

const TONE: Record<PushStatus, Tone> = { loading: 'neutral', unsupported: 'neutral', unavailable: 'warning', denied: 'danger', off: 'neutral', on: 'success' }

export function PushCard() {
  const { t } = useTranslation()
  const push = usePush()
  const { status, busy } = push

  // 状态下面的一段说明：不能用的原因 / 下一步该做什么
  let note: string | null = null
  if (status === 'unsupported' && !push.support.ok) note = t(`notifications.push.unsupported.${push.support.reason}`)
  else if (status === 'unavailable') note = t('notifications.push.unavailable')
  else if (status === 'denied') note = t('notifications.push.denied')
  else if (status === 'off' && !push.deviceId) note = t('notifications.push.noDevice')

  const Icon = status === 'on' ? BellRing : status === 'off' || status === 'loading' ? Bell : BellOff
  return (
    <Card className="p-4" testId="push-card">
      <div className="flex items-start gap-3">
        <Icon className="mt-0.5 size-5 shrink-0 text-fg-subtle" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-sm font-semibold">{t('notifications.push.title')}</h2>
            <Chip tone={TONE[status]} testId="push-status" dataState={status}>
              {t(`notifications.push.status.${status}`)}
            </Chip>
          </div>
          <p className="mt-1 text-sm text-fg-muted">{t('notifications.push.hint')}</p>
          {status === 'loading' ? <Skeleton className="mt-3 h-9 w-40" /> : null}
          {note ? (
            <p className="mt-3 text-sm text-fg-muted" data-testid="push-note">
              {note}
            </p>
          ) : null}
          <div className="mt-3 flex flex-wrap gap-2">
            {status === 'off' ? (
              <Button variant="primary" loading={busy === 'enable'} disabled={!push.deviceId || busy !== null} onClick={() => void push.enable()} data-testid="push-enable">
                {t('notifications.push.enable')}
              </Button>
            ) : null}
            {status === 'on' ? (
              <>
                <Button variant="outline" loading={busy === 'test'} disabled={busy !== null} onClick={() => void push.test()} data-testid="push-test">
                  {t('notifications.push.test')}
                </Button>
                <Button variant="outline-danger" loading={busy === 'disable'} disabled={busy !== null} onClick={() => void push.disable()} data-testid="push-disable">
                  {t('notifications.push.disable')}
                </Button>
              </>
            ) : null}
          </div>
        </div>
      </div>
    </Card>
  )
}
