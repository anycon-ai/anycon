// 拉取 blob 的字节（spec/web.md § 4.6「显示」、protocol.md § 5.6 `blob.get`）：
// 先查 IndexedDB 缓存，没有再 `blob.get` 分段拉（同一节点同时最多 2 张），拉完写缓存。
// 同一张图同时被多处要（缩略图 + 大图、两条消息引用同一个 blob）合并成一次请求；
// 要的人都走了（滚出屏幕、组件卸载）而请求还在排队时，直接取消，不白拉。
import type { AttachmentRef } from '@/protocol'
import { base64ToBytes } from '@/lib/base64'
import { AbortedError, createLimiter, withRetry, type Limiter } from './async'
import { readCached, writeCached, type CacheEntry } from './cache'
import { blobCall } from './rpc'

/** 同一节点同时拉几张图。 */
export const MAX_CONCURRENT_DOWNLOADS = 2
/** 每页最多拉多少字节（daemon 的上限也是 512 KiB）。 */
export const DOWNLOAD_PAGE_BYTES = 512 * 1024
const RETRIES = 2
/** 单张图的上限：超过这个字节数的 `blob.get` 结果不接受（daemon 的附件上限默认远小于它）。 */
const MAX_BLOB_BYTES = 64 * 1024 * 1024

const limiters = new Map<string, Limiter>()
const limiterFor = (nodeId: string): Limiter => {
  let l = limiters.get(nodeId)
  if (!l) limiters.set(nodeId, (l = createLimiter(MAX_CONCURRENT_DOWNLOADS)))
  return l
}

/** 测试用。 */
export function resetDownloads(): void {
  limiters.clear()
  inflight.clear()
}

interface Inflight {
  promise: Promise<CacheEntry>
  waiters: number
  started: boolean
  cancelled: boolean
}
const inflight = new Map<string, Inflight>()

export interface BlobHandle {
  promise: Promise<CacheEntry>
  /** 不再需要结果了。最后一个等的人放手而请求还没开始时，请求被取消（`promise` 以 `AbortedError` 结束）。 */
  release(): void
}

/** 从 daemon 分段拉整张图。`ref.bytes` 只用来防御：拉到的总字节数必须和 daemon 在响应里给的记录一致。 */
async function fetchRemote(nodeId: string, blobId: string): Promise<CacheEntry> {
  const parts: Uint8Array[] = []
  let offset = 0
  let mime = ''
  let expected = -1
  for (;;) {
    const at = offset
    const res = await withRetry(() => blobCall(nodeId, 'blob.get', { blob_id: blobId, offset: at, max_bytes: DOWNLOAD_PAGE_BYTES }), { retries: RETRIES, delayMs: 400 })
    if (res.offset !== at) throw new Error(`blob.get returned offset ${res.offset}, expected ${at}`)
    const chunk = base64ToBytes(res.data)
    mime = res.blob.mime
    expected = res.blob.bytes
    parts.push(chunk)
    offset += chunk.length
    if (offset > MAX_BLOB_BYTES) throw new Error('blob.get: the image is larger than the client accepts')
    if (res.eof) break
    if (chunk.length === 0) throw new Error('blob.get made no progress')
  }
  if (offset !== expected) throw new Error(`blob.get: got ${offset} bytes, the node says the image has ${expected}`)
  const bytes = new Uint8Array(offset)
  let at = 0
  for (const p of parts) {
    bytes.set(p, at)
    at += p.length
  }
  return { bytes, mime }
}

/**
 * 要一张图的字节。调用方用完（或不需要了）调 `release()`。
 * `ref` 里的 `mime` 在缓存和响应里都没有时的兜底不需要：缓存记录和响应都带 mime。
 */
export function acquireBlob(nodeId: string, ref: Pick<AttachmentRef, 'blob_id'>): BlobHandle {
  const key = `${nodeId}:${ref.blob_id}`
  let entry = inflight.get(key)
  if (!entry) {
    const created: Inflight = { promise: Promise.resolve({ bytes: new Uint8Array(0), mime: '' }), waiters: 0, started: false, cancelled: false }
    created.promise = (async () => {
      const hit = await readCached(ref.blob_id)
      if (hit) return hit
      return limiterFor(nodeId).run(async () => {
        if (created.cancelled) throw new AbortedError()
        created.started = true
        const fetched = await fetchRemote(nodeId, ref.blob_id)
        await writeCached(ref.blob_id, fetched.bytes, fetched.mime)
        return fetched
      })
    })().finally(() => {
      if (inflight.get(key) === created) inflight.delete(key)
    })
    // 没人接的 rejection（全都 release 了）不要变成 unhandledrejection
    created.promise.catch(() => {})
    inflight.set(key, created)
    entry = created
  }
  const current = entry
  current.waiters += 1
  let released = false
  return {
    promise: current.promise,
    release() {
      if (released) return
      released = true
      current.waiters -= 1
      if (current.waiters === 0 && !current.started) {
        current.cancelled = true
        if (inflight.get(key) === current) inflight.delete(key)
      }
    },
  }
}

/** 一次性取字节（不关心取消）。 */
export async function loadBlob(nodeId: string, ref: Pick<AttachmentRef, 'blob_id'>): Promise<CacheEntry> {
  const h = acquireBlob(nodeId, ref)
  try {
    return await h.promise
  } finally {
    h.release()
  }
}
