// 周期项（`Todo.repeat`，docs/spec/daemon.md § 26.9）在界面里的纯函数。规则按节点的本地时区，下一次的时间由节点算（`repeat.next_ms`），
// 浏览器不自己算：浏览器和节点可能不在同一个时区。
import type { Todo, TodoRepeat } from '@/protocol'

export type RepeatChoice = 'none' | 'daily' | 'weekdays' | 'weekly'
export const REPEAT_CHOICES: readonly RepeatChoice[] = ['none', 'daily', 'weekdays', 'weekly']
export const DEFAULT_REPEAT_AT = '09:00'
/** `HH:MM`，`00:00`–`23:59`。 */
export const AT_RE = /^(?:[01]\d|2[0-3]):[0-5]\d$/

/** 表单里的样子：不重复 / 每天 / 工作日 / 每周的某天，加上时间。 */
export interface RepeatForm {
  choice: RepeatChoice
  /** 1–7，周一 = 1。 */
  weekday: number
  at: string
}

/** 带周期规则的项是模板：不分拣、不派发、不交给 Bot。 */
export const isTemplate = (t: Pick<Todo, 'repeat'>): boolean => !!t.repeat

/** 这一项是周期项复制出来的：模板的 id。 */
export function repeatTemplateOf(t: Pick<Todo, 'source'>): string | null {
  return t.source.kind === 'repeat' && t.source.todo_id ? t.source.todo_id : null
}

/**
 * 草稿和输入里把规则存成一个字符串：空串 = 不重复，`daily@09:00`、`weekdays@09:00`、`weekly:3@09:00`（星期几是 1–7）。
 * 这一端不认识的种类（更新的节点给的）读成空串：编辑时不动它，保存也就不会发出 `repeat`。
 */
export function repeatToInput(r: Pick<TodoRepeat, 'kind' | 'at' | 'weekday'> | null | undefined): string {
  if (!r) return ''
  if (r.kind === 'daily' || r.kind === 'weekdays') return `${r.kind}@${r.at}`
  if (r.kind === 'weekly') return `weekly:${r.weekday ?? 1}@${r.at}`
  return ''
}

export function formOf(input: string): RepeatForm {
  const m = /^(daily|weekdays|weekly)(?::(\d))?@(.*)$/.exec(input)
  if (!m) return { choice: 'none', weekday: 1, at: DEFAULT_REPEAT_AT }
  const weekday = Number(m[2] ?? 1)
  return { choice: m[1] as RepeatChoice, weekday: weekday >= 1 && weekday <= 7 ? weekday : 1, at: m[3] ?? '' }
}

export function inputOf(f: RepeatForm): string {
  if (f.choice === 'none') return ''
  return f.choice === 'weekly' ? `weekly:${f.weekday}@${f.at}` : `${f.choice}@${f.at}`
}

/** 输入 → 要发的规则；空串 = 没有规则（`null`）。时间不合格时 `at` 原样带着，由 [`repeatInputError`] 挡掉。 */
export function repeatFromInput(input: string): TodoRepeat | null {
  const f = formOf(input)
  if (f.choice === 'none') return null
  return f.choice === 'weekly' ? { kind: 'weekly', at: f.at, weekday: f.weekday } : { kind: f.choice, at: f.at }
}

/** 规则不合格（时间不是 `HH:MM`）：和 daemon 同一条规则，发出去之前先挡掉。 */
export function repeatInputError(input: string): boolean {
  const f = formOf(input)
  return f.choice !== 'none' && !AT_RE.test(f.at)
}
export const repeatError = (r: TodoRepeat | null | undefined): boolean => !!r && (r.kind === 'unknown' || !AT_RE.test(r.at) || (r.kind === 'weekly' ? !(typeof r.weekday === 'number' && r.weekday >= 1 && r.weekday <= 7) : r.weekday != null))
