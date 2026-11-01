// device token 的轮换策略（spec/web.md § 6.4）。纯函数，副作用（发 `rotate_token`、落盘）在 client.ts。
import { TOKEN_MAX_AGE_MS } from '@/lib/constants'

/**
 * token 该轮换了吗？签发时间未知、在「未来」（时钟被往回拨过，不可信）、或已满 `TOKEN_MAX_AGE_MS` 都算。
 * 轮换之后签发时间是此刻，下一次判断就是否了；轮换请求被 hub 忽略时仍是 true，由调用方控制重试节奏。
 */
export function tokenRotationDue(issuedAtMs: number | undefined, nowMs: number): boolean {
  if (issuedAtMs === undefined || !Number.isFinite(issuedAtMs)) return true
  if (issuedAtMs > nowMs) return true
  return nowMs - issuedAtMs >= TOKEN_MAX_AGE_MS
}
