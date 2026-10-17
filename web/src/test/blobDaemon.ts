// 假 daemon 的 blob.* 方法（测试用）：记录收到的块，put_end 返回 daemon 风格的 AttachmentRef。
// 每次 put_begin 生成一个新的 blob_id（`b…001`、`b…002` …），多张图的测试里互相区分得开。
//
// 像真 daemon 一样限流：同一时间进行中（put_begin 之后、还没 put_end / discard）的上传最多 8 个，第 9 个 put_begin 回 `busy`
// （protocol.md § 5.6，`MAX_UPLOADS_PER_DEVICE`）。另外记下并发的峰值（`peak`），测试里直接断言写死的数字，
// 不拿代码里的常量跟自己比。
import type { AttachmentRef, BlobPutBeginParams, BlobPutChunkParams } from '@/protocol'
import { base64ToBytes } from '@/lib/base64'
import { rpcError, type FakeRpc } from './fakeRpc'

/** 真 daemon 每台设备同时进行中的上传上限。 */
export const DAEMON_MAX_UPLOADS = 8

export interface BlobDaemonOptions {
  chunkSize?: number
  /** 每块到达时调用；抛错 / 延迟用来模拟失败和慢速。`attempt` 从 1 数起（同一块的第几次）。 */
  onChunk?: (p: BlobPutChunkParams, attempt: number) => unknown | Promise<unknown>
  /** put_end 到达时调用（`attempt` 同上）。 */
  onEnd?: (blobId: string, attempt: number) => void
  /** put_begin 到达时调用（已经通过「最多 8 个」的检查），可抛错。 */
  onBegin?: (p: BlobPutBeginParams) => void
}

export interface BlobDaemon {
  /** blob_id → 收到的块（index → 字节）。 */
  uploads: Map<string, { begin: BlobPutBeginParams; chunks: Map<number, Uint8Array>; ended: boolean; discarded: boolean }>
  chunkAttempts: Map<string, number>
  discarded: string[]
  /** 现在进行中的上传数（开了、还没结束也没丢弃）。 */
  inProgress: () => number
  /** 现在在途的 `put_chunk` 数（到达了、还没答复，含 `onChunk` 里的延迟）。 */
  chunksInFlight: () => number
  /** 观察到的并发峰值：同时进行中的上传、同时在途的块。 */
  peak: { uploads: number; chunks: number }
}

export const blobId = (n: number): string => `b${n.toString(16).padStart(31, '0')}`

export function installBlobDaemon(rpc: FakeRpc, opts: BlobDaemonOptions = {}): BlobDaemon {
  const uploads: BlobDaemon['uploads'] = new Map()
  const endAttempts = new Map<string, number>()
  let counter = 0
  let chunks = 0
  const inProgress = (): number => [...uploads.values()].filter((u) => !u.ended && !u.discarded).length
  const d: BlobDaemon = {
    uploads,
    chunkAttempts: new Map(),
    discarded: [],
    inProgress,
    chunksInFlight: () => chunks,
    peak: { uploads: 0, chunks: 0 },
  }
  rpc.handlers['blob.put_begin'] = (p) => {
    const params = p as BlobPutBeginParams
    if (inProgress() >= DAEMON_MAX_UPLOADS) throw rpcError('busy')
    opts.onBegin?.(params)
    counter += 1
    const id = blobId(counter)
    uploads.set(id, { begin: params, chunks: new Map(), ended: false, discarded: false })
    d.peak.uploads = Math.max(d.peak.uploads, inProgress())
    return { blob_id: id, chunk_size: opts.chunkSize ?? 512 * 1024, expires_at_ms: Date.now() + 600_000 }
  }
  rpc.handlers['blob.put_chunk'] = async (p) => {
    const params = p as BlobPutChunkParams
    const key = `${params.blob_id}#${params.index}`
    const attempt = (d.chunkAttempts.get(key) ?? 0) + 1
    d.chunkAttempts.set(key, attempt)
    chunks += 1
    d.peak.chunks = Math.max(d.peak.chunks, chunks)
    try {
      await opts.onChunk?.(params, attempt)
      uploads.get(params.blob_id)?.chunks.set(params.index, base64ToBytes(params.data))
      return {}
    } finally {
      chunks -= 1
    }
  }
  rpc.handlers['blob.put_end'] = (p) => {
    const { blob_id } = p as { blob_id: string }
    const attempt = (endAttempts.get(blob_id) ?? 0) + 1
    endAttempts.set(blob_id, attempt)
    opts.onEnd?.(blob_id, attempt)
    const up = uploads.get(blob_id)!
    up.ended = true
    const ref: AttachmentRef = { blob_id, name: up.begin.name, mime: up.begin.mime, bytes: up.begin.size, width: 40, height: 30 }
    return ref
  }
  rpc.handlers['blob.discard'] = (p) => {
    const id = (p as { blob_id: string }).blob_id
    d.discarded.push(id)
    const up = uploads.get(id)
    // 已经完成的 blob 不再算「进行中」，丢弃的进行中上传腾出名额
    if (up) up.discarded = true
    return {}
  }
  return d
}
