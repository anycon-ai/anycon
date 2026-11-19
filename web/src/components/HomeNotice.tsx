import { useTranslation } from 'react-i18next'
import { useIsWide } from '@/lib/useMediaQuery'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'

/**
 * 升级之后第一次打开：首页换成了「待处理」，原来的节点首页在哪里。只给升级上来的老用户显示一次（偏好 `homeNoticeSeen`，
 * 新用户第一次启动时就记为看过），关掉（按钮、×、Esc）都算看过。
 */
export function HomeNotice() {
  const { t } = useTranslation()
  const seen = usePrefs((s) => s.homeNoticeSeen)
  const setPrefs = usePrefs((s) => s.set)
  const hasNodes = useNodes((s) => s.order.length > 0)
  const wide = useIsWide()
  const open = !seen && hasNodes
  const dismiss = (): void => setPrefs({ homeNoticeSeen: true })
  return (
    <Dialog open={open} onOpenChange={(o) => !o && dismiss()} title={t('homeNotice.title')} description={t('homeNotice.body')} closeLabel={t('common.close')} testId="home-notice">
      <div className="space-y-4">
        <p className="text-sm text-fg-muted">{wide ? t('homeNotice.whereDesktop') : t('homeNotice.wherePhone')}</p>
        <div className="flex justify-end">
          <Button variant="primary" onClick={dismiss} data-testid="home-notice-ok">
            {t('homeNotice.ok')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
