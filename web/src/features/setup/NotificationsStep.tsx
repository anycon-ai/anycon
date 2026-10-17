import { useEffect } from 'react'
import { Bell } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { usePush } from '@/features/push/usePush'
import { Button } from '@/ui/Button'

/**
 * `/setup` 里设完（或跳过）节点口令之后：问要不要给这个浏览器开通知（spec/web.md § 4.2，ADR-37）。
 * 「开启」直接走通知设置页同一套（权限提示 → 订阅 → 上报 hub），不用再跳页面；可以跳过，之后在 设置 → 通知 里开。
 * 这个浏览器根本收不了推送（http、iOS 没加到主屏……）、hub 没配推送、浏览器已拒绝、或者已经开着：不问，直接往下。
 */
export function NotificationsStep({ onDone }: { onDone: () => void }) {
  const { t } = useTranslation()
  const push = usePush()
  const nothingToAsk = push.status === 'unsupported' || push.status === 'unavailable' || push.status === 'denied' || push.status === 'on'
  useEffect(() => {
    if (nothingToAsk) onDone()
  }, [nothingToAsk, onDone])
  if (nothingToAsk) return null
  // 还在读（配置、浏览器里的订阅）：先别把问题摆出来——读完发现不用问（比如浏览器已拒绝通知）时，问题一闪而过更糟
  if (push.status === 'loading') {
    return (
      <p className="text-sm text-fg-subtle" data-testid="setup-notify-checking">
        {t('notifications.push.status.loading')}
      </p>
    )
  }

  const ready = push.status === 'off' && !!push.deviceId
  return (
    <div className="space-y-4 text-left" data-testid="setup-notify">
      <div className="flex items-center gap-2">
        <Bell className="size-5 text-accent-text" aria-hidden />
        <h1 className="text-lg font-semibold">{t('setup.notifyTitle')}</h1>
      </div>
      <p className="text-sm text-fg-muted">{t('setup.notifyBody')}</p>
      <div className="flex justify-between gap-2">
        <Button variant="ghost" onClick={onDone} disabled={push.busy === 'enable'} data-testid="setup-notify-skip">
          {t('setup.skip')}
        </Button>
        <Button variant="primary" onClick={() => void push.enable()} loading={push.busy === 'enable'} disabled={!ready} data-testid="setup-notify-enable">
          {t('setup.notifyEnable')}
        </Button>
      </div>
    </div>
  )
}
