// 预览相关错误的说明（docs/protocol.md § 5.7、docs/spec/hub.md § 13.3）。
// daemon 在 `data.reason` 里给稳定的原因，hub 在错误体的 `code` 里；都按原因查专门的说法，没有再退回通用说法。
import type { TFunction } from 'i18next'
import { ApiError } from '@/api/client'
import { describeError } from '@/lib/errors'
import { RpcCallError } from '@/transport/rpcClient'
import { PreviewLocalError } from './ticket'

/** `POST /api/preview/tokens` 的错误码（hub.md § 13.3）。 */
export const PREVIEW_HUB_CODES = ['preview_disabled', 'invalid_label', 'label_taken', 'too_many_labels', 'not_found'] as const

/** `forbidden` 的 `data.reason`（protocol.md § 5.7）。 */
export const PREVIEW_DAEMON_REASONS = ['port_not_allowed', 'disabled'] as const

export function describePreviewError(e: unknown, t: TFunction): string {
  if (e instanceof PreviewLocalError) return t(`preview.errors.local.${e.code}`)
  if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'forbidden') {
    const reason = (PREVIEW_DAEMON_REASONS as readonly string[]).find((r) => r === e.reason)
    if (reason) return t(`preview.errors.${reason}`)
  }
  if (e instanceof ApiError) {
    if (e.status === 401) return t('preview.errors.unauthorized')
    const code = (PREVIEW_HUB_CODES as readonly string[]).find((c) => c === e.code)
    if (code) return t(`preview.errors.hub.${code}`)
  }
  return describeError(e, t)
}
