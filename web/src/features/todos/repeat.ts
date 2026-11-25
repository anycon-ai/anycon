// 周期项（`Todo.repeat`，docs/spec/daemon.md § 26.9）在界面里的纯函数。规则按节点的本地时区，下一次的时间由节点算（`repeat.next_ms`），
// 浏览器不自己算：浏览器和节点可能不在同一个时区。
import type { Todo, TodoRepeat } from '@/protocol'

export type RepeatChoice = 'none' | 'daily' | 'weekdays' | 'weekly'
export const REPEAT_CHOICES: readonly RepeatChoice[] = ['none', 'daily', 'weekdays', 'weekly']
export const DEFAULT_REPEAT_AT = '09:00'
/** 「到」的缺省值：选了「每隔……」之后。 */
export const DEFAULT_REPEAT_UNTIL = '18:00'
/** `HH:MM`，`00:00`–`23:59`。 */
export const AT_RE = /^(?:[01]\d|2[0-3]):[0-5]\d$/
/** 「每隔」的选项（分钟）；0 = 只一次。daemon 接受 15–720 之间的任何整数，这里只给常用的。 */
export const REPEAT_EVERY_CHOICES: readonly number[] = [0, 15, 30, 60, 120, 180, 240, 360]
export const MIN_EVERY_MIN = 15
export const MAX_EVERY_MIN = 720

/** 表单里的样子：不重复 / 每天 / 工作日 / 每周的某天，加上时间；可选的「每隔 N 分钟、到 HH:MM」和「Bot 做完自动关闭」。 */
export interface RepeatForm {
  choice: RepeatChoice
  /** 1–7，周一 = 1。 */
  weekday: number
  at: string
  /** 0 = 只一次；否则每隔这么多分钟一次，直到 `until`（`kind = interval`）。 */
  every: number
  until: string
  autoClose: boolean
}

/** 带周期规则的项是模板：不分拣、不派发、不交给 Bot。 */
export const isTemplate = (t: Pick<Todo, 'repeat'>): boolean => !!t.repeat

/** 这一项是周期项复制出来的：模板的 id。 */
export function repeatTemplateOf(t: Pick<Todo, 'source'>): string | null {
  return t.source.kind === 'repeat' && t.source.todo_id ? t.source.todo_id : null
}

type RepeatLike = Pick<TodoRepeat, 'kind' | 'at' | 'weekday'> & Partial<Pick<TodoRepeat, 'days' | 'every_min' | 'until' | 'auto_close'>>

/**
 * 草稿和输入里把规则存成一个字符串：空串 = 不重复，`daily@09:00`、`weekdays@09:00`、`weekly:3@09:00`（星期几是 1–7）；
 * 一天里重复多次的是 `<天>@<起>-<止>/<间隔分钟>`，如 `weekdays@09:00-18:00/120`；末尾的 `#close` 是「Bot 做完自动关闭」。
 * 这一端不认识的种类（更新的节点给的）读成空串：编辑时不动它，保存也就不会发出 `repeat`。
 */
export function repeatToInput(r: RepeatLike | null | undefined): string {
  if (!r) return ''
  const close = r.auto_close ? '#close' : ''
  if (r.kind === 'daily' || r.kind === 'weekdays') return `${r.kind}@${r.at}${close}`
  if (r.kind === 'weekly') return `weekly:${r.weekday ?? 1}@${r.at}${close}`
  if (r.kind === 'interval' && (r.days === 'daily' || r.days === 'weekdays' || r.days === 'weekly')) {
    const days = r.days === 'weekly' ? `weekly:${r.weekday ?? 1}` : r.days
    return `${days}@${r.at}-${r.until ?? ''}/${r.every_min ?? 0}${close}`
  }
  return ''
}

const INPUT_RE = /^(daily|weekdays|weekly)(?::(\d))?@([^-/#]*)(?:-([^/#]*)\/(\d+))?(#close)?$/

export function formOf(input: string): RepeatForm {
  const m = INPUT_RE.exec(input)
  if (!m) return { choice: 'none', weekday: 1, at: DEFAULT_REPEAT_AT, every: 0, until: DEFAULT_REPEAT_UNTIL, autoClose: false }
  const weekday = Number(m[2] ?? 1)
  return {
    choice: m[1] as RepeatChoice,
    weekday: weekday >= 1 && weekday <= 7 ? weekday : 1,
    at: m[3] ?? '',
    every: m[5] !== undefined ? Number(m[5]) : 0,
    until: m[4] ?? DEFAULT_REPEAT_UNTIL,
    autoClose: m[6] !== undefined,
  }
}

export function inputOf(f: RepeatForm): string {
  if (f.choice === 'none') return ''
  const days = f.choice === 'weekly' ? `weekly:${f.weekday}` : f.choice
  return `${days}@${f.at}${f.every > 0 ? `-${f.until}/${f.every}` : ''}${f.autoClose ? '#close' : ''}`
}

/** 输入 → 要发的规则；空串 = 没有规则（`null`）。时间不合格时 `at` 原样带着，由 [`repeatInputError`] 挡掉。 */
export function repeatFromInput(input: string): TodoRepeat | null {
  const f = formOf(input)
  if (f.choice === 'none') return null
  const weekday = f.choice === 'weekly' ? { weekday: f.weekday } : {}
  const close = f.autoClose ? { auto_close: true } : {}
  if (f.every > 0) return { kind: 'interval', days: f.choice, at: f.at, every_min: f.every, until: f.until, ...weekday, ...close }
  return { kind: f.choice, at: f.at, ...weekday, ...close }
}

/** 规则不合格（时间不是 `HH:MM`、「到」不晚于开始、间隔不在 15–720 分钟）：和 daemon 同一条规则，发出去之前先挡掉。 */
export const repeatInputError = (input: string): boolean => repeatInputProblem(input) !== null

/** 哪一项不合格：`at`（开始的时间不是 `HH:MM`）、`until`（「到」不是 `HH:MM` 或不晚于开始）、`every`（间隔不在 15–720 分钟）。 */
export function repeatInputProblem(input: string): 'at' | 'until' | 'every' | null {
  const f = formOf(input)
  if (f.choice === 'none') return null
  if (!AT_RE.test(f.at)) return 'at'
  if (f.every <= 0) return null
  if (!AT_RE.test(f.until) || f.until <= f.at) return 'until'
  return f.every < MIN_EVERY_MIN || f.every > MAX_EVERY_MIN ? 'every' : null
}

export function repeatError(r: TodoRepeat | null | undefined): boolean {
  if (!r) return false
  if (r.kind === 'unknown' || !AT_RE.test(r.at)) return true
  const weekdayNeeded = r.kind === 'weekly' || (r.kind === 'interval' && r.days === 'weekly')
  if (weekdayNeeded ? !(typeof r.weekday === 'number' && r.weekday >= 1 && r.weekday <= 7) : r.weekday != null) return true
  if (r.kind === 'interval') {
    const everyOk = typeof r.every_min === 'number' && r.every_min >= MIN_EVERY_MIN && r.every_min <= MAX_EVERY_MIN
    const untilOk = typeof r.until === 'string' && AT_RE.test(r.until) && r.until > r.at
    return !(r.days === 'daily' || r.days === 'weekdays' || r.days === 'weekly') || !everyOk || !untilOk
  }
  return r.days != null || r.every_min != null || r.until != null
}
