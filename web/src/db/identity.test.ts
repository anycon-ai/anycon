import { beforeEach, describe, expect, it } from 'vitest'
import { clearDeviceToken, getIdentity, mergeIdentity, putIdentity, resetDb, type IdentityRecord } from '@/db'

const key = (n: number) => ({ public: new Uint8Array(32).fill(n), secret: new Uint8Array(32).fill(n + 100) })

function rec(over: Partial<IdentityRecord> = {}): IdentityRecord {
  return { id: 'self', static_keypair: key(1), device_name: 'Chrome', device_id: 'dev1', device_token: 'tok1', device_token_issued_at_ms: 1000, ...over }
}

beforeEach(async () => {
  await resetDb()
})

describe('mergeIdentity', () => {
  it('keeps the stored token when it was issued strictly later (a stale tab writing back)', () => {
    const stored = rec({ device_token: 'tok2', device_token_issued_at_ms: 2000, device_id: 'dev2' })
    const stale = rec({ device_name: 'Renamed' })
    expect(mergeIdentity(stored, stale)).toMatchObject({ device_name: 'Renamed', device_token: 'tok2', device_token_issued_at_ms: 2000, device_id: 'dev2' })
  })

  it('lets the incoming token win when it is newer or as new, or the same', () => {
    const stored = rec({ device_token: 'tok1', device_token_issued_at_ms: 1000 })
    expect(mergeIdentity(stored, rec({ device_token: 'tok2', device_token_issued_at_ms: 2000 })).device_token).toBe('tok2')
    expect(mergeIdentity(stored, rec({ device_token: 'tok2', device_token_issued_at_ms: 1000 })).device_token).toBe('tok2')
    expect(mergeIdentity(stored, rec({ device_name: 'x' })).device_token).toBe('tok1')
  })

  it('counts an unknown issue time as 0', () => {
    const stored = rec({ device_token: 'tok2', device_token_issued_at_ms: 5 })
    const legacy = rec({ device_token: 'tok1' })
    delete legacy.device_token_issued_at_ms
    expect(mergeIdentity(stored, legacy).device_token).toBe('tok2')
    // 库里的是旧记录（没有签发时间），写入的有：写入的赢
    const storedLegacy = rec({ device_token: 'tok2' })
    delete storedLegacy.device_token_issued_at_ms
    expect(mergeIdentity(storedLegacy, rec({ device_token: 'tok1' })).device_token).toBe('tok1')
  })

  it('writes the record as it is when it has no token (an explicit clear), when nothing is stored, or when the identity differs', () => {
    const stored = rec({ device_token: 'tok2', device_token_issued_at_ms: 2000 })
    const cleared = rec()
    delete cleared.device_token
    delete cleared.device_token_issued_at_ms
    expect(mergeIdentity(stored, cleared)).toBe(cleared)
    const incoming = rec()
    expect(mergeIdentity(undefined, incoming)).toBe(incoming)
    const other = rec({ static_keypair: key(7) })
    expect(mergeIdentity(stored, other)).toBe(other)
  })
})

describe('putIdentity / clearDeviceToken (IndexedDB)', () => {
  it('a stale write does not put an older token over the newer stored one', async () => {
    await putIdentity(rec({ device_token: 'tok2', device_token_issued_at_ms: 2000 }))
    await putIdentity(rec({ device_name: 'Renamed' })) // 另一个标签页手里还是 tok1
    expect(await getIdentity()).toMatchObject({ device_name: 'Renamed', device_token: 'tok2', device_token_issued_at_ms: 2000 })
  })

  it('a newer token replaces the stored one', async () => {
    await putIdentity(rec())
    await putIdentity(rec({ device_token: 'tok2', device_token_issued_at_ms: 3000 }))
    expect(await getIdentity()).toMatchObject({ device_token: 'tok2', device_token_issued_at_ms: 3000 })
  })

  it('clearDeviceToken clears only the expected token and takes the device id and issue time with it', async () => {
    await putIdentity(rec())
    expect(await clearDeviceToken('other')).toBe(false)
    expect(await getIdentity()).toMatchObject({ device_token: 'tok1' })
    expect(await clearDeviceToken('tok1')).toBe(true)
    const after = await getIdentity()
    expect(after).toMatchObject({ id: 'self', device_name: 'Chrome' })
    expect(after?.device_token).toBeUndefined()
    expect(after?.device_id).toBeUndefined()
    expect(after?.device_token_issued_at_ms).toBeUndefined()
    // fake-indexeddb 的结构化克隆在另一个 realm 里，toEqual 比的是原型：转成数组再比
    expect(Array.from(after!.static_keypair.public)).toEqual(Array.from(key(1).public))
  })

  it('clearDeviceToken on an empty store does nothing', async () => {
    expect(await clearDeviceToken('tok1')).toBe(false)
    expect(await getIdentity()).toBeUndefined()
  })
})
