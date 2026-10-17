import { describe, expect, it, vi } from 'vitest'
import { handleSubscriptionChange, type ResubscribeDeps } from './resubscribe'
import type { PushSubscriptionLike } from './types'

const sub = (endpoint: string, keys: Record<string, string> | null = { p256dh: 'P', auth: 'A' }, key?: ArrayBuffer): PushSubscriptionLike => ({
  endpoint,
  ...(key ? { options: { applicationServerKey: key } } : {}),
  toJSON: () => ({ endpoint, ...(keys ? { keys } : {}) }),
})

function setup(over: Partial<ResubscribeDeps> = {}) {
  const upload = vi.fn<ResubscribeDeps['upload']>(async () => 'ok')
  const subscribe = vi.fn<ResubscribeDeps['subscribe']>(async () => sub('https://fcm.googleapis.com/new'))
  const deps: ResubscribeDeps = { deviceId: async () => 'dev1', vapidKey: async () => 'VAPID', subscribe, upload, ...over }
  return { deps, upload, subscribe }
}

describe('handleSubscriptionChange', () => {
  it('subscribes again with the old subscription’s key and reports the new one for this device', async () => {
    const { deps, upload, subscribe } = setup()
    const key = new Uint8Array([4, 1, 2]).buffer
    const r = await handleSubscriptionChange({ oldSubscription: sub('https://fcm.googleapis.com/old', null, key), newSubscription: null }, deps)
    expect(r).toBe('ok')
    expect(subscribe).toHaveBeenCalledWith(key)
    expect(upload).toHaveBeenCalledWith({ device_id: 'dev1', endpoint: 'https://fcm.googleapis.com/new', keys: { p256dh: 'P', auth: 'A' } })
  })

  it('asks the hub for its VAPID key when the old subscription is gone or has none', async () => {
    const { deps, subscribe } = setup()
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, deps)).toBe('ok')
    expect(subscribe).toHaveBeenCalledWith('VAPID')
  })

  it('uses the new subscription the browser already made, without subscribing again', async () => {
    const { deps, upload, subscribe } = setup()
    const r = await handleSubscriptionChange({ oldSubscription: null, newSubscription: sub('https://updates.push.services.mozilla.com/x') }, deps)
    expect(r).toBe('ok')
    expect(subscribe).not.toHaveBeenCalled()
    expect(upload).toHaveBeenCalledWith(expect.objectContaining({ endpoint: 'https://updates.push.services.mozilla.com/x' }))
  })

  it('does nothing for a browser that was never paired (no device id to report)', async () => {
    const { deps, upload, subscribe } = setup({ deviceId: async () => null })
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, deps)).toBe('no_device')
    expect(subscribe).not.toHaveBeenCalled()
    expect(upload).not.toHaveBeenCalled()
  })

  it('gives up when there is no key to subscribe with', async () => {
    const { deps, subscribe } = setup({ vapidKey: async () => null })
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, deps)).toBe('no_key')
    expect(subscribe).not.toHaveBeenCalled()
  })

  it('does not upload a subscription without endpoint or keys', async () => {
    const { deps, upload } = setup({ subscribe: async () => sub('https://fcm.googleapis.com/new', null) })
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, deps)).toBe('bad_subscription')
    expect(upload).not.toHaveBeenCalled()
  })

  it('reports an expired login session (the settings page reconciles later) and upload failures', async () => {
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, setup({ upload: async () => 'unauthorized' }).deps)).toBe('unauthorized')
    expect(await handleSubscriptionChange({ oldSubscription: null, newSubscription: null }, setup({ upload: async () => 'failed' }).deps)).toBe('failed')
  })
})
