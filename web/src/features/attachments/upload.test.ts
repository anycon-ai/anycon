import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef, BlobPutBeginParams, BlobPutChunkParams } from '@/protocol'
import { resetDb } from '@/db'
import { base64ToBytes } from '@/lib/base64'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { isAborted } from './async'
import { readCached, resetBlobCache } from './cache'
import type { NormalizedImage } from './normalize'
import { sha256Hex } from './sha256'
import { CHUNK_RETRIES, UPLOAD_REQUEST_TIMEOUT_MS, uploadImage } from './upload'

const NODE = 'n'.repeat(32)
const BLOB = 'b'.repeat(32)
let rpc: FakeRpc

function image(size: number, over: Partial<NormalizedImage> = {}): NormalizedImage {
  return { bytes: Uint8Array.from({ length: size }, (_, i) => (i * 13 + 5) % 256), mime: 'image/png', name: 'shot.png', width: 40, height: 30, ...over }
}

/** 一个会记录收到的块的假 daemon；`chunkSize` 用小值，免得测试里分配几 MB。 */
function daemon(chunkSize: number, over: { chunk?: (p: BlobPutChunkParams, attempt: number) => unknown | Promise<unknown>; end?: (attempt: number) => unknown } = {}) {
  const received = new Map<number, Uint8Array>()
  const attempts = new Map<number, number>()
  let begin: BlobPutBeginParams | null = null
  let endAttempts = 0
  rpc.handlers['blob.put_begin'] = (p) => {
    begin = p as BlobPutBeginParams
    return { blob_id: BLOB, chunk_size: chunkSize, expires_at_ms: Date.now() + 600_000 }
  }
  rpc.handlers['blob.put_chunk'] = async (p) => {
    const params = p as BlobPutChunkParams
    const n = (attempts.get(params.index) ?? 0) + 1
    attempts.set(params.index, n)
    await over.chunk?.(params, n)
    received.set(params.index, base64ToBytes(params.data))
    return {}
  }
  rpc.handlers['blob.put_end'] = () => {
    endAttempts += 1
    over.end?.(endAttempts)
    const b = begin!
    const ref: AttachmentRef = { blob_id: BLOB, name: b.name, mime: b.mime, bytes: b.size, width: 40, height: 30 }
    return ref
  }
  rpc.handlers['blob.discard'] = () => ({})
  return { received, attempts, begin: () => begin, endAttempts: () => endAttempts }
}

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  rpc = new FakeRpc()
  setRpc(rpc)
})
afterEach(() => setRpc(null))

