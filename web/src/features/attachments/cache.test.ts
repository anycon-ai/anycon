import { beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { CACHE_LIMIT_BYTES, pickEvictions, readCached, resetBlobCache, TOUCH_AFTER_MS, writeCached } from './cache'

const bytes = (n: number, fill = 1): Uint8Array => new Uint8Array(n).fill(fill)

beforeEach(async () => {
  await db.resetDb()
  resetBlobCache()
})

describe('pickEvictions', () => {
  const e = (id: string, size: number, last: number) => ({ blob_id: id, size, last_used_ms: last })

  it('does nothing while the total is within the limit', () => {
    expect(pickEvictions([e('a', 50, 1), e('b', 50, 2)], 100)).toEqual([])
  })

  it('drops the least recently used first, and stops as soon as the total fits', () => {
    expect(pickEvictions([e('new', 40, 30), e('old', 40, 10), e('mid', 40, 20)], 100)).toEqual(['old'])
    expect(pickEvictions([e('new', 40, 30), e('old', 40, 10), e('mid', 40, 20)], 50)).toEqual(['old', 'mid'])
  })

  it('never drops the entry that was just written, even when it is the oldest', () => {
    expect(pickEvictions([e('fresh', 60, 1), e('x', 60, 2)], 100, 'fresh')).toEqual(['x'])
  })

  it('is stable for equal timestamps', () => {
    expect(pickEvictions([e('b', 10, 5), e('a', 10, 5), e('c', 10, 5)], 10)).toEqual(['a', 'b'])
  })
})

describe('the image cache', () => {
  it('stores bytes with their mime and reads them back; an unknown id is a miss', async () => {
    expect(await readCached('missing')).toBeNull()
    await writeCached('a', bytes(10, 7), 'image/png')
    const hit = await readCached('a')
    expect(hit?.mime).toBe('image/png')
    expect(Array.from(hit!.bytes)).toEqual(Array.from(bytes(10, 7)))
  })

  it('treats a record in the old shape (no mime) as a cache miss, and removes it on the first write', async () => {
    const d = await db.getDb()
    await d.put('blobs', { blob_id: 'old', bytes: bytes(5) } as unknown as db.BlobRecord)
    expect(await db.getBlobRecord('old')).toBeUndefined()
    expect(await readCached('old')).toBeNull()
    await writeCached('new', bytes(5), 'image/jpeg')
    expect(await d.get('blobs', 'old')).toBeUndefined()
    expect(await readCached('new')).not.toBeNull()
  })

  it('evicts by last_used_ms once the total goes over the limit, and a recent read protects an entry', async () => {
    const limit = 100
    await writeCached('a', bytes(40), 'image/png', { now: 1000, limit })
    await writeCached('b', bytes(40), 'image/png', { now: 2000, limit })
    // 读一下 a，它就比 b 新了
    await readCached('a', 3000)
    await writeCached('c', bytes(40), 'image/png', { now: 4000, limit })
    expect(await readCached('b')).toBeNull()
    expect(await readCached('a')).not.toBeNull()
    expect(await readCached('c')).not.toBeNull()
  })

  it('keeps the 200 MiB cap as the default', () => {
    expect(CACHE_LIMIT_BYTES).toBe(200 * 1024 * 1024)
  })

  it('rewrites the stored timestamp on a read only when it is stale', async () => {
    await writeCached('a', bytes(8), 'image/png', { now: 1000 })
    const put = vi.spyOn(db, 'putBlobRecord')
    await readCached('a', 1000 + 1000)
    expect(put).not.toHaveBeenCalled()
    await readCached('a', 1000 + TOUCH_AFTER_MS + 1)
    expect(put).toHaveBeenCalledTimes(1)
    await vi.waitFor(async () => expect((await db.getBlobRecord('a'))?.last_used_ms).toBe(1000 + TOUCH_AFTER_MS + 1))
  })

  it('a failing cache write only warns', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    vi.spyOn(db, 'putBlobRecord').mockRejectedValue(new Error('QuotaExceededError'))
    await expect(writeCached('a', bytes(8), 'image/png')).resolves.toBeUndefined()
    expect(warn).toHaveBeenCalled()
  })

  it('a failing cache read is a miss', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    vi.spyOn(db, 'getBlobRecord').mockRejectedValue(new Error('closed'))
    expect(await readCached('a')).toBeNull()
  })
})
