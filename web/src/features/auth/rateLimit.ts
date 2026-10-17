import type { TFunction } from 'i18next'
import { formatDuration } from '@/lib/format'

/**
 * 429 的提示。两分钟以内按秒说（e2e 的登录 helper 按这句里的秒数等）；更长的——比如邮箱每天的发码上限，
 * hub 的 `Retry-After` 可能是几个小时——换成「3h 59m」这种写法，不显示一个五位数的秒数。
 */
export function rateLimitedText(t: TFunction, seconds: number): string {
  return seconds < 120
    ? t('login.rateLimited', { seconds })
    : t('login.rateLimitedLong', { wait: formatDuration(seconds * 1000) })
}
