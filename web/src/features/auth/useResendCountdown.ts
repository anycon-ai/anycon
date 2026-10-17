import { useNow } from '@/lib/useNow'

/** hub 对同一个邮箱 60 秒内只发一次码（hub.md § 7.2）。 */
export const RESEND_SECONDS = 60

/** 距离可以重发还剩几秒；`sentAt` 是上一次发码的时间（没发过是 null），`seconds` 默认 60，被 429 限速时用 `Retry-After`。 */
export function useResendCountdown(sentAt: number | null, seconds: number = RESEND_SECONDS): number {
  // `Math.max(now, sentAt)`：刚发完码、`useNow` 还没刷新的那一帧，`now` 可能比 `sentAt` 旧
  const left = (now: number): number => (sentAt === null ? 0 : Math.max(0, Math.ceil((sentAt + seconds * 1000 - Math.max(now, sentAt)) / 1000)))
  const now = useNow(1000, sentAt !== null && left(Date.now()) > 0)
  return left(now)
}
