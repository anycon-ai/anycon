// 会话历史缓存（IndexedDB `events`）的上限（spec/web.md § 6.2）：只留这个浏览器最近打开的 `KEEP_SESSIONS` 个会话，
// 每个最多 `MAX_EVENTS_PER_SESSION` 条。打开会话时记下时间，最多每 `PRUNE_EVERY_MS` 淘汰一次。
// 事件的大小差得很远（一条工具输出可能很大），按条数而不是字节算，是因为 IndexedDB 读不出记录大小、只能整条读出来量。
import * as db from '@/db'
import { warnOnError } from '@/lib/log'
import { useSessions } from '@/stores/sessions'

export const KEEP_SESSIONS = 20
/** 和打开会话时「缓存落后太多就整个丢掉」的门槛（`MAX_INCREMENTAL_GAP`，features/session/actions.ts）一样大。 */
export const MAX_EVENTS_PER_SESSION = 2000
export const PRUNE_EVERY_MS = 5 * 60 * 1000

let lastPrune = Number.NEGATIVE_INFINITY

/** 测试用：下一次 `noteSessionOpened` 一定会淘汰。 */
export function resetPruneTimer(): void {
  lastPrune = Number.NEGATIVE_INFINITY
}

/** 淘汰一次。被整个删掉缓存的会话（不在这个标签页里打开着的）`cached_seq` 归零。 */
export async function pruneEventCacheNow(): Promise<db.EventPruneResult> {
  const protect = new Set(Object.keys(useSessions.getState().open))
  const result = await db.pruneEventCache({ keepSessions: KEEP_SESSIONS, maxPerSession: MAX_EVENTS_PER_SESSION, protect })
  if (result.dropped.length > 0) {
    useSessions.setState((s) => {
      const list = { ...s.list }
      for (const id of result.dropped) {
        const item = list[id]
        if (item && !s.open[id]) list[id] = { ...item, cached_seq: 0 }
      }
      return { list }
    })
  }
  return result
}

/**
 * 打开了一个会话：记下时间，必要时淘汰一次。都在后台跑，失败只记 warn——缓存不是必须的，
 * 写不进去时「没能在本地保存数据」的横幅已经由写事件的那一步负责。
 */
export async function noteSessionOpened(sessionId: string, now: number = Date.now()): Promise<void> {
  try {
    await db.touchEventMeta(sessionId, now)
  } catch (e) {
    warnOnError('note when the session was opened')(e)
  }
  if (now - lastPrune < PRUNE_EVERY_MS) return
  lastPrune = now
  await pruneEventCacheNow().catch(warnOnError('prune the session history cache'))
}
