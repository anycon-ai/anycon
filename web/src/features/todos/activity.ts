// 派发之后清单项的「现在的情况」（`Todo.activity`，docs/spec/daemon.md § 22.3）和它在清单页里的分组。纯函数。
// `activity` 是 daemon 算出来的，浏览器从来不自己算：从 `todo.list` / `todo.get` / `todo.changed` 拿到什么就是什么。
// 不认识的值（更新的 daemon 加的）当作没有 `activity`；已关闭的项没有 `activity`（daemon 也不给，这里再兜一层）。
import type { Todo, TodoActivity } from '@/protocol'
import { isClosed } from './types'

export type KnownActivity = Exclude<TodoActivity, 'unknown'>

export function activityOf(t: Pick<Todo, 'activity' | 'state'>): KnownActivity | null {
  if (isClosed(t.state)) return null
  const a: unknown = t.activity
  return a === 'working' || a === 'waiting' || a === 'review' ? a : null
}

/** 清单页的分组，按显示顺序：等你决定、待验收、处理中、还没开始、已关闭（spec/web.md § 4.12）。 */
export const LIST_GROUPS = ['decide', 'review', 'working', 'open', 'closed'] as const
export type ListGroup = (typeof LIST_GROUPS)[number]

export function listGroupOf(t: Pick<Todo, 'activity' | 'state'>): ListGroup {
  if (isClosed(t.state)) return 'closed'
  switch (activityOf(t)) {
    case 'waiting':
      return 'decide'
    case 'review':
      return 'review'
    case 'working':
      return 'working'
    default:
      // 没有 `activity`：人自己设的状态说了算（`working`，加上别的客户端写的 `waiting` / `review`，都归在「处理中」）
      return t.state === 'open' ? 'open' : 'working'
  }
}

/** 谁在处理：agent 正在跑的是 agent，其余（等你决定、待验收、你自己设的「处理中」、还没开始）都在你手上。 */
export type Handler = 'me' | 'agent' | 'twin'
export function handlerOf(t: Pick<Todo, 'activity' | 'state' | 'handler'>): Handler {
  if (!isClosed(t.state) && t.handler === 'twin') return 'twin'
  if (t.handler !== undefined && t.handler !== 'me' && t.handler !== 'twin') return 'me'
  return activityOf(t) === 'working' ? 'agent' : 'me'
}

/** 这一项有没有派发过（有关联的会话，不管它们现在还在不在）。 */
export const wasDispatched = (t: Pick<Todo, 'sessions'>): boolean => (t.sessions?.length ?? 0) > 0

/** 待验收：`activity = review`。 */
export const isReview = (t: Pick<Todo, 'activity' | 'state'>): boolean => activityOf(t) === 'review'
