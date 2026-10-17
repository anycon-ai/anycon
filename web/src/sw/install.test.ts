// service worker 的接线：事件是否登记、异步工作是否被 waitUntil 包住、「新版本就绪才刷新」的约定没有被破坏。
import { describe, expect, it, vi } from 'vitest'
import { NAVIGATE_MESSAGE } from './click'
import { installHandlers, NAVIGATION_DENYLIST, SKIP_WAITING, type Handlers } from './install'
import { pushTranslator } from './i18n'
import type { MessageEventLike, NotificationEventLike, PushEventLike, SubscriptionChangeEventLike, SwEventMap, SwScope, WindowClientLike } from './types'

type Listener = (event: never) => void

function fakeScope() {
  const listeners = new Map<string, Listener>()
  const skipWaiting = vi.fn(async () => undefined)
  const scope: SwScope = {
    location: { origin: 'https://hub.example' },
    registration: { showNotification: vi.fn(async () => undefined), pushManager: { subscribe: vi.fn() } },
    clients: { matchAll: vi.fn(async () => []), openWindow: vi.fn(async () => undefined) },
    skipWaiting,
    addEventListener: (type, listener) => void listeners.set(type, listener as Listener),
  }
  const fire = <K extends keyof SwEventMap>(type: K, event: SwEventMap[K]): void => (listeners.get(type) as unknown as (e: SwEventMap[K]) => void)(event)
  return { scope, listeners, skipWaiting, fire }
}

function waiter() {
  const waited: Array<Promise<unknown>> = []
  return { waitUntil: (p: Promise<unknown>) => void waited.push(p), waited }
}

function handlers(over: Partial<Handlers> = {}) {
  const show = vi.fn(async () => undefined)
  const openWindow = vi.fn(async () => undefined)
  const win: WindowClientLike = { url: 'https://hub.example/', focus: vi.fn(async () => undefined), postMessage: vi.fn() }
  const h: Handlers = {
    push: { directory: async () => ({ nodeName: 'mac-mini', nodeIds: [] }), language: async () => 'en', navigatorLanguage: () => 'en', translator: pushTranslator, show },
    click: { origin: 'https://hub.example', windows: async () => [win], openWindow },
    resubscribe: { deviceId: async () => 'dev1', vapidKey: async () => 'K', subscribe: vi.fn(), upload: vi.fn(async () => 'ok' as const) },
    ...over,
  }
  return { h, show, openWindow, win }
}

describe('installHandlers', () => {
  it('registers exactly message, push, notificationclick and pushsubscriptionchange — no install / activate takeover', () => {
    const { scope, listeners } = fakeScope()
    installHandlers(scope, handlers().h)
    expect([...listeners.keys()].sort()).toEqual(['message', 'notificationclick', 'push', 'pushsubscriptionchange'])
  })

  it('SKIP_WAITING from the page makes the new version take over; other messages do not', async () => {
    const { scope, fire, skipWaiting } = fakeScope()
    installHandlers(scope, handlers().h)
    for (const data of [undefined, null, 'SKIP_WAITING', { type: 'something else' }, {}]) {
      const w = waiter()
      fire('message', { data, waitUntil: w.waitUntil } as MessageEventLike)
      expect(w.waited).toHaveLength(0)
    }
    expect(skipWaiting).not.toHaveBeenCalled()
    const w = waiter()
    fire('message', { data: { type: SKIP_WAITING }, waitUntil: w.waitUntil } as MessageEventLike)
    await Promise.all(w.waited)
    expect(skipWaiting).toHaveBeenCalledTimes(1)
  })

  it('push: the notification is shown inside waitUntil', async () => {
    const { scope, fire } = fakeScope()
    const { h, show } = handlers()
    installHandlers(scope, h)
    const w = waiter()
    fire('push', { data: { json: () => ({ category: 'enroll_requested', node: 'n'.repeat(32), session: null }) }, waitUntil: w.waitUntil } satisfies PushEventLike)
    expect(w.waited).toHaveLength(1)
    await Promise.all(w.waited)
    expect(show).toHaveBeenCalledWith('A device wants to join mac-mini', expect.objectContaining({ tag: `enroll_requested:${'n'.repeat(32)}` }))
  })

  it('push: a failure while showing is logged, never thrown into the event', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const { scope, fire } = fakeScope()
    const { h } = handlers()
    h.push.show = async () => {
      throw new Error('notifications are blocked')
    }
    installHandlers(scope, h)
    const w = waiter()
    fire('push', { data: null, waitUntil: w.waitUntil })
    await expect(Promise.all(w.waited)).resolves.toBeDefined()
    expect(warn).toHaveBeenCalled()
  })

  it('notificationclick: closes the notification and opens / focuses a window inside waitUntil', async () => {
    const { scope, fire } = fakeScope()
    const { h, win } = handlers()
    installHandlers(scope, h)
    const close = vi.fn()
    const w = waiter()
    fire('notificationclick', { action: '', notification: { data: { url: '/n/abcd1234/s/sess_1' }, tag: 't', close }, waitUntil: w.waitUntil } satisfies NotificationEventLike)
    expect(close).toHaveBeenCalledTimes(1)
    await Promise.all(w.waited)
    expect(win.postMessage).toHaveBeenCalledWith({ type: NAVIGATE_MESSAGE, url: '/n/abcd1234/s/sess_1' })
  })

  it('pushsubscriptionchange: renews and reports the subscription inside waitUntil', async () => {
    const { scope, fire } = fakeScope()
    const { h } = handlers()
    installHandlers(scope, h)
    const w = waiter()
    const event: SubscriptionChangeEventLike = {
      oldSubscription: null,
      newSubscription: { endpoint: 'https://fcm.googleapis.com/x', toJSON: () => ({ endpoint: 'https://fcm.googleapis.com/x', keys: { p256dh: 'P', auth: 'A' } }) },
      waitUntil: w.waitUntil,
    }
    fire('pushsubscriptionchange', event)
    await Promise.all(w.waited)
    expect(h.resubscribe.upload).toHaveBeenCalledWith({ device_id: 'dev1', endpoint: 'https://fcm.googleapis.com/x', keys: { p256dh: 'P', auth: 'A' } })
  })
})

describe('navigation fallback', () => {
  it('keeps the exact deny list of the previous generateSW configuration', () => {
    expect(NAVIGATION_DENYLIST.map(String)).toEqual([String(/^\/api\//), String(/^\/auth\//), String(/^\/v1\//), String(/^\/healthz/), String(/^\/readyz/)])
    for (const path of ['/api/me', '/auth/token', '/v1/ws', '/healthz', '/readyz']) expect(NAVIGATION_DENYLIST.some((r) => r.test(path))).toBe(true)
    for (const path of ['/', '/settings/notifications', '/n/abcd1234/s/sess_1', '/login']) expect(NAVIGATION_DENYLIST.some((r) => r.test(path))).toBe(false)
  })
})
