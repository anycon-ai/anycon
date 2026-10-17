import { afterEach, describe, expect, it, vi } from 'vitest'
import { installFakePush, uninstallFakePush, VAPID_BYTES, VAPID_KEY } from '@/test/fakePush'
import { detectPushSupport, endpointHost, getLocalSubscription, getRegistration, notificationPermission, toSubscribeBody, usesServerKey } from './browser'

afterEach(() => {
  uninstallFakePush()
  vi.restoreAllMocks()
  for (const key of ['userAgent', 'platform', 'maxTouchPoints', 'standalone']) Reflect.deleteProperty(navigator, key)
})

function setUa(ua: string, platform = 'iPhone', touch = 5): void {
  for (const [key, value] of Object.entries({ userAgent: ua, platform, maxTouchPoints: touch })) Object.defineProperty(navigator, key, { configurable: true, value })
}

describe('detectPushSupport', () => {
  it('is fine with a secure context, a service worker, PushManager and Notification', () => {
    installFakePush()
    expect(detectPushSupport()).toEqual({ ok: true })
  })

  it('an insecure context (plain http on the LAN) is reported first', () => {
    installFakePush({ secure: false })
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'insecure' })
  })

  it('no service worker support', () => {
    installFakePush()
    Reflect.deleteProperty(navigator, 'serviceWorker')
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'no_service_worker' })
  })

  it('no PushManager in an ordinary browser is just "not supported"', () => {
    installFakePush()
    vi.stubGlobal('PushManager', undefined)
    Reflect.deleteProperty(window, 'PushManager')
    Reflect.deleteProperty(globalThis, 'PushManager')
    setUa('Mozilla/5.0 (X11; Linux x86_64) Firefox/200.0', 'Linux x86_64', 0)
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'no_push' })
  })

  it('on iPhone / iPad without PushManager it tells the user to add the app to the Home Screen', () => {
    installFakePush()
    Reflect.deleteProperty(window, 'PushManager')
    Reflect.deleteProperty(globalThis, 'PushManager')
    setUa('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1')
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'ios_install' })
    // iPadOS 的 Safari 自称 Mac，靠触点数认
    setUa('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Safari/604.1', 'MacIntel', 5)
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'ios_install' })
  })

  it('an installed iOS PWA without PushManager is not told to install again', () => {
    installFakePush()
    Reflect.deleteProperty(window, 'PushManager')
    Reflect.deleteProperty(globalThis, 'PushManager')
    setUa('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1')
    Object.defineProperty(navigator, 'standalone', { configurable: true, value: true })
    expect(detectPushSupport()).toEqual({ ok: false, reason: 'no_push' })
  })
})

describe('notification permission', () => {
  it('reads Notification.permission, or "unsupported" without Notification', () => {
    const fake = installFakePush({ permission: 'denied' })
    expect(notificationPermission()).toBe('denied')
    fake.permission = 'granted'
    expect(notificationPermission()).toBe('granted')
    vi.stubGlobal('Notification', undefined)
    Reflect.deleteProperty(globalThis, 'Notification')
    expect(notificationPermission()).toBe('unsupported')
  })
})

describe('service worker registration', () => {
  it('returns the active registration and the subscription it holds', async () => {
    const fake = installFakePush()
    expect(await getRegistration()).not.toBeNull()
    expect(await getLocalSubscription()).toBeNull()
    fake.subscription = fake.makeSubscription()
    expect(await getLocalSubscription()).toBe(fake.subscription)
  })

  it('no registration (the dev server) → null at once, without waiting', async () => {
    installFakePush({ registered: false })
    expect(await getRegistration(10)).toBeNull()
    expect(await getLocalSubscription()).toBeNull()
  })
})

describe('subscription helpers', () => {
  it('usesServerKey compares the key the subscription was made with against the hub’s current one', () => {
    const fake = installFakePush()
    expect(usesServerKey(fake.makeSubscription(), VAPID_KEY)).toBe(true)
    const other = Uint8Array.from(VAPID_BYTES)
    other[10] = (other[10] ?? 0) ^ 0xff
    expect(usesServerKey(fake.makeSubscription('https://x.push.services.mozilla.com/a', other), VAPID_KEY)).toBe(false)
    expect(usesServerKey(fake.makeSubscription('https://x.push.services.mozilla.com/a', null), VAPID_KEY)).toBe(true) // 读不到密钥：不动它
  })

  it('toSubscribeBody builds the POST body with this browser’s device id', () => {
    const fake = installFakePush()
    expect(toSubscribeBody(fake.makeSubscription('https://fcm.googleapis.com/fcm/send/xyz'), 'dev1')).toEqual({
      device_id: 'dev1',
      endpoint: 'https://fcm.googleapis.com/fcm/send/xyz',
      keys: { p256dh: 'P256DH_KEY', auth: 'AUTH_KEY' },
    })
  })

  it('toSubscribeBody refuses a subscription without keys', () => {
    const fake = installFakePush()
    const sub = fake.makeSubscription()
    vi.spyOn(sub, 'toJSON').mockReturnValue({ endpoint: sub.endpoint })
    expect(toSubscribeBody(sub, 'dev1')).toBeNull()
  })

  it('endpointHost keeps only the host (the path is a credential)', () => {
    expect(endpointHost('https://fcm.googleapis.com/fcm/send/SECRET-TOKEN')).toBe('fcm.googleapis.com')
    expect(endpointHost('https://updates.push.services.mozilla.com/wpush/v2/SECRET')).toBe('updates.push.services.mozilla.com')
    expect(endpointHost('garbage')).toBe('')
  })
})
