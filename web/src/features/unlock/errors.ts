// 解锁失败的原因 → 给人看的说明（i18n key）。同一个原因在不同入口里意思不同（`wrong_secret`：码 / 口令；`no_code`：只有接入码入口）。
import type { EnrollError, EnrollErrorCode } from '@/transport/enroll'

export type UnlockEntry = 'scan' | 'approve' | 'code' | 'password'

/** 每个错误码都有说明（`i18n.test.ts` 用 Record 让编译器保证枚举完整）。 */
export const UNLOCK_ERROR_CODES: Record<EnrollErrorCode, true> = {
  hub_unavailable: true,
  hub_no_reply: true,
  invalid_code: true,
  not_found: true,
  timeout: true,
  peer_gone: true,
  rate_limited: true,
  quota: true,
  forbidden: true,
  handshake_failed: true,
  no_identity: true,
  cancelled: true,
  ack_invalid: true,
  key_changed: true,
  offline: true,
  busy: true,
  method_disabled: true,
  password_not_set: true,
  backoff: true,
  locked: true,
  no_code: true,
  denied: true,
  wrong_secret: true,
  expired: true,
  invalid: true,
}

/** 这些原因意味着「这一次接入没有了，重新开始就行」。 */
export function isRestartable(code: EnrollErrorCode): boolean {
  return code === 'timeout' || code === 'expired' || code === 'peer_gone' || code === 'denied' || code === 'cancelled' || code === 'hub_no_reply'
}

/** 二维码到期（hub 的 5 分钟，或 daemon 说过期）。 */
export function isExpiry(code: EnrollErrorCode): boolean {
  return code === 'timeout' || code === 'expired'
}

/**
 * `scan` 入口自己换一个新二维码的情形：到期，或者和 hub 的连接断了（临时连接最长 10 分钟，到点 hub 会断开它，
 * 客户端重连后是一条新的临时连接，旧的接入会话随之作废）。用户没做错什么，不该看到一个错误。
 */
export function restartsByItself(code: EnrollErrorCode): boolean {
  return isExpiry(code) || code === 'hub_unavailable'
}

/** 失败说明的 i18n key。`backoff` 的倒计时、`locked` 的命令由界面另外画。 */
export function unlockErrorKey(err: EnrollError, entry: UnlockEntry): string {
  if (err.code === 'wrong_secret') {
    if (entry === 'password') return 'unlock.errors.wrongPassword'
    if (entry === 'code' || entry === 'approve') return err.attemptsLeft === 0 ? 'unlock.errors.codeExhausted' : 'unlock.errors.wrongCode'
    return 'unlock.errors.wrongScan'
  }
  // 码过期 / 次数用完（daemon 回 `expired`）：对码的两个入口，说的是码而不是请求
  if (err.code === 'expired' && (entry === 'code' || entry === 'approve')) return 'unlock.errors.codeExpired'
  return `unlock.errors.${err.code}`
}

/** 界面会用到的全部 i18n key（`i18n.test.ts` 据此保证两种语言都有）。 */
export function unlockErrorKeys(): string[] {
  const direct = (Object.keys(UNLOCK_ERROR_CODES) as EnrollErrorCode[]).filter((c) => c !== 'wrong_secret').map((c) => `unlock.errors.${c}`)
  return [...direct, 'unlock.errors.wrongCode', 'unlock.errors.codeExhausted', 'unlock.errors.codeExpired', 'unlock.errors.wrongPassword', 'unlock.errors.wrongScan', 'unlock.errors.backoff', 'unlock.errors.backoffOver']
}
