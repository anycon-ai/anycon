// 退出时清掉推送：普通退出登录会退订浏览器的订阅 + 删 hub 里这台设备的订阅行；退出并移除本设备再多删 service worker 的语言库。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, type FakePush } from '@/test/fakePush'
import { dropPushOnSignOut } from './cleanup'

let push: FakePush
const row = (id: string, device_id: string) => ({ id, device_id, endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 })

beforeEach(() => {
  push = installFakePush({ permission: 'granted' })
  push.subscription = push.makeSubscription()
})
afterEach(() => {
  uninstallFakePush()
  vi.useRealTimers()
})

describe('dropPushOnSignOut', () => {
  it('cancels the browser subscription and deletes this device’s row at the hub — only this device’s', async () => {
    const sub = push.subscription!
    const api = installFakeApi({
      'GET /api/push/subscriptions': () => [row('mine', 'dev1'), row('other-browser', 'dev2'), row('mine-too', 'dev1')],
      'DELETE /api/push/subscriptions/*': () => undefined,
    })
    await dropPushOnSignOut('dev1')
    expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
    expect(push.subscription).toBeNull()
    expect(api.calls.filter((c) => c.method === 'DELETE').map((c) => c.path)).toEqual(['/api/push/subscriptions/mine', '/api/push/subscriptions/mine-too'])
  })

  it('a browser that never paired has no device row at the hub: only the browser subscription goes, and the hub is not asked', async () => {
    const sub = push.subscription!
    const api = installFakeApi({})
    await dropPushOnSignOut(null)
    expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
    expect(api.calls).toHaveLength(0)
  })

  it('no subscription in the browser: still removes a leftover row at the hub', async () => {
    push.subscription = null
    const api = installFakeApi({ 'GET /api/push/subscriptions': () => [row('stale', 'dev1')], 'DELETE /api/push/subscriptions/stale': () => undefined })
    await dropPushOnSignOut('dev1')
    expect(api.callsTo('DELETE /api/push/subscriptions/stale')).toHaveLength(1)
  })

  it('a hub that fails does not stop the browser unsubscribe, and nothing is thrown', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const sub = push.subscription!
    installFakeApi({ 'GET /api/push/subscriptions': () => ({ status: 500, body: errorBody('internal') }) })
    await expect(dropPushOnSignOut('dev1')).resolves.toBeUndefined()
    expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
  })

  it('a browser unsubscribe that fails does not stop the hub delete (that is what makes the hub stop pushing), and nothing is thrown', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    push.subscription!.unsubscribe = vi.fn(async () => {
      throw new Error('push service unreachable')
    })
    const api = installFakeApi({ 'GET /api/push/subscriptions': () => [row('mine', 'dev1')], 'DELETE /api/push/subscriptions/mine': () => undefined })
    await expect(dropPushOnSignOut('dev1')).resolves.toBeUndefined()
    expect(api.callsTo('DELETE /api/push/subscriptions/mine')).toHaveLength(1)
  })

  it('does not hold the sign-out for ever: gives up after the time limit', async () => {
    vi.useFakeTimers()
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>(() => undefined))) // hub 不回话
    const done = vi.fn()
    const p = dropPushOnSignOut('dev1', 5000).then(done)
    await vi.advanceTimersByTimeAsync(4999)
    expect(done).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(1)
    await p
    expect(done).toHaveBeenCalled()
    vi.unstubAllGlobals()
  })

  it('a browser without push support has nothing to cancel: no requests at all', async () => {
    uninstallFakePush()
    const api = installFakeApi({})
    await dropPushOnSignOut('dev1')
    expect(api.calls).toHaveLength(0)
  })
})
