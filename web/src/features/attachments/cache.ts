// 附件缓存（IndexedDB `blobs`，键是 `blob_id`）：历史里的附件只拉一次，自己刚传的附件 `put_end` 之后立刻写进来
// （blob 被节点回收后可以从这里重传）。
// LRU，上限 200 MiB，按 `last_used_ms` 淘汰最旧的。
//
// IndexedDB 不能按「最近使用」排序也不能只读大小（要升版本加索引），所以第一次写之前扫一遍建一份内存索引，
// 之后写 / 淘汰都用它。读命中只在记录里的时间戳旧了超过 `TOUCH_AFTER_MS` 时才回写（回写要重存整个附件）。
import * as db from '@/db'
import { warnOnError } from '@/lib/log'

export const CACHE_LIMIT_BYTES = 200 * 1024 * 1024
/** 记录里的 `last_used_ms` 比现在旧过这么久才更新（避免每次显示都重存整个附件）。 */
export const TOUCH_AFTER_MS = 10 * 60 * 1000

export interface CacheEntry {
  bytes: Uint8Array
  mime: string
}

export interface IndexEntry {
  blob_id: string
  size: number
  last_used_ms: number
}

/** 总大小超过 `limit` 时，按 `last_used_ms` 从旧到新挑出要删的，直到总大小 ≤ limit。`keep` 里的不删（刚写的那个）。 */
export function pickEvictions(entries: Iterable<IndexEntry>, limit: number, keep?: string): string[] {
  const all = [...entries]
  let total = all.reduce((n, e) => n + e.size, 0)
  if (total <= limit) return []
  const out: string[] = []
  for (const e of all.sort((a, b) => a.last_used_ms - b.last_used_ms || a.blob_id.localeCompare(b.blob_id))) {
    if (total <= limit) break
    if (e.blob_id === keep) continue
    out.push(e.blob_id)
    total -= e.size
  }
  return out
}

let index: Map<string, IndexEntry> | null = null
let indexLoading: Promise<Map<string, IndexEntry>> | null = null

/** 测试用：丢掉内存索引（换了一个新库）。 */
export function resetBlobCache(): void {
  index = null
  indexLoading = null
}

async function loadIndex(): Promise<Map<string, IndexEntry>> {
  if (index) return index
  indexLoading ??= (async () => {
    const scan = await db.scanBlobRecords()
    if (scan.invalid.length > 0) await db.deleteBlobRecords(scan.invalid)
    index = new Map(scan.entries.map((e) => [e.blob_id, e]))
    return index
  })().finally(() => {
    indexLoading = null
  })
  return indexLoading
}

/** 查缓存。没有、读不出、或是旧形状的记录都返回 null（缓存未命中）。 */
export async function readCached(blobId: string, now: number = Date.now()): Promise<CacheEntry | null> {
  let rec: db.BlobRecord | undefined
  try {
    rec = await db.getBlobRecord(blobId)
  } catch (e) {
    console.warn('anycon: could not read the attachment cache', e)
    return null
  }
  if (!rec) return null
  const known = index?.get(blobId)
  if (known) known.last_used_ms = now
  if (now - rec.last_used_ms > TOUCH_AFTER_MS) db.putBlobRecord({ ...rec, last_used_ms: now }).catch(warnOnError('update the attachment cache'))
  return { bytes: rec.bytes, mime: rec.mime }
}

/** 写缓存并按 LRU 淘汰到上限以内。失败只记 warn（缓存不是必须的）。 */
export async function writeCached(blobId: string, bytes: Uint8Array, mime: string, opts: { now?: number; limit?: number } = {}): Promise<void> {
  const now = opts.now ?? Date.now()
  try {
    const idx = await loadIndex()
    await db.putBlobRecord({ blob_id: blobId, bytes, mime, last_used_ms: now })
    idx.set(blobId, { blob_id: blobId, size: bytes.byteLength, last_used_ms: now })
    const drop = pickEvictions(idx.values(), opts.limit ?? CACHE_LIMIT_BYTES, blobId)
    if (drop.length > 0) {
      await db.deleteBlobRecords(drop)
      for (const id of drop) idx.delete(id)
    }
  } catch (e) {
    console.warn('anycon: could not write the attachment cache', e)
  }
}
