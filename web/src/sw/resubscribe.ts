// `pushsubscriptionchange`：浏览器让订阅过期 / 换掉了（推送服务轮换、用户在浏览器里清了站点数据又恢复……）。
// service worker 重新订阅并上报给 hub（同一设备 hub 只留一条，新的顶掉旧的）。上报要登录 cookie：
// 登录态没了（401）就放弃——设置页打开时会再对一次账（`features/push/actions.ts` 的 `reconcilePush`）。
import type { PushSubscriptionLike, SubscriptionChangeEventLike } from './types'

export interface ResubscribeBody {
  device_id: string
  endpoint: string
  keys: { p256dh: string; auth: string }
}

export type ResubscribeResult = 'ok' | 'no_device' | 'no_key' | 'bad_subscription' | 'unauthorized' | 'failed'

export interface ResubscribeDeps {
  /** 这个浏览器的 web 设备 id（主库 `keys.self.device_id`）。 */
  deviceId(): Promise<string | null>
  /** hub 的 VAPID 公钥（`/api/config`）。 */
  vapidKey(): Promise<string | null>
  subscribe(applicationServerKey: ArrayBuffer | string): Promise<PushSubscriptionLike>
  upload(body: ResubscribeBody): Promise<'ok' | 'unauthorized' | 'failed'>
}

export async function handleSubscriptionChange(event: Pick<SubscriptionChangeEventLike, 'oldSubscription' | 'newSubscription'>, deps: ResubscribeDeps): Promise<ResubscribeResult> {
  const deviceId = await deps.deviceId()
  if (!deviceId) return 'no_device'
  let sub = event.newSubscription
  if (!sub) {
    // 优先沿用旧订阅用的密钥（hub 的密钥不会变），拿不到再问 hub
    const key = event.oldSubscription?.options?.applicationServerKey ?? (await deps.vapidKey())
    if (!key) return 'no_key'
    sub = await deps.subscribe(key)
  }
  const json = sub.toJSON()
  const { p256dh, auth } = json.keys ?? {}
  if (!json.endpoint || !p256dh || !auth) return 'bad_subscription'
  const res = await deps.upload({ device_id: deviceId, endpoint: json.endpoint, keys: { p256dh, auth } })
  return res
}
