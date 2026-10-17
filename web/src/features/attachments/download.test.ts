import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef, BlobGetParams, BlobGetResult } from '@/protocol'
import { resetDb } from '@/db'
import { bytesToBase64 } from '@/lib/base64'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { isAborted } from './async'
import { readCached, resetBlobCache, writeCached } from './cache'
import { acquireBlob, DOWNLOAD_PAGE_BYTES, loadBlob, MAX_CONCURRENT_DOWNLOADS, resetDownloads } from './download'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const id = (n: number): string => n.toString(16).padStart(32, '0')
const content = (size: number, seed = 1): Uint8Array => Uint8Array.from({ length: size }, (_, i) => (i * 7 + seed) % 256)

/** 假 daemon 的 blob.get：按 offset/max_bytes 切页。`pageSize` 小一点，省得分配 MB 级数据。 */
function serve(blobs: Record<string, Uint8Array>, opts: { pageSize?: number; delayMs?: number; mime?: string } = {}) {
  const active = { now: 0, peak: 0 }
  const requested: string[] = []
  rpc.handlers['blob.get'] = async (p) => {
    const params = p as BlobGetParams
    const all = blobs[params.blob_id]
    if (!all) throw rpcError('not_found', undefined, 'blob')
    requested.push(`${params.blob_id}@${params.offset ?? 0}`)
    active.now += 1
    active.peak = Math.max(active.peak, active.now)
    if (opts.delayMs) await new Promise((r) => setTimeout(r, opts.delayMs))
    active.now -= 1
    const offset = params.offset ?? 0
    const end = Math.min(all.length, offset + Math.min(params.max_bytes ?? 512 * 1024, opts.pageSize ?? 512 * 1024))
    const blob: AttachmentRef = { blob_id: params.blob_id, name: 'a.png', mime: opts.mime ?? 'image/png', bytes: all.length }
    const res: BlobGetResult = { blob, offset, data: bytesToBase64(all.subarray(offset, end)), eof: end >= all.length }
    return res
  }
  return { active, requested }
}

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  resetDownloads()
  rpc = new FakeRpc()
  setRpc(rpc)
})
afterEach(() => setRpc(null))

