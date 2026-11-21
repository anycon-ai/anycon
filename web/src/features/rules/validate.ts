// 规则文字的本地校验（docs/spec/daemon.md § 23.2）：和 daemon 同一套规则，在发出去之前先挡掉，daemon 仍然拒绝时再显示它的 `invalid`。
// 长度按 Unicode 码点算（`[...s].length`，不是 UTF-16 的 `.length`）。

export const TEXT_MAX = 600
export const PROJECT_MAX = 200

/** 除换行外的控制字符（含 C1，如 U+0085）和 U+2028 / U+2029。 */
const CONTROL = new RegExp('[^\\P{Cc}\\n]|[\\u2028\\u2029]', 'u')
/**
 * 看不见的字符：双向文字控制符（U+202A–U+202E、U+2066–U+2069、U+200E / U+200F、U+061C）和零宽字符（U+200B–U+200D、U+2060、U+FEFF）。
 * 规则原样注入、要由人看过才生效，看不见的字符没法审阅。
 */
/** 单行字段（项目名）里不许有的：控制字符（含换行、制表符）和 U+2028 / U+2029。 */
const SINGLE_LINE_BAD = new RegExp('[\\p{Cc}\\u2028\\u2029]', 'u')
const INVISIBLE = new RegExp('[\\u202A-\\u202E\\u2066-\\u2069\\u200E\\u200F\\u061C\\u200B-\\u200D\\u2060\\uFEFF]', 'u')

export type RuleTextError = 'empty' | 'too_long' | 'control' | 'invisible'

export const codePoints = (s: string): number => [...s].length

/** daemon 存的是去掉首尾空白之后的文字。 */
export const normalizeRuleText = (raw: string): string => raw.replace(/\r\n?/g, '\n').trim()

export function ruleTextError(raw: string): RuleTextError | null {
  const v = normalizeRuleText(raw)
  if (v === '') return 'empty'
  if (CONTROL.test(v)) return 'control'
  if (INVISIBLE.test(v)) return 'invisible'
  if (codePoints(v) > TEXT_MAX) return 'too_long'
  return null
}

/** 项目名：空白按没给；给了就是单行、1–200 个码点、没有控制字符。 */
export function ruleProjectError(raw: string | null | undefined): 'control' | 'too_long' | null {
  if (raw == null || raw.trim() === '') return null
  const v = raw.trim()
  if (SINGLE_LINE_BAD.test(v)) return 'control'
  return codePoints(v) > PROJECT_MAX ? 'too_long' : null
}

export function newRuleId(): string {
  const id = typeof crypto.randomUUID === 'function' ? crypto.randomUUID().toLowerCase() : ''
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id)) return id
  const b = crypto.getRandomValues(new Uint8Array(16))
  b[6] = (b[6]! & 0x0f) | 0x40
  b[8] = (b[8]! & 0x3f) | 0x80
  const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('')
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}
