// 清单页的分组、过滤、排序（spec/web.md § 4.12）。纯函数。
import { handlerOf, listGroupOf, LIST_GROUPS, type Handler, type ListGroup } from './activity'
import { groupOf } from './outbox'
import { isHumanSource } from './source'
import { isClosed, type ViewTodo } from './types'

export type SourceFilter = 'all' | 'mine' | 'agent'
/** 谁在处理：全部 / 我 / agent。 */
export type HandlerFilter = 'all' | Handler

export interface TodoFilters {
  /** 项目名（精确）；`null` = 全部。 */
  project: string | null
  source: SourceFilter
  /** 缺省全部。 */
  handler?: HandlerFilter
}

export const NO_FILTERS: TodoFilters = { project: null, source: 'all', handler: 'all' }

/**
 * 清单里显示的项：agent 记的、还没人收下的（`unreviewed`）不在清单里，它们在「待处理」的「稍后」段等人收下（收下之后才进清单）。
 */
export const inList = (t: ViewTodo): boolean => !t.unreviewed

export function matches(t: ViewTodo, f: TodoFilters): boolean {
  if (f.project !== null && (t.project ?? null) !== f.project) return false
  if (f.source === 'agent' && t.source.kind !== 'agent') return false
  // 「我记的」只算人记的：不认识的来源不算
  if (f.source === 'mine' && !isHumanSource(t.source)) return false
  if (f.handler && f.handler !== 'all' && handlerOf(t) !== f.handler) return false
  return true
}

/** 周期项的顺序：下一次早的在前（节点还没给下一次的排在后面），再按标题。 */
export function compareTemplates(a: ViewTodo, b: ViewTodo): number {
  const an = a.repeat?.next_ms ?? null
  const bn = b.repeat?.next_ms ?? null
  if ((an !== null) !== (bn !== null)) return an !== null ? -1 : 1
  if (an !== null && bn !== null && an !== bn) return an - bn
  return a.title.localeCompare(b.title) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)
}

/** 组内顺序：有期望时间的在前（早的在前），再按最近更新。 */
export function compareTodos(a: ViewTodo, b: ViewTodo): number {
  const ad = a.due_ms ?? null
  const bd = b.due_ms ?? null
  if ((ad !== null) !== (bd !== null)) return ad !== null ? -1 : 1
  if (ad !== null && bd !== null && ad !== bd) return ad - bd
  if (a.updated_ms !== b.updated_ms) return b.updated_ms - a.updated_ms
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0
}

/** 五个分组，加上没关闭的周期项（模板）：它们不是要处理的事，单独一组，不进「还没开始」。 */
export type TodoGroups = Record<ListGroup, ViewTodo[]> & { repeat: ViewTodo[] }

/**
 * 等你决定（`activity = waiting`）、待验收（`activity = review`）、处理中（`activity = working`，或没有 `activity` 的 `working` 状态，
 * 加上别的客户端写的 `waiting` / `review` 状态）、还没开始（`open`）、已关闭（`done` / `dropped`）。不认识的 `activity` 当作没有。
 */
export function groupTodos(items: readonly ViewTodo[], f: TodoFilters = NO_FILTERS): TodoGroups {
  const out: TodoGroups = { decide: [], review: [], working: [], open: [], closed: [], repeat: [] }
  for (const t of items) {
    if (!inList(t) || !matches(t, f)) continue
    if (t.repeat && !isClosed(t.state)) out.repeat.push(t)
    else out[listGroupOf(t)].push(t)
  }
  for (const g of LIST_GROUPS) out[g].sort(compareTodos)
  out.repeat.sort(compareTemplates)
  return out
}

/** 导航项上的数字：这些节点上没关闭的（并且在清单里的）项。 */
export function openCount(items: readonly ViewTodo[]): number {
  // 周期项是模板，不是要处理的事：不计数（它复制出来的项才计）
  return items.filter((t) => inList(t) && !t.repeat && groupOf(t.state) !== 'closed').length
}

/** 过滤器里能选的项目（出现过的），按名字排。 */
export function projectsOf(items: readonly ViewTodo[]): string[] {
  return [...new Set(items.filter(inList).map((t) => t.project).filter((p): p is string => !!p))].sort((a, b) => a.localeCompare(b))
}

/** 键盘 / 多选用的平铺顺序：等你决定、待验收、处理中、还没开始、已关闭（只有展开时）。 */
export function flatOrder(g: TodoGroups, closedExpanded: boolean, repeatExpanded = true): ViewTodo[] {
  return [...g.decide, ...g.review, ...g.working, ...g.open, ...(repeatExpanded ? g.repeat : []), ...(closedExpanded ? g.closed : [])]
}
