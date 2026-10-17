// 上传一张规整过的图（spec/web.md § 4.6「上传」、protocol.md § 5.6）：
// `blob.put_begin` → 每块 512 KiB、最多 2 块在途的 `blob.put_chunk` → `blob.put_end`。
// 每块都有答复：失败的块自己重试 2 次，不用整张重传；块与块之间让出主线程；取消 = `blob.discard`。
import type { AttachmentRef, BlobPutBeginResult } from '@/protocol'
import { bytesToBase64 } from '@/lib/base64'
import { warnOnError } from '@/lib/log'
import { AbortedError, isAborted, withRetry, yieldToMain } from './async'
import { writeCached } from './cache'
import type { NormalizedImage } from './normalize'
import { blobCall } from './rpc'
import { sha256Hex } from './sha256'

/**
 * 同时在途的块数。上传和这台设备的其它请求（允许 / 停止 / 发消息）共用一条有序的加密通道，
 * 在途的字节越多，排在后面的交互请求等得越久（经 hub 时还有客户端限速）：2 块 ≈ 1.4 MB（base64 后），最多让它们等一两秒。
 * 每个节点同时也只有一张图在上传（见 `composerUploads.ts`），所以整个节点在途的就是这 2 块。
 */
export const MAX_IN_FLIGHT_CHUNKS = 2
/** 失败的块（和 `put_end`）再试几次。 */
export const CHUNK_RETRIES = 2
const RETRY_DELAY_MS = 400
/**
 * 块和 put_end 的请求超时。一块 512 KiB（base64 后约 700 KB），慢速上行（几百 kbps 的手机网络）一块就要十几秒，
 * 默认的 30 秒会在数据还在路上时就判超时、白白重发。
 */
export const UPLOAD_REQUEST_TIMEOUT_MS = 90_000
/** daemon 给的 `chunk_size` 不会离谱；超出这个范围当作协议错误，不照着切。 */
const MAX_CHUNK_BYTES = 4 * 1024 * 1024

export interface UploadHooks {
  signal?: AbortSignal
  /** 已确认的字节数 / 总字节数，0..1。 */
  onProgress?: (fraction: number) => void
  /** 重试前等多久（× 第几次重试）；测试里设 0。 */
  retryDelayMs?: number
}

/**
 * 等 `p`，但 `signal` 中止时立刻放弃（抛 `AbortedError`；在途的请求还会跑完，结果不再有人要）。
 * 每个节点同时只有一张图在上传：被移除的图不能让队列里的下一张等它的请求超时（最长 90 秒）。
 */
function raceAbort<T>(p: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return p
  return new Promise<T>((resolve, reject) => {
    const onAbort = (): void => reject(new AbortedError())
    if (signal.aborted) return onAbort()
    signal.addEventListener('abort', onAbort, { once: true })
    p.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort))
  })
}

/** 丢掉一个进行中的上传，或已完成但还没被消息引用的 blob。失败只记 warn：daemon 会自己回收（10 分钟 / 24 小时）。 */
export function discardBlob(nodeId: string, blobId: string): void {
  blobCall(nodeId, 'blob.discard', { blob_id: blobId }).catch(warnOnError('discard the attachment on the node'))
}

/**
 * 上传并返回 daemon 的记录（`AttachmentRef`，含宽高）。成功后立刻写进图片缓存，之后显示不用再拉。
 * 失败 / 中止时尽力 `blob.discard`，原样抛出错误（中止抛 `AbortedError`）。
 */
export async function uploadImage(nodeId: string, image: NormalizedImage, hooks: UploadHooks = {}): Promise<AttachmentRef> {
  const { signal, onProgress } = hooks
  const delayMs = hooks.retryDelayMs ?? RETRY_DELAY_MS
  const aborted = (): boolean => signal?.aborted === true
  if (aborted()) throw new AbortedError()

  const size = image.bytes.length
  const sha256 = await sha256Hex(image.bytes)
  if (aborted()) throw new AbortedError()
  const beginReq = blobCall(nodeId, 'blob.put_begin', { name: image.name, mime: image.mime, size, sha256 })
  let begin: BlobPutBeginResult
  try {
    begin = await raceAbort(beginReq, signal)
  } catch (e) {
    // 等 put_begin 的时候被中止了：它迟早会答复，到时候把这个刚开的上传丢掉
    if (isAborted(e)) beginReq.then((b) => discardBlob(nodeId, b.blob_id), () => {})
    throw e
  }
  const blobId = begin.blob_id
  try {
    if (aborted()) throw new AbortedError()
    const chunkSize = begin.chunk_size
    if (!Number.isInteger(chunkSize) || chunkSize <= 0 || chunkSize > MAX_CHUNK_BYTES) throw new Error(`blob.put_begin returned an unusable chunk_size (${chunkSize})`)
    const total = Math.ceil(size / chunkSize)
    let next = 0
    let confirmed = 0
    let failure: { error: unknown } | null = null
    onProgress?.(0)

    const worker = async (): Promise<void> => {
      while (failure === null && !aborted()) {
        const index = next++
        if (index >= total) return
        await yieldToMain()
        if (failure !== null || aborted()) return
        const start = index * chunkSize
        const slice = image.bytes.subarray(start, Math.min(size, start + chunkSize))
        try {
          await withRetry(() => blobCall(nodeId, 'blob.put_chunk', { blob_id: blobId, index, data: bytesToBase64(slice) }, UPLOAD_REQUEST_TIMEOUT_MS), { retries: CHUNK_RETRIES, delayMs, signal })
        } catch (e) {
          failure ??= { error: e }
          return
        }
        confirmed += slice.length
        onProgress?.(confirmed / size)
      }
    }
    await raceAbort(Promise.all(Array.from({ length: Math.min(MAX_IN_FLIGHT_CHUNKS, total) }, worker)), signal)
    if (failure !== null) throw (failure as { error: unknown }).error
    if (aborted()) throw new AbortedError()

    // put_end 对已完成的 blob 重复调用返回同一个结果，所以可以放心重试
    const ref = await raceAbort(withRetry(() => blobCall(nodeId, 'blob.put_end', { blob_id: blobId }, UPLOAD_REQUEST_TIMEOUT_MS), { retries: CHUNK_RETRIES, delayMs, signal }), signal)
    // 传完的同时用户移除了它：不再有人引用，下面的 catch 会丢掉
    if (aborted()) throw new AbortedError()
    await writeCached(ref.blob_id, image.bytes, ref.mime)
    onProgress?.(1)
    return ref
  } catch (e) {
    discardBlob(nodeId, blobId)
    throw e
  }
}
