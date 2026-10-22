// 浏览器里的推送 API（spec/web.md § 4.9）：能力检测、通知权限、service worker 注册、本浏览器的 `PushSubscription`。
// 都在调用时才读全局对象（jsdom 里没有这些，测试用 stub 换进去）。
import { base64UrlToBytes } from '@/lib/base64'
import { bytesEqual } from '@/lib/bytes'
import { isIos } from '@/lib/device'
import type { ApiPushSubscribe } from '@/api/types'

/** 为什么这个浏览器收不了推送。 */
export type UnsupportedReason = 'insecure' | 'no_service_worker' | 'no_push' | 'ios_install'
export type PushSupport = { ok: true } | { ok: false; reason: UnsupportedReason }

function isStandalone(): boolean {
  return (navigator as Navigator & { standalone?: boolean }).standalone === true || window.matchMedia?.('(display-mode: standalone)').matches === true
}

/**
 * 能不能用 Web Push：要安全上下文（https 或 localhost）、service worker、`PushManager`、`Notification`。
 * iOS 的 Safari 标签页里没有 `PushManager`，只有加到主屏的 PWA 有，这种情况单独说明（教用户怎么做）。
 */
export function detectPushSupport(): PushSupport {
  if (!window.isSecureContext) return { ok: false, reason: 'insecure' }
  if (!('serviceWorker' in navigator)) return { ok: false, reason: 'no_service_worker' }
  if (!('PushManager' in window) || !('Notification' in window)) return { ok: false, reason: isIos() && !isStandalone() ? 'ios_install' : 'no_push' }
  return { ok: true }
}

export function notificationPermission(): NotificationPermission | 'unsupported' {
  return typeof Notification === 'undefined' ? 'unsupported' : Notification.permission
}

/**
 * 当前 service worker 注册。没有注册（开发服务器上就没有）返回 null；注册了但还在安装，等它激活（最多 `timeoutMs`）。
 */
export async function getRegistration(timeoutMs = 8000): Promise<ServiceWorkerRegistration | null> {
  const container = navigator.serviceWorker
  const existing = await container.getRegistration()
  if (!existing) return null
  if (existing.active) return existing
  return Promise.race([container.ready, new Promise<null>((resolve) => setTimeout(() => resolve(null), timeoutMs))])
}

export async function getLocalSubscription(timeoutMs?: number): Promise<PushSubscription | null> {
  const reg = await getRegistration(timeoutMs)
  return reg ? reg.pushManager.getSubscription() : null
}

/** 已有订阅用的是不是 hub 现在这把 VAPID 公钥。读不到密钥（旧浏览器）当作一样。 */
export function usesServerKey(sub: PushSubscription, vapidKey: string): boolean {
  const have = sub.options?.applicationServerKey
  return !have || bytesEqual(new Uint8Array(have), base64UrlToBytes(vapidKey))
}

/** `PushSubscription` → `POST /api/push/subscriptions` 的请求体；缺 endpoint 或密钥返回 null。 */
export function toSubscribeBody(sub: PushSubscription, deviceId: string): ApiPushSubscribe | null {
  const json = sub.toJSON()
  const { p256dh, auth } = json.keys ?? {}
  if (!json.endpoint || !p256dh || !auth) return null
  return { device_id: deviceId, endpoint: json.endpoint, keys: { p256dh, auth } }
}

/** endpoint 的主机名（路径本身是个凭据，永远不显示、不记日志）。 */
export function endpointHost(endpoint: string): string {
  try {
    return new URL(endpoint).hostname
  } catch {
    return ''
  }
}
