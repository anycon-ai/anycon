/** 接入会话、接入码、批准码都是 5 分钟。 */
export const ENROLL_WINDOW_MS = 5 * 60_000

/**
 * hub / daemon 给的 `expires_at_ms` 是对方的时钟，本机时钟可能有偏差（手机和服务器差几分钟很常见）。
 * 倒计时只信「剩余时间落在 (0, max + 1 分钟]」的值；落在这个区间之外说明时钟对不上，按整个有效期（`maxMs`）从现在算。
 * 返回本机时钟下的到期时刻。
 */
export function localExpiry(expiresAtMs: number | null | undefined, now: number, maxMs: number = ENROLL_WINDOW_MS): number {
  if (expiresAtMs == null) return now + maxMs
  const left = expiresAtMs - now
  return left > 0 && left <= maxMs + 60_000 ? expiresAtMs : now + maxMs
}

/** 剩余毫秒 → `m:ss`。 */
export function formatCountdown(ms: number): string {
  const s = Math.max(0, Math.ceil(ms / 1000))
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
}
