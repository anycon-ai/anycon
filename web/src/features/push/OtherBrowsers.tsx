// 本账号其他浏览器的推送订阅（hub 只给 id、设备、endpoint 的主机名和创建时间），可以删。
import { Laptop } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { useDeletePushSubscription } from '@/api/notifications'
import type { ApiPushSubscription } from '@/api/types'
import { describeError } from '@/lib/errors'
import { formatRelative } from '@/lib/format'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card } from '@/ui/Misc'

export function OtherBrowsers({ rows, selfDeviceId }: { rows: readonly ApiPushSubscription[]; selfDeviceId: string | null }) {
  const { t, i18n } = useTranslation()
  const devices = useDevices()
  const remove = useDeletePushSubscription()
  const others = rows.filter((r) => r.device_id !== selfDeviceId)
  // 设备列表还在加载时先显示 …，别闪一下「未知浏览器」
  const nameOf = (r: ApiPushSubscription): string => r.device_name || devices.data?.find((d) => d.id === r.device_id)?.name || (devices.isPending ? '…' : t('notifications.others.unknownDevice'))

  return (
    <section className="space-y-3" data-testid="other-browsers">
      <h2 className="text-sm font-semibold">{t('notifications.others.title')}</h2>
      {others.length === 0 ? (
        <p className="text-sm text-fg-muted" data-testid="other-browsers-empty">
          {t('notifications.others.empty')}
        </p>
      ) : (
        <div className="space-y-2">
          {others.map((r) => (
            <Card key={r.id} className="flex items-center gap-3 p-3" testId="other-browser-row">
              <Laptop className="size-5 shrink-0 text-fg-subtle" aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium">{nameOf(r)}</p>
                <p className="truncate text-xs text-fg-subtle">
                  {r.endpoint_host} · {t('notifications.others.since', { when: formatRelative(r.created_at_ms, i18n.language) })}
                </p>
              </div>
              <Button
                size="sm"
                variant="outline-danger"
                loading={remove.isPending && remove.variables === r.id}
                onClick={() => remove.mutate(r.id, { onSuccess: () => toast.success(t('notifications.others.removed')), onError: (e) => toast.error(describeError(e, t)) })}
                data-testid="other-browser-remove"
              >
                {t('notifications.others.remove')}
              </Button>
            </Card>
          ))}
        </div>
      )}
    </section>
  )
}
