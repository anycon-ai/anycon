// 开启 / 关闭 / 对账本浏览器的推送订阅。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { ApiError } from '@/api/client'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, VAPID_BYTES, VAPID_KEY, type FakePush } from '@/test/fakePush'
import { describePushError, disablePush, enablePush, PushError, reconcilePush } from './actions'

let push: FakePush

beforeEach(() => {
  push = installFakePush()
})
afterEach(() => {
  uninstallFakePush()
})

describe('enablePush', () => {
  it('asks for permission, subscribes with userVisibleOnly and the hub’s VAPID key, then reports the subscription for this device', async () => {
    const api = installFakeApi({ 'POST /api/push/subscriptions': () => ({ id: 's1' }) })
    await enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })
    expect(push.permissionRequests).toBe(1)
    expect(push.subscribeCalls).toHaveLength(1)
    expect(push.subscribeCalls[0]!.userVisibleOnly).toBe(true)
    expect(new Uint8Array(push.subscribeCalls[0]!.applicationServerKey as Uint8Array)).toEqual(VAPID_BYTES)
    const posted = api.callsTo('POST /api/push/subscriptions')
    expect(posted).toHaveLength(1)
    expect(posted[0]!.body).toEqual({ device_id: 'dev1', endpoint: push.subscription!.endpoint, keys: { p256dh: 'P256DH_KEY', auth: 'AUTH_KEY' } })
    expect(posted[0]!.headers['X-Anycon']).toBe('1')
  })

  it('does not ask again when permission is already granted', async () => {
    push.permission = 'granted'
    installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    await enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })
    expect(push.permissionRequests).toBe(0)
    expect(push.subscription).not.toBeNull()
  })

  it('permission already denied: nothing is requested, subscribed or reported', async () => {
    push.permission = 'denied'
    const api = installFakeApi({})
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'denied' })
    expect(push.permissionRequests).toBe(0)
    expect(push.subscribeCalls).toHaveLength(0)
    expect(api.calls).toHaveLength(0)
  })

  it('the user answers "denied" or just closes the prompt', async () => {
    const api = installFakeApi({})
    push.answer = 'denied'
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'denied' })
    push.permission = 'default'
    push.answer = 'default'
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'dismissed' })
    expect(push.subscribeCalls).toHaveLength(0)
    expect(api.calls).toHaveLength(0)
  })

  it('no service worker registered (dev server): a clear error, no subscription', async () => {
    uninstallFakePush()
    push = installFakePush({ registered: false })
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'no_registration' })
  })

  it('reuses an existing subscription made with the same key (no second subscribe), and still reports it', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription('https://fcm.googleapis.com/fcm/send/existing')
    const api = installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    await enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })
    expect(push.subscribeCalls).toHaveLength(0)
    expect(api.callsTo('POST /api/push/subscriptions')[0]!.body).toMatchObject({ endpoint: 'https://fcm.googleapis.com/fcm/send/existing' })
  })

  it('replaces an existing subscription that was made with another key', async () => {
    push.permission = 'granted'
    const stale = push.makeSubscription('https://fcm.googleapis.com/fcm/send/stale', Uint8Array.from({ length: 65 }, () => 9))
    push.subscription = stale
    installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    await enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })
    expect(stale.unsubscribe).toHaveBeenCalledTimes(1)
    expect(push.subscribeCalls).toHaveLength(1)
  })

  it('the browser cannot subscribe (push service unreachable): subscribe_failed, nothing reported', async () => {
    push.permission = 'granted'
    push.subscribeError = new DOMException('Registration failed - push service error', 'AbortError')
    const api = installFakeApi({})
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'subscribe_failed' })
    expect(api.calls).toHaveLength(0)
  })

  it('the hub refuses the subscription: the new browser subscription is rolled back and the hub’s error comes through', async () => {
    push.permission = 'granted'
    installFakeApi({ 'POST /api/push/subscriptions': () => ({ status: 400, body: errorBody('push_endpoint_not_allowed') }) })
    const err = await enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY }).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err).toMatchObject({ code: 'push_endpoint_not_allowed', status: 400 })
    expect(push.subscription).toBeNull() // 退订了，浏览器和 hub 没有对不上
  })

  it('does not unsubscribe a subscription it did not create when the report fails', async () => {
    push.permission = 'granted'
    const existing = push.makeSubscription('https://fcm.googleapis.com/fcm/send/existing')
    push.subscription = existing
    installFakeApi({ 'POST /api/push/subscriptions': () => ({ status: 404, body: errorBody('device_not_found') }) })
    await expect(enablePush({ deviceId: 'dev1', vapidKey: VAPID_KEY })).rejects.toMatchObject({ code: 'device_not_found' })
    expect(existing.unsubscribe).not.toHaveBeenCalled()
  })
})

