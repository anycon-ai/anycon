import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'

/**
 * 普通「退出登录」的确认框。它只结束 hub 的登录会话并清内存：这个浏览器里的设备 token、Noise 私钥和明文会话缓存都还在
 * （同一个浏览器再登录仍是同一台设备）。所以先把这件事讲清楚，共用电脑的人可以转去「退出并移除本设备」。
 */
export function SignOutDialog({ open, onOpenChange, onConfirm, onRemoveInstead }: { open: boolean; onOpenChange: (v: boolean) => void; onConfirm: () => Promise<void>; onRemoveInstead: () => void }) {
  const { t } = useTranslation()
  const [busy, setBusy] = useState(false)
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)} title={t('signOut.title')} description={t('signOut.body')} closeLabel={t('common.close')} testId="logout-dialog">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Button variant="ghost" size="sm" disabled={busy} onClick={onRemoveInstead} data-testid="logout-remove-instead">
          {t('signOut.removeInstead')}
        </Button>
        <div className="flex gap-2">
          <Button variant="outline" disabled={busy} onClick={() => onOpenChange(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="primary"
            loading={busy}
            onClick={() => {
              setBusy(true)
              void onConfirm().finally(() => setBusy(false))
            }}
            data-testid="logout-confirm"
          >
            {t('sidebar.signOut')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
