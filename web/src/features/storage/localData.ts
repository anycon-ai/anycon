// 本机存储（`/settings/data`，spec/web.md § 4.9）：浏览器报告的用量和配额、持久化存储的状态和请求，以及清空缓存。
// 清空的只是缓存——会话历史（`events`）和附件（`blobs`），打开会话时会再从节点拉；身份、节点、草稿、未发送消息、
// 清单和待发队列都不动。
import * as db from '@/db'
import { attachmentCacheStats, resetBlobCache } from '@/features/attachments/cache'
import { useDrafts } from '@/stores/drafts'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

export interface StorageUsage {
  /** 这个 origin 用了多少字节（浏览器给的估计，包括 service worker 的缓存）。 */
  usage: number
  /** 浏览器允许这个 origin 用到多少（估计）；常常按设备剩余空间算。 */
  quota: number
}

/** `navigator.storage.estimate()`；不支持或失败时是 null。 */
export async function readUsage(): Promise<StorageUsage | null> {
  const storage = typeof navigator === 'undefined' ? undefined : navigator.storage
  if (typeof storage?.estimate !== 'function') return null
  try {
    const e = await storage.estimate()
    if (typeof e.usage !== 'number' || typeof e.quota !== 'number' || e.quota <= 0) return null
    return { usage: e.usage, quota: e.quota }
  } catch (e) {
    console.warn('anycon: could not read the storage estimate', e)
    return null
  }
}

export type PersistState = 'persisted' | 'best_effort' | 'unsupported'

function persistApi(): StorageManager | null {
  const storage = typeof navigator === 'undefined' ? undefined : navigator.storage
  return typeof storage?.persisted === 'function' && typeof storage.persist === 'function' ? storage : null
}

/** 浏览器是否答应过不为腾空间删这个 origin 的数据（`navigator.storage.persisted()`）。 */
export async function readPersist(): Promise<PersistState> {
  const api = persistApi()
  if (!api) return 'unsupported'
  try {
    return (await api.persisted()) ? 'persisted' : 'best_effort'
  } catch (e) {
    console.warn('anycon: could not read the persistent storage state', e)
    return 'unsupported'
  }
}

/** 向浏览器请求持久化存储（Firefox 会弹窗问用户，Chromium / Safari 自己按使用情况决定）。返回请求后的状态。 */
export async function requestPersist(): Promise<PersistState> {
  const api = persistApi()
  if (!api) return 'unsupported'
  try {
    return (await api.persist()) ? 'persisted' : 'best_effort'
  } catch (e) {
    console.warn('anycon: could not request persistent storage', e)
    return 'best_effort'
  }
}

export interface CacheStats {
  /** 缓存的会话事件条数。 */
  events: number
  attachments: { count: number; bytes: number }
}

export async function readCacheStats(): Promise<CacheStats> {
  const [events, attachments] = await Promise.all([db.countEvents(), attachmentCacheStats()])
  return { events, attachments }
}

/**
 * 内存里还在用的附件：草稿（含还没落盘的）、输入区里传完的附件、发送中的消息。
 * 已经落盘的草稿 `db.clearCaches` 自己在同一个事务里再读一遍。
 */
export function blobIdsInUse(): Set<string> {
  const s = useDrafts.getState()
  const out = new Set<string>()
  for (const d of Object.values(s.byKey)) for (const id of db.draftBlobIds(d)) out.add(id)
  for (const items of Object.values(s.items)) for (const i of items) if (i.ref) out.add(i.ref.blob_id)
  for (const i of Object.values(s.inflight)) for (const a of i.attachments ?? []) out.add(a.blob_id)
  return out
}

/**
 * 清空会话历史和附件缓存。之后：
 * - 没打开的会话的 `cached_seq` 归零（本地已经没有它的事件）；打开着的会话内存里的事件还在，之后的新事件照常写进缓存
 * - 附件缓存的内存索引丢掉，下次写缓存时重新扫
 * - 「没能在本地保存数据」的横幅收起（空间腾出来了；再写失败会再出来）
 *
 * 别的标签页的内存索引不知道这次清空，它们会把附件缓存算大，可能提前淘汰；刷新后就对了。
 */
export async function clearLocalCaches(): Promise<{ events: number; blobs: number }> {
  const result = await db.clearCaches(blobIdsInUse())
  resetBlobCache()
  useSessions.setState((s) => {
    const list = { ...s.list }
    for (const [id, item] of Object.entries(list)) if (!s.open[id] && item.cached_seq !== 0) list[id] = { ...item, cached_seq: 0 }
    return { list }
  })
  useUi.getState().setStorageError(false)
  return result
}
