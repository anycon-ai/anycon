// 退出时清掉推送相关的本地状态（公用 / 借来的电脑上不该留下还能收到你会话通知的订阅）。
//
// - 普通「退出登录」（`dropPushOnSignOut`）：退订浏览器里的推送订阅，并 `DELETE` hub 里这台设备的订阅行；
//   锁屏上不该再出现这个账号的会话标题，下次登录要在通知设置里重新开启。**必须在 `POST /auth/logout` 之前做**——删 hub 里的行要登录态。
//   尽力而为、不能卡住退出：失败只记日志（hub 里剩下的行会在下一次推送得到 404 / 410 时被 hub 删掉），整体最多等几秒。
// - 「退出并移除本设备」（`dropLocalPushState`）：hub 吊销设备时已经删了它那边的订阅；这里再把浏览器里的订阅退掉、把 service worker 用的语言记录删掉。
import { deletePushSubscription, fetchPushSubscriptions } from '@/api/notifications'
import { deleteSwStore } from '@/sw/directory'
import { detectPushSupport, getLocalSubscription } from './browser'

export async function dropLocalPushState(): Promise<void> {
  try {
    if (detectPushSupport().ok) await (await getLocalSubscription(2000))?.unsubscribe()
  } catch (e) {
    console.warn('anycon: could not cancel the push subscription of this browser', e)
  }
  try {
    await deleteSwStore()
  } catch (e) {
    console.warn('anycon: could not delete the notification settings of the service worker', e)
  }
}

/** 退出登录时等推送清理的上限：推送服务或 hub 慢，不能让「退出登录」卡着。 */
export const SIGN_OUT_PUSH_TIMEOUT_MS = 5000

/**
 * 普通退出登录之前：退订本浏览器的推送，并删掉 hub 里这台设备（`deviceId`，没有配对过就是 null）的订阅行。
 * 两件事互不依赖、同时做：hub 那一行才是让 hub 不再推的，浏览器那一侧慢（推送服务不可达）也不拖它的后腿。
 */
export async function dropPushOnSignOut(deviceId: string | null, timeoutMs: number = SIGN_OUT_PUSH_TIMEOUT_MS): Promise<void> {
  if (!detectPushSupport().ok) return
  const browser = (async (): Promise<void> => {
    try {
      await (await getLocalSubscription(2000))?.unsubscribe()
    } catch (e) {
      console.warn('anycon: could not cancel the push subscription of this browser', e)
    }
  })()
  const hub = (async (): Promise<void> => {
    if (!deviceId) return
    try {
      const rows = await fetchPushSubscriptions()
      for (const row of rows.filter((r) => r.device_id === deviceId)) await deletePushSubscription(row.id)
    } catch (e) {
      console.warn('anycon: could not delete this browser’s push subscription at the hub (it drops it on the next push)', e)
    }
  })()
  let timer: ReturnType<typeof setTimeout> | undefined
  const giveUp = new Promise<void>((resolve) => {
    timer = setTimeout(() => {
      console.warn('anycon: push cleanup is taking too long; signing out anyway')
      resolve()
    }, timeoutMs)
  })
  try {
    await Promise.race([Promise.all([browser, hub]).then(() => undefined), giveUp])
  } finally {
    clearTimeout(timer)
  }
}