describe('disablePush', () => {
  it('unsubscribes in the browser, then deletes this device’s rows at the hub (and only those)', async () => {
    push.permission = 'granted'
    const sub = push.makeSubscription()
    push.subscription = sub
    const api = installFakeApi({
      'GET /api/push/subscriptions': () => [
        { id: 'mine', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 },
        { id: 'other', device_id: 'dev2', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 },
      ],
      'DELETE /api/push/subscriptions/mine': () => undefined,
    })
    await disablePush({ deviceId: 'dev1' })
    expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
    expect(api.callsTo('DELETE /api/push/subscriptions/mine')).toHaveLength(1)
    expect(api.calls.some((c) => c.path.endsWith('/other'))).toBe(false)
  })

  it('a hub that is unreachable does not undo the local unsubscribe (the dead row is dropped on the next push)', async () => {
    const sub = push.makeSubscription()
    push.subscription = sub
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    installFakeApi({ 'GET /api/push/subscriptions': () => ({ status: 500, body: errorBody('internal') }) })
    await expect(disablePush({ deviceId: 'dev1' })).resolves.toBeUndefined()
    expect(sub.unsubscribe).toHaveBeenCalled()
  })

  it('a row the hub already dropped (404 on delete) counts as deleted', async () => {
    push.subscription = push.makeSubscription()
    installFakeApi({
      'GET /api/push/subscriptions': () => [{ id: 'mine', device_id: 'dev1', endpoint_host: 'x', created_at_ms: 1 }],
      'DELETE /api/push/subscriptions/mine': () => ({ status: 404, body: errorBody('not_found') }),
    })
    await expect(disablePush({ deviceId: 'dev1' })).resolves.toBeUndefined()
  })

  it('when the browser refuses to unsubscribe nothing is changed at the hub', async () => {
    const sub = push.makeSubscription()
    sub.unsubscribe = vi.fn(async () => {
      throw new Error('nope')
    })
    push.subscription = sub
    const api = installFakeApi({})
    await expect(disablePush({ deviceId: 'dev1' })).rejects.toMatchObject({ code: 'unsubscribe_failed' })
    expect(api.calls).toHaveLength(0)
  })
})

describe('reconcilePush', () => {
  const row = (over: Partial<{ id: string; device_id: string; endpoint_host: string }> = {}) => ({ id: 's1', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1, ...over })

  it('re-reports a subscription the hub does not have', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    const api = installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    expect(await reconcilePush({ deviceId: 'dev1', rows: [] })).toBe(true)
    expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(1)
  })

  it('re-reports when the hub’s row points at another push service than the browser’s subscription', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription('https://updates.push.services.mozilla.com/wpush/v2/new')
    const api = installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    expect(await reconcilePush({ deviceId: 'dev1', rows: [row()] })).toBe(true)
    expect(api.calls).toHaveLength(1)
  })

  it('does nothing when they agree, when there is no subscription, or when permission is gone', async () => {
    const api = installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    expect(await reconcilePush({ deviceId: 'dev1', rows: [row()] })).toBe(false)
    push.subscription = null
    expect(await reconcilePush({ deviceId: 'dev1', rows: [] })).toBe(false)
    push.subscription = push.makeSubscription()
    push.permission = 'denied'
    expect(await reconcilePush({ deviceId: 'dev1', rows: [] })).toBe(false)
    expect(api.calls).toHaveLength(0)
  })

  it('ignores another device’s row', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    installFakeApi({ 'POST /api/push/subscriptions': () => undefined })
    expect(await reconcilePush({ deviceId: 'dev1', rows: [row({ device_id: 'dev2' })] })).toBe(true)
  })

  it('a hub that refuses is logged, not thrown', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    installFakeApi({ 'POST /api/push/subscriptions': () => ({ status: 400, body: errorBody('push_endpoint_not_allowed') }) })
    expect(await reconcilePush({ deviceId: 'dev1', rows: [] })).toBe(false)
  })
})

describe('describePushError', () => {
  const en = i18n.getFixedT('en')
  it('explains browser-side failures and falls back to the API error text', () => {
    expect(describePushError(new PushError('denied'), en)).toMatch(/blocked/)
    expect(describePushError(new PushError('no_registration'), en)).toMatch(/service worker/)
    expect(describePushError(new ApiError(400, 'push_endpoint_not_allowed', 'x'), en)).toMatch(/does not support this browser’s push service/)
  })
})
