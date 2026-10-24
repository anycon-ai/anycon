import 'fake-indexeddb/auto'
import { encodePreviewLaunch } from '@anycon/protocol'
import { beforeEach, describe, expect, it } from 'vitest'
import { makeLaunch } from '../test/rig'
import { DB_NAME, STORES, idbAll, idbClear, idbDelete, idbGet, idbPut } from './idb'
import { clearLaunch, hostLabel, launchMatchesHost, loadLaunch, saveLaunch, validateLaunch } from './launch'

const NOW = 1_700_000_000_000

beforeEach(async () => {
  await idbClear(STORES.launch)
})

describe('idb helpers', () => {
  it('stores, reads, lists and deletes values per object store', async () => {
    expect(DB_NAME).toBe('anycon-preview')
    expect(await idbGet(STORES.cookies, 'k')).toBeUndefined()
    await idbPut(STORES.cookies, 'k', { a: 1 })
    await idbPut(STORES.cookies, 'k2', [1, 2])
    expect(await idbGet(STORES.cookies, 'k')).toEqual({ a: 1 })
    expect((await idbAll(STORES.cookies)).map((e) => e.key).sort()).toEqual(['k', 'k2'])
    await idbDelete(STORES.cookies, 'k')
    expect(await idbGet(STORES.cookies, 'k')).toBeUndefined()
    await idbClear(STORES.cookies)
    expect(await idbAll(STORES.cookies)).toEqual([])
  })

  it('keeps the three stores apart', async () => {
    await idbPut(STORES.httpCache, 'u', 1)
    expect(await idbGet(STORES.launch, 'u')).toBeUndefined()
    await idbClear(STORES.httpCache)
  })
})

describe('launch storage', () => {
  it('round-trips a launch', async () => {
    const launch = makeLaunch({ expires_at_ms: NOW + 1000 })
    await saveLaunch(launch)
    expect(await loadLaunch(NOW)).toEqual({ ok: true, launch })
  })

  it('reports nothing stored, expired, and broken records', async () => {
    expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'none' })
    await saveLaunch(makeLaunch({ expires_at_ms: NOW }))
    expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'expired' })
    expect(await loadLaunch(NOW - 1)).toMatchObject({ ok: true })
    await idbPut(STORES.launch, 'current', { v: 1, token: 'only a token' })
    expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'invalid' })
    await idbPut(STORES.launch, 'current', 'a string')
    expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'invalid' })
  })

  it('does not trust what it reads back: the same rules as for the URL fragment', async () => {
    for (const bad of [
      { ...makeLaunch({ expires_at_ms: NOW + 1000 }), path: '//evil.example/' },
      { ...makeLaunch({ expires_at_ms: NOW + 1000 }), path: '/\\evil.example/' },
      { ...makeLaunch({ expires_at_ms: NOW + 1000 }), psk: 'zz' },
      { ...makeLaunch({ expires_at_ms: NOW + 1000 }), v: 2 },
    ]) {
      await idbPut(STORES.launch, 'current', bad)
      expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'invalid' })
    }
  })

  it('replaces the previous launch and can clear it', async () => {
    await saveLaunch(makeLaunch({ token: 'one', expires_at_ms: NOW + 1000 }))
    await saveLaunch(makeLaunch({ token: 'two', expires_at_ms: NOW + 1000 }))
    expect(await loadLaunch(NOW)).toMatchObject({ ok: true, launch: { token: 'two' } })
    await clearLaunch()
    expect(await loadLaunch(NOW)).toEqual({ ok: false, reason: 'none' })
  })

  it('validateLaunch accepts exactly what the fragment decoder accepts', () => {
    const launch = makeLaunch()
    expect(validateLaunch(launch)).toEqual(launch)
    expect(validateLaunch(JSON.parse(JSON.stringify(launch)))).toEqual(launch)
    expect(validateLaunch(null)).toBeUndefined()
    expect(validateLaunch(encodePreviewLaunch(launch))).toBeUndefined()
  })
})

describe('host label', () => {
  const label = 'abcdefghijklmnopqrstuvwxyz'

  it('is the first part of the host name', () => {
    expect(hostLabel(`${label}.aiplace.app`)).toBe(label)
    expect(hostLabel(`${label}.preview.localhost`)).toBe(label)
    expect(hostLabel('localhost')).toBe('localhost')
  })

  it('a launch matches only the preview origin it was made for', () => {
    const launch = makeLaunch()
    expect(launchMatchesHost(launch, `${label}.aiplace.app`)).toBe(true)
    expect(launchMatchesHost(launch, `${label}.preview.localhost`)).toBe(true)
    expect(launchMatchesHost(launch, 'zzzzzzzzzzzzzzzzzzzzzzzzzz.aiplace.app')).toBe(false)
    expect(launchMatchesHost(launch, 'hub.aiplace.app')).toBe(false)
    expect(launchMatchesHost(launch, 'localhost')).toBe(false)
    expect(launchMatchesHost(launch, '')).toBe(false)
  })
})
