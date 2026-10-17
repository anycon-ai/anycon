// 把 service worker 的事件接到上面的纯逻辑上。`src/sw.ts` 只负责 workbox 的预缓存 / 导航回退，然后调 `installHandlers`。
// 这一层同样可以用假的 scope 测试：事件是否登记、`waitUntil` 是否包住了异步工作、「新版本就绪才刷新」的约定。
import { pushTranslator } from './i18n'
import { handleNotificationClick, type ClickDeps } from './click'
import { readDeviceId, readPushDirectory, readSwLanguage } from './directory'
import { handlePush, type PushDeps } from './push'
import { handleSubscriptionChange, type ResubscribeDeps } from './resubscribe'
import type { SwScope } from './types'

/** 导航回退不碰的路径：hub 的 API、认证、WebSocket、健康检查（和旧的 generateSW 配置一致）。 */
export const NAVIGATION_DENYLIST: readonly RegExp[] = [/^\/api\//, /^\/auth\//, /^\/v1\//, /^\/healthz/, /^\/readyz/]

/** 页面（workbox-window 的 `messageSkipWaiting`）要求新版本接管时发的消息。 */
export const SKIP_WAITING = 'SKIP_WAITING'

export interface Handlers {
  push: PushDeps
  click: ClickDeps
  resubscribe: ResubscribeDeps
}

function fail(what: string): (e: unknown) => void {
  return (e) => console.warn(`anycon sw: ${what} failed`, e)
}

/** 真实的依赖：IndexedDB、`fetch`、scope 上的 API。 */
export function realHandlers(scope: SwScope): Handlers {
  const origin = scope.location.origin
  return {
    push: {
      directory: readPushDirectory,
      language: readSwLanguage,
      navigatorLanguage: () => (typeof navigator === 'undefined' ? undefined : navigator.language),
      translator: pushTranslator,
      show: (title, options) => scope.registration.showNotification(title, options),
    },
    click: {
      origin,
      windows: () => scope.clients.matchAll({ type: 'window', includeUncontrolled: true }),
      openWindow: (url) => scope.clients.openWindow(url),
    },
    resubscribe: {
      deviceId: readDeviceId,
      vapidKey: async () => {
        const res = await fetch('/api/config', { headers: { Accept: 'application/json' } })
        if (!res.ok) return null
        const cfg: unknown = await res.json()
        const key = typeof cfg === 'object' && cfg !== null ? (cfg as Record<string, unknown>).vapid_public_key : undefined
        return typeof key === 'string' && key !== '' ? key : null
      },
      subscribe: (applicationServerKey) => scope.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey }),
      upload: async (body) => {
        const res = await fetch('/api/push/subscriptions', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json', 'X-Anycon': '1', Accept: 'application/json' },
          body: JSON.stringify(body),
        })
        if (res.ok) return 'ok'
        return res.status === 401 ? 'unauthorized' : 'failed'
      },
    },
  }
}

export function installHandlers(scope: SwScope, handlers: Handlers = realHandlers(scope)): void {
  // 新版本装好后一直 waiting，直到页面点了「刷新」才发这条消息（spec/web.md § 8）。**不要**在 install 里 skipWaiting / clients.claim。
  scope.addEventListener('message', (event) => {
    const data = event.data
    if (typeof data === 'object' && data !== null && (data as Record<string, unknown>).type === SKIP_WAITING) event.waitUntil(scope.skipWaiting())
  })

  scope.addEventListener('push', (event) => {
    event.waitUntil(handlePush(event.data, handlers.push).catch(fail('showing a push notification')))
  })

  scope.addEventListener('notificationclick', (event) => {
    event.notification.close()
    event.waitUntil(handleNotificationClick(event.notification.data, handlers.click).catch(fail('opening a notification')))
  })

  scope.addEventListener('pushsubscriptionchange', (event) => {
    event.waitUntil(handleSubscriptionChange(event, handlers.resubscribe).then((r) => (r === 'ok' ? undefined : console.warn(`anycon sw: push subscription not renewed (${r})`)), fail('renewing the push subscription')))
  })
}
