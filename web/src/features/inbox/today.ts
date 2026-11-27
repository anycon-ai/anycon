// 「待处理」清空时的今日小结（spec/web.md § 4.11）：只用这个浏览器本地已有的数据，不发请求，不碰会话内容。
// - 有进展的会话数：会话摘要的 `updated_at_ms`（会话里最后一个事件的时间，归档、改项目也会动它）落在本地的今天。
//   归档的会话不算：归档本身就会动这个时间，算进去会把「今天归档了一个旧会话」也说成今天有进展。
// - 答复的请求数：节点不记「谁在什么时候答复了几次」，所以由这个浏览器自己数——`permission.respond` 成功一次加一，
//   按节点分开记（小结和页面一样只算左栏勾选的节点），存在 localStorage，换了一天就从零开始。别的设备上的答复不在里面，文案写明「这个浏览器里」。
import type { SessionItem } from '@/stores/sessions'

const KEY = 'anycon.today'

interface Stored {
  /** 本地日期 `YYYY-MM-DD`。 */
  day: string
  /** 节点 id → 今天在这个浏览器里答复的请求数。 */
  answered: Record<string, number>
}

/** 本地时区的日期，`YYYY-MM-DD`。 */
export function localDay(now: number): string {
  const d = new Date(now)
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
}

/** 本地时区今天 0 点的毫秒时间戳。 */
export function startOfLocalDay(now: number): number {
  const d = new Date(now)
  d.setHours(0, 0, 0, 0)
  return d.getTime()
}

function read(now: number): Stored {
  const today = localDay(now)
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return { day: today, answered: {} }
    const v: unknown = JSON.parse(raw)
    if (!v || typeof v !== 'object') return { day: today, answered: {} }
    const { day, answered } = v as { day?: unknown; answered?: unknown }
    if (day !== today || !answered || typeof answered !== 'object') return { day: today, answered: {} }
    const out: Record<string, number> = {}
    for (const [k, n] of Object.entries(answered)) if (typeof n === 'number' && Number.isFinite(n) && n > 0) out[k] = Math.floor(n)
    return { day: today, answered: out }
  } catch {
    // 读不了 localStorage（隐私模式、被禁用）或内容坏了：当作今天还没有答复过
    return { day: today, answered: {} }
  }
}

/** 这个浏览器在节点 `nodeId` 上成功答复了一个请求（权限、计划、提问）。 */
export function recordAnswered(nodeId: string, now: number = Date.now()): void {
  const s = read(now)
  s.answered[nodeId] = (s.answered[nodeId] ?? 0) + 1
  try {
    localStorage.setItem(KEY, JSON.stringify(s))
  } catch {
    // 存不下就不记：小结只是少一个数，不影响答复本身
  }
}

/** 今天在这个浏览器里、在这些节点上答复的请求数。 */
export function answeredToday(nodes: readonly string[], now: number = Date.now()): number {
  const s = read(now)
  return nodes.reduce((sum, id) => sum + (s.answered[id] ?? 0), 0)
}

/** 这些节点上今天有过进展的会话数（见文件头的口径）。 */
export function sessionsActiveToday(list: Readonly<Record<string, SessionItem>>, nodes: readonly string[], now: number = Date.now()): number {
  const picked = new Set(nodes)
  const from = startOfLocalDay(now)
  let n = 0
  for (const s of Object.values(list)) if (picked.has(s.node_id) && !s.archived && s.updated_at_ms >= from) n++
  return n
}