describe('uploadImage', () => {
  it('sends put_begin with the file facts, then every chunk at its index, then put_end, and returns the daemon record', async () => {
    const img = image(2500)
    const d = daemon(1024)
    const progress: number[] = []
    const ref = await uploadImage(NODE, img, { retryDelayMs: 0, onProgress: (f) => progress.push(f) })

    expect(d.begin()).toEqual({ name: 'shot.png', mime: 'image/png', size: 2500, sha256: await sha256Hex(img.bytes) })
    expect([...d.received.keys()].sort()).toEqual([0, 1, 2])
    expect(d.received.get(0)).toEqual(img.bytes.subarray(0, 1024))
    expect(d.received.get(2)).toEqual(img.bytes.subarray(2048))
    expect(d.received.get(2)!.length).toBe(452) // 最后一块是余数
    expect(rpc.methods()).toEqual(['blob.put_begin', 'blob.put_chunk', 'blob.put_chunk', 'blob.put_chunk', 'blob.put_end'])
    expect(ref).toMatchObject({ blob_id: BLOB, bytes: 2500, width: 40, height: 30 })
    expect(progress[0]).toBe(0)
    expect(progress.at(-1)).toBe(1)
    expect([...progress].sort((a, b) => a - b)).toEqual(progress)
  })

  it('writes the uploaded bytes to the image cache right after put_end', async () => {
    const img = image(1500)
    daemon(1024)
    const ref = await uploadImage(NODE, img, { retryDelayMs: 0 })
    const hit = await readCached(ref.blob_id)
    expect(hit?.mime).toBe('image/png')
    // fake-indexeddb 的结构化克隆在另一个 realm 里，toEqual 比的是原型：转成数组再比
    expect(Array.from(hit!.bytes)).toEqual(Array.from(img.bytes))
  })

  it('keeps at most 2 chunks in flight (so a permission answer or a stop does not wait behind megabytes of image)', async () => {
    let active = 0
    let peak = 0
    const d = daemon(100, {
      chunk: async () => {
        active += 1
        peak = Math.max(peak, active)
        await new Promise((r) => setTimeout(r, 5))
        active -= 1
      },
    })
    await uploadImage(NODE, image(1500), { retryDelayMs: 0 })
    expect(d.received.size).toBe(15)
    expect(peak).toBe(2)
  })

  it('retries a failed chunk (twice) without resending the others, then succeeds', async () => {
    const d = daemon(1024, {
      chunk: (p, attempt) => {
        if (p.index === 1 && attempt <= CHUNK_RETRIES) throw new RpcCallError('timeout', 'put_chunk: timed out')
      },
    })
    await uploadImage(NODE, image(3000), { retryDelayMs: 0 })
    expect(d.attempts.get(1)).toBe(3)
    expect(d.attempts.get(0)).toBe(1)
    expect(d.attempts.get(2)).toBe(1)
    expect(rpc.methods().at(-1)).toBe('blob.put_end')
  })

  it('fails (and discards the upload) when a chunk still fails after the retries, and never calls put_end', async () => {
    daemon(1024, {
      chunk: (p) => {
        if (p.index === 1) throw new RpcCallError('offline', 'node link not ready')
      },
    })
    await expect(uploadImage(NODE, image(3000), { retryDelayMs: 0 })).rejects.toMatchObject({ kind: 'offline' })
    expect(rpc.methods()).not.toContain('blob.put_end')
    expect(rpc.calls.filter((c) => c.method === 'blob.discard')).toHaveLength(1)
    expect(rpc.calls.find((c) => c.method === 'blob.discard')!.params).toEqual({ blob_id: BLOB })
    // 第 1 块试了 1 + 2 次
    expect(rpc.calls.filter((c) => c.method === 'blob.put_chunk' && (c.params as BlobPutChunkParams).index === 1)).toHaveLength(3)
  })

  it('does not retry a definite refusal from the daemon', async () => {
    const d = daemon(1024, {
      chunk: () => {
        throw rpcError('not_found')
      },
    })
    await expect(uploadImage(NODE, image(900), { retryDelayMs: 0 })).rejects.toMatchObject({ code: 'not_found' })
    expect(d.attempts.get(0)).toBe(1)
  })

  it('retries put_end (it is idempotent) and surfaces a refusal such as hash_mismatch', async () => {
    const d = daemon(1024, {
      end: (n) => {
        if (n === 1) throw new RpcCallError('timeout', 'put_end: timed out')
      },
    })
    await uploadImage(NODE, image(900), { retryDelayMs: 0 })
    expect(d.endAttempts()).toBe(2)

    rpc.calls = []
    rpc.handlers['blob.put_end'] = () => {
      throw rpcError('invalid', undefined, 'hash_mismatch')
    }
    await expect(uploadImage(NODE, image(900), { retryDelayMs: 0 })).rejects.toMatchObject({ code: 'invalid', reason: 'hash_mismatch' })
    expect(rpc.methods().filter((m) => m === 'blob.put_end')).toHaveLength(1)
  })

  it('does not discard anything when put_begin itself is refused', async () => {
    rpc.handlers['blob.put_begin'] = () => {
      throw rpcError('invalid', undefined, 'too_large')
    }
    await expect(uploadImage(NODE, image(900), { retryDelayMs: 0 })).rejects.toMatchObject({ reason: 'too_large' })
    expect(rpc.methods()).toEqual(['blob.put_begin'])
  })

  it('aborting mid-upload stops sending chunks, discards the upload and never calls put_end', async () => {
    const ctl = new AbortController()
    const d = daemon(100, {
      chunk: async (p) => {
        if (p.index === 2) ctl.abort()
        await new Promise((r) => setTimeout(r, 2))
      },
    })
    const err = await uploadImage(NODE, image(5000), { retryDelayMs: 0, signal: ctl.signal }).catch((e: unknown) => e)
    expect(isAborted(err)).toBe(true)
    expect(rpc.methods()).not.toContain('blob.put_end')
    expect(d.received.size).toBeLessThan(50)
    expect(rpc.calls.filter((c) => c.method === 'blob.discard')).toHaveLength(1)
  })

  it('aborting before it starts sends nothing; aborting while put_begin is in flight discards the new upload', async () => {
    const before = new AbortController()
    before.abort()
    daemon(1024)
    expect(isAborted(await uploadImage(NODE, image(100), { signal: before.signal }).catch((e: unknown) => e))).toBe(true)
    expect(rpc.calls).toEqual([])

    const ctl = new AbortController()
    const inner = rpc.handlers['blob.put_begin']!
    rpc.handlers['blob.put_begin'] = async (p, n) => {
      ctl.abort()
      return inner(p, n)
    }
    expect(isAborted(await uploadImage(NODE, image(100), { signal: ctl.signal }).catch((e: unknown) => e))).toBe(true)
    await vi.waitFor(() => expect(rpc.methods()).toEqual(['blob.put_begin', 'blob.discard']))
  })

  it('an abort does not wait for a request that never answers: the upload returns at once (the node queue moves on), and still discards', async () => {
    const ctl = new AbortController()
    daemon(100, { chunk: () => new Promise(() => {}) })
    const p = uploadImage(NODE, image(1000), { retryDelayMs: 0, signal: ctl.signal })
    await vi.waitFor(() => expect(rpc.methods()).toContain('blob.put_chunk'))
    ctl.abort()
    // 请求本身永远不返回；没有 raceAbort 的话这里要等 90 秒的超时
    expect(isAborted(await p.catch((e: unknown) => e))).toBe(true)
    expect(rpc.calls.filter((c) => c.method === 'blob.discard')).toHaveLength(1)

    // put_begin 还没答复就中止：答复来了以后把刚开的上传丢掉
    rpc.calls = []
    const ctl2 = new AbortController()
    let answer: (v: unknown) => void = () => {}
    rpc.handlers['blob.put_begin'] = () => new Promise((resolve) => (answer = resolve))
    const q = uploadImage(NODE, image(100), { signal: ctl2.signal })
    await vi.waitFor(() => expect(rpc.methods()).toEqual(['blob.put_begin']))
    ctl2.abort()
    expect(isAborted(await q.catch((e: unknown) => e))).toBe(true)
    answer({ blob_id: BLOB, chunk_size: 100, expires_at_ms: Date.now() + 1000 })
    await vi.waitFor(() => expect(rpc.methods()).toEqual(['blob.put_begin', 'blob.discard']))
  })

  it('rejects a nonsensical chunk_size from the daemon instead of slicing by it', async () => {
    rpc.handlers['blob.put_begin'] = () => ({ blob_id: BLOB, chunk_size: 0, expires_at_ms: 1 })
    rpc.handlers['blob.discard'] = () => ({})
    await expect(uploadImage(NODE, image(100), { retryDelayMs: 0 })).rejects.toThrow(/chunk_size/)
    expect(rpc.methods()).toEqual(['blob.put_begin', 'blob.discard'])
  })

  it('treats a lost write to the image cache as harmless', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const db = await import('@/db')
    vi.spyOn(db, 'putBlobRecord').mockRejectedValue(new Error('quota'))
    daemon(1024)
    await expect(uploadImage(NODE, image(500), { retryDelayMs: 0 })).resolves.toMatchObject({ blob_id: BLOB })
  })

  it('gives chunks and put_end a longer timeout than a normal request (slow uplinks), and leaves the others at the default', async () => {
    const seen: Array<[string, number | undefined]> = []
    daemon(1024)
    setRpc({
      isReady: () => true,
      request: async <R>(nodeId: string, method: string, params?: unknown, timeoutMs?: number) => {
        seen.push([method, timeoutMs])
        return (await rpc.request(nodeId, method, params)) as R
      },
    })
    await uploadImage(NODE, image(1500), { retryDelayMs: 0 })
    expect(seen).toEqual([
      ['blob.put_begin', undefined],
      ['blob.put_chunk', UPLOAD_REQUEST_TIMEOUT_MS],
      ['blob.put_chunk', UPLOAD_REQUEST_TIMEOUT_MS],
      ['blob.put_end', UPLOAD_REQUEST_TIMEOUT_MS],
    ])
    expect(UPLOAD_REQUEST_TIMEOUT_MS).toBeGreaterThan(30_000)
  })
})
