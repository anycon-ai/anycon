import type { TodoRepeat } from '@/protocol'
import { repeatError } from './repeat'

// 清单项的本地校验（docs/spec/daemon.md § 21.2）：和 daemon 同一套规则，在发出去之前先挡掉，daemon 仍然拒绝时再显示它的 `invalid`。
// 长度按 Unicode 码点算（`[...s].length`，不是 UTF-16 的 `.length`：一个 emoji 在 `.length` 里是 2），正文按 UTF-8 字节。

export const TITLE_MAX = 200
export const PROJECT_MAX = 200
export const BODY_MAX_BYTES = 16 * 1024

/** 控制字符（含制表符、换行）和 U+2028 / U+2029：单行字段里都不许有。 */
const LINE_BAD = /[\p{Cc}\u2028\u2029]/u

export function codePoints(s: string): number {
  return [...s].length
}

export function utf8Bytes(s: string): number {
  return new TextEncoder().encode(s).length
}

export type TodoField = 'title' | 'body' | 'project' | 'due_ms' | 'repeat'
export interface FieldError {
  /** `storage`：本地库写不进去（配额、库被别的标签页关掉），改动没有入队。 */
  field: TodoField | 'storage'
  reason: 'empty' | 'too_long' | 'control' | 'invalid' | 'failed'
}

/** 单行字段：去掉首尾空白，1–`max` 个码点，没有控制字符。返回错误，或 null。 */
export function lineError(field: TodoField, raw: string, max: number): FieldError | null {
  const v = raw.trim()
  if (v === '') return { field, reason: 'empty' }
  if (LINE_BAD.test(v)) return { field, reason: 'control' }
  if (codePoints(v) > max) return { field, reason: 'too_long' }
  return null
}

export const titleError = (raw: string): FieldError | null => lineError('title', raw, TITLE_MAX)

/** 项目名：空白按没给处理（不是错误）；给了就要合格。 */
export function projectError(raw: string | null | undefined): FieldError | null {
  if (raw == null || raw.trim() === '') return null
  return lineError('project', raw, PROJECT_MAX)
}

export function bodyError(raw: string): FieldError | null {
  return utf8Bytes(raw) > BODY_MAX_BYTES ? { field: 'body', reason: 'too_long' } : null
}

export function dueError(ms: number | null | undefined): FieldError | null {
  return ms == null || (Number.isSafeInteger(ms) && ms >= 0) ? null : { field: 'due_ms', reason: 'invalid' }
}

/** 小写带连字符的 UUID（daemon 不做规范化，别的写法都 `invalid`）。 */
export const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

export function newTodoId(): string {
  const id = typeof crypto.randomUUID === 'function' ? crypto.randomUUID().toLowerCase() : ''
  return UUID_RE.test(id) ? id : fallbackUuid()
}

function fallbackUuid(): string {
  const b = crypto.getRandomValues(new Uint8Array(16))
  b[6] = (b[6]! & 0x0f) | 0x40
  b[8] = (b[8]! & 0x3f) | 0x80
  const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('')
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}

/**
 * 记一项的输入：第一行是标题，换行之后是正文（「只要求一行字」，spec/web.md § 4.12.1）。
 * 标题去掉首尾空白；正文去掉开头的空行和结尾的空白。
 */
/** 单行字段里粘进来的制表符按一个空格算（不是「必须是一行」的错误）。 */
export const tabsToSpaces = (s: string): string => s.replace(/\t/g, ' ')

export function parseCapture(text: string): { title: string; body: string } {
  const normalized = text.replace(/\r\n?/g, '\n')
  const nl = normalized.indexOf('\n')
  const title = tabsToSpaces(nl < 0 ? normalized : normalized.slice(0, nl)).trim()
  const body = nl < 0 ? '' : normalized.slice(nl + 1).replace(/^(?:[ \t]*\n)+/, '').trimEnd()
  return { title, body }
}

export interface DraftTodo {
  /** 客户端先定下的 id（小写 UUID）；没给就新生成一个。 */
  id?: string
  handler?: 'me' | 'twin'
  title: string
  body: string
  project?: string | null
  due_ms?: number | null
  repeat?: TodoRepeat | null
}

/** 新建前的整体校验：返回第一个错误，或 null。 */
export function draftError(d: DraftTodo): FieldError | null {
  return titleError(d.title) ?? bodyError(d.body) ?? projectError(d.project) ?? dueError(d.due_ms) ?? repeatFieldError(d.repeat)
}

/** 周期规则不合格（时间不是 `HH:MM`、星期不对）。 */
export function repeatFieldError(r: TodoRepeat | null | undefined): FieldError | null {
  return repeatError(r) ? { field: 'repeat', reason: 'invalid' } : null
}

/** 显示用：去掉控制字符和双向控制符，折成一行（agent 给的标题等不可信文字，和会话标题同一个处理）。 */
export function cleanOneLine(raw: string | null | undefined, max = 200): string {
  const cleaned = (raw ?? '')
    .replace(/[\p{Cc}\u2028\u2029\u200e\u200f\u202a-\u202e\u2066-\u2069]/gu, ' ')
    .replace(/\s+/g, ' ')
    .trim()
  const cps = [...cleaned]
  return cps.length > max ? `${cps.slice(0, max).join('')}…` : cleaned
}
