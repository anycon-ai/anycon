// 开启 / 关闭 / 对账本浏览器的推送订阅（spec/web.md § 4.9，ADR-37）。
// 顺序有讲究：先在浏览器里订阅，成功了才上报给 hub；上报失败就把刚建的订阅退掉，免得浏览器和 hub 对不上。
import type { TFunction } from 'i18next'
import { ApiError } from '@/api/client'
import { deletePushSubscription, fetchPushSubscriptions, registerPushSubscription } from '@/api/notifications'
import type { ApiPushSubscription } from '@/api/types'
import { base64UrlToBytes } from '@/lib/base64'
import { describeError } from '@/lib/errors'
import { endpointHost, getLocalSubscription, getRegistration, toSubscribeBody, usesServerKey } from './browser'

export type PushErrorCode = 'dismissed' | 'denied' | 'no_registration' | 'subscribe_failed' | 'unsubscribe_failed'

/** 浏览器这一侧出的错（权限、service worker、订阅）；hub 那一侧的错仍是 `ApiError`。 */
export class PushError extends Error {
  constructor(
    readonly code: PushErrorCode,
    cause?: unknown,
  ) {
    super(code, { cause })
    this.name = 'PushError'
  }
}

export function describePushError(e: unknown, t: TFunction): string {
  return e instanceof PushError ? t(`notifications.push.errors.${e.code}`) : describeError(e, t)
}

/**
 * 开启推送。必须由用户点击直接触发（`Notification.requestPermission()` 是第一个 await，Safari 要求它在用户手势里）。
 * `deviceId`：本浏览器在 hub 上的 web 设备 id；`vapidKey`：`/api/config.vapid_public_key`。
 */
export async function enablePush(opts: { deviceId: string; vapidKey: string }): Promise<void> {
  if (Notification.permission === 'denied') throw new PushError('denied')
  if (Notification.permission !== 'granted') {
    const answer = await Notification.requestPermission()
    if (answer !== 'granted') throw new PushError(answer === 'denied' ? 'denied' : 'dismissed')
  }
  const reg = await getRegistration()
  if (!reg) throw new PushError('no_registration')

  let sub = await reg.pushManager.getSubscription()
  let created = false
  try {
    // hub 的 VAPID 密钥换过的话旧订阅发不出推送：退掉重订
    if (sub && !usesServerKey(sub, opts.vapidKey)) {
      await sub.unsubscribe()
      sub = null
    }
    if (!sub) {
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: base64UrlToBytes(opts.vapidKey) })
      created = true
    }
  } catch (e) {
    throw new PushError('subscribe_failed', e)
  }

  const body = toSubscribeBody(sub, opts.deviceId)
  try {
    if (!body) throw new PushError('subscribe_failed')
    await registerPushSubscription(body)
  } catch (e) {
    if (created) await sub.unsubscribe().catch((err: unknown) => console.warn('anycon: could not roll back the push subscription', err))
    throw e
  }
}

/**
 * 关闭推送：先在浏览器里退订（这一步成功，推送就已经收不到了），再让 hub 删掉这台设备的订阅。
 * hub 那一步失败不算失败——死掉的订阅会在 hub 下一次推送得到 404 / 410 时被删。
 */
export async function disablePush(opts: { deviceId: string }): Promise<void> {
  const sub = await getLocalSubscription()
  if (sub) {
    try {
      await sub.unsubscribe()
    } catch (e) {
      throw new PushError('unsubscribe_failed', e)
    }
  }
  try {
    const rows = await fetchPushSubscriptions()
    for (const row of rows.filter((r) => r.device_id === opts.deviceId)) await deletePushSubscription(row.id)
  } catch (e) {
    console.warn('anycon: push is off in this browser, but the hub still lists the subscription (it drops it on the next push)', e)
  }
}

/**
 * 对账：浏览器里有订阅、权限还在，但 hub 里这台设备没有（或指向别的推送服务）——比如 service worker 的
 * `pushsubscriptionchange` 没来得及上报（登录态过期）——就补报一次。返回是否补报了。失败只记日志。
 */
export async function reconcilePush(opts: { deviceId: string; rows: readonly ApiPushSubscription[] }): Promise<boolean> {
  try {
    if (Notification.permission !== 'granted') return false
    const sub = await getLocalSubscription()
    if (!sub) return false
    const mine = opts.rows.find((r) => r.device_id === opts.deviceId)
    if (mine && mine.endpoint_host === endpointHost(sub.endpoint)) return false
    const body = toSubscribeBody(sub, opts.deviceId)
    if (!body) return false
    await registerPushSubscription(body)
    return true
  } catch (e) {
    if (!(e instanceof ApiError)) console.warn('anycon: could not re-report the push subscription', e)
    else console.warn(`anycon: the hub refused the push subscription (${e.code})`)
    return false
  }
}
