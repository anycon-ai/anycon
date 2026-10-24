import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'

/** 配额类失败（`quota`）说明后面的出口：去「账号设置 → 配额」看哪一项满了；释放配额在「设备」页移除不用的设备。 */
export function QuotaLink() {
  const { t } = useTranslation()
  return (
    <Link to="/settings/quota" className="font-medium underline underline-offset-2" data-testid="quota-link">
      {t('quota.open')}
    </Link>
  )
}
