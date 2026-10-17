// /settings/notifications（spec/web.md § 4.9）：本浏览器的推送、每个类别的 push / email 开关、本账号其他浏览器的订阅。
import { useTranslation } from 'react-i18next'
import { usePushSubscriptions } from '@/api/notifications'
import { CategoriesCard } from '@/features/push/CategoriesCard'
import { OtherBrowsers } from '@/features/push/OtherBrowsers'
import { PushCard } from '@/features/push/PushCard'
import { useWebDeviceId } from '@/features/push/usePush'

export function NotificationsPage() {
  const { t } = useTranslation()
  const deviceId = useWebDeviceId()
  const subs = usePushSubscriptions()
  return (
    <div className="space-y-6">
      <h1 className="text-xl font-semibold">{t('notifications.title')}</h1>
      <p className="text-sm text-fg-muted">{t('notifications.intro')}</p>
      <PushCard />
      <CategoriesCard />
      {subs.data ? <OtherBrowsers rows={subs.data} selfDeviceId={deviceId} /> : null}
    </div>
  )
}
