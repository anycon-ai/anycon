// 测试用的假浏览器推送栈：安全上下文、service worker 注册、`PushManager`、`Notification`。jsdom 里这些都没有。
// `installFakePush()` 装上，返回可以观察 / 摆弄的句柄；`uninstallFakePush()`（或测试的 afterEach）拆掉。
import { vi } from 'vitest'
import { bytesToBase64Url } from '@/lib/base64'

export interface FakeSubscription extends PushSubscription {
  unsubscribed: boolean
}

export interface FakePush {
  /** 当前通知权限；测试里可以直接改（模拟用户在浏览器设置里改了）。 */
  permission: NotificationPermission
  /** 弹出权限提示时用户的回答。 */
  answer: NotificationPermission
  permissionRequests: number
  /** 本浏览器当前的订阅（没有 = null）。 */
  subscription: FakeSubscription | null
  /** `pushManager.subscribe` 的调用参数。 */
  subscribeCalls: Array<{ userVisibleOnly?: boolean; applicationServerKey?: BufferSource | string | null }>
  /** 让下一次 subscribe 抛错（推送服务不可达、被 Brave 之类拦掉）。 */
  subscribeError: Error | null
  /** service worker 是否已经注册；false = 开发服务器的样子。 */
  registered: boolean
  /** 页面通过 registration.showNotification 弹出的通知。 */
  shown: Array<{ title: string; options: NotificationOptions & { renotify?: boolean } }>
  /** 造一个订阅（endpoint 是假的）。 */
  makeSubscription(endpoint?: string, key?: Uint8Array | null): FakeSubscription
}

export const VAPID_BYTES = Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : (i * 29 + 7) % 256))
export const VAPID_KEY = bytesToBase64Url(VAPID_BYTES)

export function installFakePush(opts: { permission?: NotificationPermission; secure?: boolean; registered?: boolean } = {}): FakePush {
  const fake: FakePush = {
    permission: opts.permission ?? 'default',
    answer: 'granted',
    permissionRequests: 0,
    subscription: null,
    subscribeCalls: [],
    subscribeError: null,
    registered: opts.registered ?? true,
    shown: [],
    makeSubscription(endpoint = 'https://fcm.googleapis.com/fcm/send/abc', key = VAPID_BYTES) {
      const sub = {
        endpoint,
        expirationTime: null,
        options: { applicationServerKey: key ? (key.buffer.slice(key.byteOffset, key.byteOffset + key.byteLength) as ArrayBuffer) : null, userVisibleOnly: true },
        unsubscribed: false,
        toJSON: () => ({ endpoint, expirationTime: null, keys: { p256dh: 'P256DH_KEY', auth: 'AUTH_KEY' } }),
        getKey: () => null,
        unsubscribe: vi.fn(async () => {
          sub.unsubscribed = true
          if (fake.subscription === sub) fake.subscription = null
          return true
        }),
      }
      return sub as unknown as FakeSubscription
    },
  }

  const registration = {
    active: {},
    pushManager: {
      getSubscription: async () => fake.subscription,
      subscribe: async (o: { userVisibleOnly?: boolean; applicationServerKey?: BufferSource | string | null }) => {
        fake.subscribeCalls.push(o)
        if (fake.subscribeError) throw fake.subscribeError
        const key = o.applicationServerKey instanceof Uint8Array ? o.applicationServerKey : null
        fake.subscription = fake.makeSubscription(`https://fcm.googleapis.com/fcm/send/new-${fake.subscribeCalls.length}`, key)
        return fake.subscription
      },
    },
    showNotification: async (title: string, options: NotificationOptions & { renotify?: boolean }) => void fake.shown.push({ title, options }),
  }
  const container = {
    getRegistration: async () => (fake.registered ? registration : undefined),
    ready: fake.registered ? Promise.resolve(registration) : new Promise(() => undefined),
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
  }
  Object.defineProperty(navigator, 'serviceWorker', { configurable: true, value: container })
  Object.defineProperty(window, 'isSecureContext', { configurable: true, value: opts.secure ?? true })

  class FakeNotification {
    static get permission(): NotificationPermission {
      return fake.permission
    }
    static async requestPermission(): Promise<NotificationPermission> {
      fake.permissionRequests += 1
      fake.permission = fake.answer
      return fake.answer
    }
  }
  vi.stubGlobal('Notification', FakeNotification)
  vi.stubGlobal('PushManager', class PushManager {})
  return fake
}

export function uninstallFakePush(): void {
  vi.unstubAllGlobals()
  Reflect.deleteProperty(navigator, 'serviceWorker')
  Reflect.deleteProperty(window, 'isSecureContext')
}
