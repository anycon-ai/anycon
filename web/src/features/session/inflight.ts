// 正在答复中的权限请求（按请求 id），会话页的按钮和 ⌘⇧A / ⌘⇧D 共用同一份：同一个请求已经在答复时，第二次答复（连点、连按）直接丢掉，
// 不会发出两次，也不会落到列表里补位上来的下一条请求上。
// 另记「在这个页面上处理的条目」（按条目 key）：「待处理」页据此区分「我刚处理完它」（选中和焦点前进到下一条）和
// 「它在别处被处理了」（不替用户挑下一条，见 spec/web.md § 4.11）。
import { create } from 'zustand'

/** 处理成功之后这条记录留多久（毫秒）：条目从列表里消失、页面来消费它之前。 */
const HANDLED_TTL_MS = 5000

interface InflightState {
  ids: ReadonlySet<string>
  handled: ReadonlySet<string>
}

export const useInflight = create<InflightState>()(() => ({ ids: new Set(), handled: new Set() }))

const timers = new Map<string, ReturnType<typeof setTimeout>>()

function without(set: ReadonlySet<string>, id: string): ReadonlySet<string> {
  if (!set.has(id)) return set
  const next = new Set(set)
  next.delete(id)
  return next
}

/** 请求的条目 key（和 `features/inbox/derive.ts` 里一致）。 */
export const requestItemKey = (requestId: string): string => `request:${requestId}`

/** 标记「这个条目是在这个页面上处理的」。 */
export function markHandled(itemKey: string): void {
  clearTimeout(timers.get(itemKey))
  useInflight.setState((s) => ({ handled: new Set(s.handled).add(itemKey) }))
}

/** 处理没成功（请求失败、被拒绝）：条目还在，不算处理过。 */
export function unmarkHandled(itemKey: string): void {
  clearTimeout(timers.get(itemKey))
  timers.delete(itemKey)
  useInflight.setState((s) => ({ handled: without(s.handled, itemKey) }))
}

/** 页面用来判断消失的条目是不是自己处理的；读一次就清掉。 */
export function consumeHandled(itemKey: string): boolean {
  const had = useInflight.getState().handled.has(itemKey)
  if (had) unmarkHandled(itemKey)
  return had
}

/** 开始答复这个请求。已经在答复就返回 false（调用方什么都不发）。 */
export function beginAnswer(requestId: string): boolean {
  if (useInflight.getState().ids.has(requestId)) return false
  useInflight.setState((s) => ({ ids: new Set(s.ids).add(requestId) }))
  markHandled(requestItemKey(requestId))
  return true
}

/** 答复结束。`ok` 为真时「已处理」的记录再留一小会儿，等页面消费。 */
export function endAnswer(requestId: string, ok: boolean): void {
  useInflight.setState((s) => ({ ids: without(s.ids, requestId) }))
  const key = requestItemKey(requestId)
  if (!ok) {
    unmarkHandled(key)
    return
  }
  timers.set(
    key,
    setTimeout(() => unmarkHandled(key), HANDLED_TTL_MS),
  )
}

/** 标记「这个条目是在这个页面上处理的」，请求没发完就不用等：过一会儿自己清掉（没被消费也不留着）。 */
export function holdHandled(itemKey: string): void {
  markHandled(itemKey)
  timers.set(
    itemKey,
    setTimeout(() => unmarkHandled(itemKey), HANDLED_TTL_MS),
  )
}
