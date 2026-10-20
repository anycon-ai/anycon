// 连接耗时埋点：页面加载 → 连上 hub → 与节点握手 → 首次同步 → 打开的会话拿到历史，每一步记一个时间点。
//
// - 总是记：用 `performance.mark` / `performance.measure`（名字带 `anycon:` 前缀），DevTools 的 Performance 面板里能看到；
//   另外在内存里留最近 `MAX_ENTRIES` 条。只记阶段名、方法名和节点 id 前缀，不记任何内容。
// - `localStorage['anycon.debug.timing'] = '1'` 时，一串时间点停下来 `REPORT_AFTER_MS` 后在控制台打一张表
//   （从最近一次「开始连 hub」算起）。随时可以在控制台调 `anyconTiming()` 打出全部记录。

const PREFIX = 'anycon:'
const FLAG = 'anycon.debug.timing'
const MAX_ENTRIES = 300
/** 最后一个时间点之后这么久没有新的，就认为这一轮连接 / 同步结束了，打表。 */
const REPORT_AFTER_MS = 1500

export interface TimingEntry {
  name: string
  /** 距页面导航开始的毫秒数（`performance.now()`）。 */
  at: number
  /** 有持续时间的一步（一次 RPC）：耗时毫秒数，`at` 是结束时刻。 */
  durationMs?: number
}

const entries: TimingEntry[] = []
let reportTimer: ReturnType<typeof setTimeout> | null = null

function now(): number {
  return typeof performance !== 'undefined' ? performance.now() : Date.now()
}

function enabled(): boolean {
  try {
    return typeof localStorage !== 'undefined' && localStorage.getItem(FLAG) === '1'
  } catch {
    return false
  }
}

function push(e: TimingEntry): void {
  entries.push(e)
  if (entries.length > MAX_ENTRIES) entries.splice(0, entries.length - MAX_ENTRIES)
  if (!enabled()) return
  if (reportTimer) clearTimeout(reportTimer)
  reportTimer = setTimeout(() => {
    reportTimer = null
    printTiming(sinceLastConnect())
  }, REPORT_AFTER_MS)
}

/** 记一个时间点。 */
export function mark(name: string): void {
  const at = now()
  try {
    performance.mark(PREFIX + name)
  } catch {
    // 没有 User Timing API（测试环境 / 很老的浏览器）：只记在内存里
  }
  push({ name, at })
}

/** 给一个异步步骤计时：结束（不论成败）时记一条带耗时的记录。 */
export async function timed<T>(name: string, f: () => Promise<T>): Promise<T> {
  const start = now()
  try {
    return await f()
  } finally {
    const end = now()
    try {
      performance.measure(PREFIX + name, { start, end })
    } catch {
      // 同上
    }
    push({ name, at: end, durationMs: Math.round(end - start) })
  }
}

/** 节点 id 的短前缀，用在时间点名字里。 */
export function shortId(id: string): string {
  return id.slice(0, 8)
}

/** 从最近一次「开始连 hub」起的记录；只连过一次（页面刚打开）就从头算，带上连 hub 之前的加载步骤。 */
function sinceLastConnect(): TimingEntry[] {
  const connects = entries.flatMap((e, i) => (e.name === 'hub.connect' ? [i] : []))
  return connects.length > 1 ? entries.slice(connects.at(-1)) : entries.slice()
}

function printTiming(list: TimingEntry[]): void {
  if (list.length === 0) return
  const base = list[0]!.at
  let prev = base
  const rows = list.map((e) => {
    const row = {
      step: e.name,
      'at (ms since load)': Math.round(e.at),
      '+ms since first row': Math.round(e.at - base),
      'gap ms': Math.round(e.at - prev),
      'took ms': e.durationMs ?? '',
    }
    prev = e.at
    return row
  })
  console.info('anycon: connection timing (disable with localStorage.removeItem("anycon.debug.timing"))')
  console.table(rows)
}

/** 测试用：清空记录。 */
export function resetTiming(): void {
  entries.length = 0
  if (reportTimer) clearTimeout(reportTimer)
  reportTimer = null
}

/** 当前记录的副本（测试 / 调试用）。 */
export function timingEntries(): TimingEntry[] {
  return entries.slice()
}

if (typeof window !== 'undefined') {
  ;(window as unknown as { anyconTiming?: () => void }).anyconTiming = () => printTiming(entries)
}
