// 同步算法里「和本地比对」那一步（spec/web.md § 6.3 第 2 条），纯函数。
import type { SessionSummary } from '@/protocol'

/** 本地对一个会话已缓存的东西。 */
export interface LocalSessionMeta {
  /** 本地事件缓存里最大的 seq；没有缓存是 0。 */
  cachedSeq: number
}

export interface SyncPlan {
  /** 远端全部会话（新会话插入，旧的刷新元数据）。有缓存而落后的会话打开时才增量拉，没有缓存的打开时再拉。 */
  upsert: SessionSummary[]
  /** 远端 last_seq 反而更小：daemon 那边重建过，本地缓存作废。 */
  reset: string[]
  /** 本地有、远端没有：已被删除。 */
  deleted: string[]
}

export function planSessionSync(local: Record<string, LocalSessionMeta>, remote: SessionSummary[]): SyncPlan {
  const remoteIds = new Set(remote.map((s) => s.id))
  const reset: string[] = []
  for (const s of remote) {
    const l = local[s.id]
    if (l && s.last_seq < l.cachedSeq) reset.push(s.id)
  }
  const deleted = Object.keys(local).filter((id) => !remoteIds.has(id))
  return { upsert: remote, reset, deleted }
}
