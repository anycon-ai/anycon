// 「页面在后台时的系统通知」要不要弹，看账号的通知偏好里这个类别的 push 开关（和 hub 决定要不要发 Web Push 是同一个开关）。
// 偏好在 hub 上，读一次缓存 60 秒；在设置页改了开关就丢掉缓存。读不到（hub 不可用、未登录）当作开——宁可多提醒一次，也别漏掉审批。
import { fetchNotificationPrefs } from '@/api/notifications'
import type { NotifyCategory } from '@/protocol'

const TTL_MS = 60_000

let cached: { at: number; push: ReadonlyMap<NotifyCategory, boolean> } | null = null
let inflight: Promise<ReadonlyMap<NotifyCategory, boolean> | null> | null = null

export function forgetSystemNotifyPrefs(): void {
  cached = null
}

async function load(now: number): Promise<ReadonlyMap<NotifyCategory, boolean> | null> {
  if (cached && now - cached.at < TTL_MS) return cached.push
  inflight ??= fetchNotificationPrefs().then(
    (prefs) => {
      const push = new Map(prefs.categories.map((c) => [c.category, c.push] as const))
      cached = { at: now, push }
      return push
    },
    (e: unknown) => {
      console.debug('anycon: could not read the notification preferences; notifying anyway', e)
      return null
    },
  )
  try {
    return await inflight
  } finally {
    inflight = null
  }
}

/** 这个类别的推送开关；偏好里没有这个类别、或读不到偏好，都是开。 */
export async function pushEnabledFor(category: NotifyCategory, now: number = Date.now()): Promise<boolean> {
  const push = await load(now)
  return push?.get(category) ?? true
}
