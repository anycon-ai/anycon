/**
 * 设备 token 轮换的策略常量和纯函数（docs/spec/sdk.md § 2，hub 侧见 docs/spec/hub.md § 5）。
 *
 * 轮换的发起和落盘在 `Client`，这里只放「什么时候该换」「多久再试一次」的判断，方便单独测。
 */

/** token 超过这么久（30 天）就该轮换。 */
export const TOKEN_MAX_AGE_MS = 30 * 24 * 3600 * 1000
/**
 * 发出 `rotate_token` 之后没等到 `token_rotated`，多久后再发一次。比 hub 的「同一设备 1 分钟最多轮换一次」窗口长：
 * 第一次的回复丢了而 hub 其实已经轮换过，重试时窗口已经过去，才不会被当作多余的请求忽略。
 */
export const ROTATE_RETRY_MS = 75_000
/** 一条连接上头几次发送（间隔 [`ROTATE_RETRY_MS`]）的次数；之后只按 [`ROTATE_RECHECK_MS`] 的节奏复查。 */
export const ROTATE_QUICK_SENDS = 3
/** 复查间隔：长连接可能连续几十天不重连，不能只在 `auth_ok` 时看一次；hub 一直不回复（旧版本）时也不会刷屏。 */
export const ROTATE_RECHECK_MS = 3600 * 1000
/** `client.rotateToken()` 等 `token_rotated` 的默认时间。 */
export const ROTATE_TIMEOUT_MS = 10_000

/**
 * token 该不该轮换：签发时间未知、签发时间在未来（时钟被拨回过，当作未知）、或已满 `maxAgeMs`。
 * 时间未知的老记录会在下次连上时换一次，之后就有记录了。
 */
export function isTokenDue(
  issuedAtMs: unknown,
  nowMs: number,
  maxAgeMs: number = TOKEN_MAX_AGE_MS,
): boolean {
  if (typeof issuedAtMs !== 'number' || !Number.isFinite(issuedAtMs)) return true
  if (issuedAtMs > nowMs) return true
  return nowMs - issuedAtMs >= maxAgeMs
}

/** 已经发了 `sends` 次请求之后，下一次检查在多久以后。 */
export function nextRotationDelay(
  sends: number,
  retryMs: number = ROTATE_RETRY_MS,
  recheckMs: number = ROTATE_RECHECK_MS,
): number {
  return sends < ROTATE_QUICK_SENDS ? retryMs : recheckMs
}

/**
 * `client.close()` 最多再等多久（从发出 `rotate_token` 起算）让还在路上的 `token_rotated` 落盘：hub 在回复之前就已经换了 token，
 * 刚连上就关的短脚本不等的话，新 token 会丢，旧 token 60 秒后失效。回复很快（一个来回），所以几乎不会真等这么久；
 * hub 不回复（旧版本、或 1 分钟内的第二次请求被忽略）时，只有发出请求后的头 2 秒里调 `close()` 才会多等。
 */
export const ROTATE_CLOSE_WAIT_MS = 2000