describe('loadBlob', () => {
  it('serves from the cache without asking the node', async () => {
    await writeCached(id(1), content(30), 'image/webp')
    const hit = await loadBlob(NODE, { blob_id: id(1) })
    expect(hit.mime).toBe('image/webp')
    expect(rpc.calls).toEqual([])
  })

  it('pulls a multi-page blob with blob.get, stitches the pages, and writes the cache', async () => {
    const data = content(2500)
    const s = serve({ [id(1)]: data }, { pageSize: 1000 })
    const got = await loadBlob(NODE, { blob_id: id(1) })
    expect(Array.from(got.bytes)).toEqual(Array.from(data))
    expect(got.mime).toBe('image/png')
    expect(s.requested).toEqual([`${id(1)}@0`, `${id(1)}@1000`, `${id(1)}@2000`])
    expect(rpc.calls.every((c) => (c.params as BlobGetParams).max_bytes === DOWNLOAD_PAGE_BYTES)).toBe(true)
    // 第二次从缓存来
    rpc.calls = []
    await loadBlob(NODE, { blob_id: id(1) })
    expect(rpc.calls).toEqual([])
    expect(Array.from((await readCached(id(1)))!.bytes)).toEqual(Array.from(data))
  })

  it('pulls at most 2 images at a time per node, and nodes do not share the limit', async () => {
    const blobs: Record<string, Uint8Array> = {}
    for (let i = 1; i <= 6; i++) blobs[id(i)] = content(100, i)
    const s = serve(blobs, { delayMs: 10 })
    await Promise.all([1, 2, 3, 4, 5, 6].map((i) => loadBlob(NODE, { blob_id: id(i) })))
    expect(s.active.peak).toBe(MAX_CONCURRENT_DOWNLOADS)

    s.active.peak = 0
    resetDownloads()
    resetBlobCache()
    await resetDb()
    // 两个节点各 2 张：最多 4 张同时
    const other = 'o'.repeat(32)
    await Promise.all([1, 2, 3, 4].map((i, k) => loadBlob(k % 2 ? other : NODE, { blob_id: id(i) })))
    expect(s.active.peak).toBeLessThanOrEqual(4)
    expect(s.active.peak).toBeGreaterThan(2)
  })

  it('merges concurrent requests for the same image into one fetch', async () => {
    const s = serve({ [id(1)]: content(50) }, { delayMs: 5 })
    const [a, b] = await Promise.all([loadBlob(NODE, { blob_id: id(1) }), loadBlob(NODE, { blob_id: id(1) })])
    expect(s.requested).toHaveLength(1)
    expect(a.bytes).toBe(b.bytes)
  })

  it('cancels a queued fetch when everyone waiting for it lets go, but not one that already started', async () => {
    const blobs: Record<string, Uint8Array> = {}
    for (let i = 1; i <= 4; i++) blobs[id(i)] = content(40, i)
    const s = serve(blobs, { delayMs: 20 })
    const handles = [1, 2, 3, 4].map((i) => acquireBlob(NODE, { blob_id: id(i) }))
    // 1、2 已经开始，3、4 在排队
    await new Promise((r) => setTimeout(r, 5))
    handles[2]!.release()
    handles[3]!.release()
    const first = await handles[0]!.promise
    await handles[1]!.promise
    expect(first.bytes.length).toBe(40)
    expect(isAborted(await handles[2]!.promise.catch((e: unknown) => e))).toBe(true)
    expect(isAborted(await handles[3]!.promise.catch((e: unknown) => e))).toBe(true)
    expect(s.requested.sort()).toEqual([`${id(1)}@0`, `${id(2)}@0`])
    handles[0]!.release()
    handles[1]!.release()

    // 取消过的图以后还能重新要
    const again = await loadBlob(NODE, { blob_id: id(3) })
    expect(again.bytes.length).toBe(40)
  })

  it('keeps a fetch going while somebody still waits for it', async () => {
    serve({ [id(1)]: content(40) }, { delayMs: 5 })
    const a = acquireBlob(NODE, { blob_id: id(1) })
    const b = acquireBlob(NODE, { blob_id: id(1) })
    a.release()
    expect((await b.promise).bytes.length).toBe(40)
    b.release()
  })

  it('fails on not_found, and does not cache anything', async () => {
    serve({})
    await expect(loadBlob(NODE, { blob_id: id(9) })).rejects.toMatchObject({ code: 'not_found', reason: 'blob' })
    expect(await readCached(id(9))).toBeNull()
  })

  it('retries a transient failure of a page', async () => {
    const data = content(100)
    serve({ [id(1)]: data })
    const inner = rpc.handlers['blob.get']!
    let failed = 0
    rpc.handlers['blob.get'] = (p, n) => {
      if (failed++ === 0) throw new RpcCallError('timeout', 'blob.get: timed out')
      return inner(p, n)
    }
    vi.useFakeTimers()
    const p = loadBlob(NODE, { blob_id: id(1) })
    await vi.advanceTimersByTimeAsync(1000)
    expect(Array.from((await p).bytes)).toEqual(Array.from(data))
    vi.useRealTimers()
  })

  it('refuses a result whose length disagrees with the record, instead of caching a truncated image', async () => {
    rpc.handlers['blob.get'] = () => ({ blob: { blob_id: id(1), name: 'a.png', mime: 'image/png', bytes: 500 }, offset: 0, data: bytesToBase64(content(100)), eof: true })
    await expect(loadBlob(NODE, { blob_id: id(1) })).rejects.toThrow(/500/)
    expect(await readCached(id(1))).toBeNull()
  })

  it('refuses a page at the wrong offset or a non-final empty page', async () => {
    rpc.handlers['blob.get'] = () => ({ blob: { blob_id: id(1), name: 'a.png', mime: 'image/png', bytes: 100 }, offset: 5, data: bytesToBase64(content(10)), eof: false })
    await expect(loadBlob(NODE, { blob_id: id(1) })).rejects.toThrow(/offset/)
    resetDownloads()
    rpc.handlers['blob.get'] = () => ({ blob: { blob_id: id(2), name: 'a.png', mime: 'image/png', bytes: 100 }, offset: 0, data: '', eof: false })
    await expect(loadBlob(NODE, { blob_id: id(2) })).rejects.toThrow(/progress/)
  })
})
