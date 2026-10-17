import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { forgetSystemNotifyPrefs, pushEnabledFor } from './prefsCache'

const prefs = { categories: [{ category: 'permission_requested', push: true, email: true }, { category: 'turn_finished', push: false, email: false }], email_available: true, push_available: true }

beforeEach(() => forgetSystemNotifyPrefs())
afterEach(() => vi.unstubAllGlobals())

describe('pushEnabledFor', () => {
  it('follows the account’s push switch for the category', async () => {
    installFakeApi({ 'GET /api/me/notifications': () => prefs })
    expect(await pushEnabledFor('permission_requested')).toBe(true)
    expect(await pushEnabledFor('turn_finished')).toBe(false)
  })

  it('asks the hub once per minute, not once per notification, and again after a save', async () => {
    const api = installFakeApi({ 'GET /api/me/notifications': () => prefs })
    const t0 = 1_000_000
    await pushEnabledFor('error', t0)
    await pushEnabledFor('error', t0 + 30_000)
    expect(api.callsTo('GET /api/me/notifications')).toHaveLength(1)
    await pushEnabledFor('error', t0 + 61_000)
    expect(api.callsTo('GET /api/me/notifications')).toHaveLength(2)
    forgetSystemNotifyPrefs()
    await pushEnabledFor('error', t0 + 62_000)
    expect(api.callsTo('GET /api/me/notifications')).toHaveLength(3)
  })

  it('concurrent notifications share one request', async () => {
    const api = installFakeApi({ 'GET /api/me/notifications': () => prefs })
    await Promise.all([pushEnabledFor('error'), pushEnabledFor('security'), pushEnabledFor('turn_finished')])
    expect(api.callsTo('GET /api/me/notifications')).toHaveLength(1)
  })

  it('a category the hub does not list, or a hub that cannot be asked, counts as on (better one extra than a missed approval)', async () => {
    installFakeApi({ 'GET /api/me/notifications': () => prefs })
    expect(await pushEnabledFor('security')).toBe(true)
    forgetSystemNotifyPrefs()
    vi.spyOn(console, 'debug').mockImplementation(() => undefined)
    installFakeApi({ 'GET /api/me/notifications': () => ({ status: 500, body: errorBody('internal') }) })
    expect(await pushEnabledFor('turn_finished')).toBe(true)
  })
})
