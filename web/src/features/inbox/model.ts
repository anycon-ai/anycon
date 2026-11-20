// 「待处理」的模型只算一份，导航项、手机标签栏、窄条、页面、应用角标共用（`useSyncExternalStore` 的外部 store）。
// 只在它的输入真的变了才重算：会话摘要里和待处理有关的字段、待答复的请求、接入请求、节点过滤器的勾选。打开的会话每来一条流式事件，
// `list` 里那一项的 `cached_seq` 都会变，但那和待处理无关，不触发重算。
// 定时器：有还没过期的接入请求（按秒倒计时）时每秒一次，否则每分钟一次（出错会话的 7 天期限）；没人订阅时不起定时器。
import { selectedNodes } from '@/lib/nodeFilter'
import { useEnroll, type EnrollItem } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { deriveInbox, inboxNowCount, nowItems, type InboxModel } from './derive'

export interface InboxSnapshot {
  /** 左栏节点过滤器勾选的节点上的待处理。 */
  model: InboxModel
  /** 没勾选的节点上「现在」段的条数：节点过滤按钮上的小点据此显示。 */
  outside: number
}

interface Inputs {
  list: Record<string, SessionItem>
  pending: ReturnType<typeof useSessions.getState>['pending']
  enrollItems: Record<string, EnrollItem>
  enrollResolved: ReturnType<typeof useEnroll.getState>['resolved']
  order: string[]
  saved: string[] | null
}

function readInputs(): Inputs {
  const s = useSessions.getState()
  const e = useEnroll.getState()
  return { list: s.list, pending: s.pending, enrollItems: e.items, enrollResolved: e.resolved, order: useNodes.getState().order, saved: usePrefs.getState().sidebarNodes }
}

/** 一个会话摘要里和待处理有关的字段是否变了（`cached_seq`、`last_seq`、设置等不算）。 */
function sessionRelevantChanged(a: SessionItem, b: SessionItem): boolean {
  return (
    a.status !== b.status ||
    a.archived !== b.archived ||
    a.follow_up !== b.follow_up ||
    a.pending_permissions !== b.pending_permissions ||
    a.updated_at_ms !== b.updated_at_ms ||
    a.title !== b.title ||
    a.project !== b.project ||
    a.node_id !== b.node_id ||
    a.worktree !== b.worktree
  )
}

function listRelevantChanged(a: Record<string, SessionItem>, b: Record<string, SessionItem>): boolean {
  if (a === b) return false
  const ak = Object.keys(a)
  if (ak.length !== Object.keys(b).length) return true
  for (const k of ak) {
    const x = a[k]
    const y = b[k]
    if (!y) return true
    if (x !== y && sessionRelevantChanged(x!, y)) return true
  }
  return false
}

function inputsChanged(a: Inputs, b: Inputs): boolean {
  return listRelevantChanged(a.list, b.list) || a.pending !== b.pending || a.enrollItems !== b.enrollItems || a.enrollResolved !== b.enrollResolved || a.order !== b.order || a.saved !== b.saved
}

function compute(i: Inputs, now: number): InboxSnapshot {
  const selected = selectedNodes(i.order, i.saved)
  const base = { sessions: i.list, pending: i.pending, enroll: { items: i.enrollItems, resolved: i.enrollResolved }, now }
  const outsideNodes = i.order.filter((id) => !selected.includes(id))
  return {
    model: deriveInbox({ ...base, selected }),
    outside: outsideNodes.length > 0 ? nowItems(deriveInbox({ ...base, selected: outsideNodes })).length : 0,
  }
}

function sameKeys(a: InboxSnapshot, b: InboxSnapshot): boolean {
  const ka = nowItems(a.model).map((x) => x.key)
  const kb = nowItems(b.model).map((x) => x.key)
  return a.outside === b.outside && ka.length === kb.length && ka.every((k, n) => k === kb[n]) && a.model.inProgress.length === b.model.inProgress.length && a.model.inProgress.every((s, n) => s.id === b.model.inProgress[n]!.id)
}

let cache: { inputs: Inputs; at: number; snap: InboxSnapshot } | null = null

/** 当前的模型。输入没变、时间也没到下一次重算就原样返回同一个对象。 */
export function getInboxSnapshot(now: number = Date.now()): InboxSnapshot {
  const inputs = readInputs()
  if (cache && !inputsChanged(cache.inputs, inputs)) {
    if (now - cache.at < tickMs(cache.inputs, cache.at) && now >= cache.at) {
      cache = { ...cache, inputs }
      return cache.snap
    }
    // 只是时间过去了（接入请求过期、出错会话过了期限）：条目没变就保持同一个对象，订阅的组件不用重渲染
    const next = compute(inputs, now)
    cache = { inputs, at: now, snap: sameKeys(cache.snap, next) ? cache.snap : next }
    return cache.snap
  }
  cache = { inputs, at: now, snap: compute(inputs, now) }
  return cache.snap
}

/** 还有没过期的接入请求时按秒倒计时，否则一分钟一次。 */
function tickMs(i: Inputs, now: number): number {
  return Object.values(i.enrollItems).some((e) => e.expiresAt > now) ? 1000 : 60_000
}

const listeners = new Set<() => void>()
let unsubs: (() => void)[] = []
let timer: ReturnType<typeof setTimeout> | null = null

function notify(): void {
  for (const l of listeners) l()
}

function schedule(): void {
  if (timer) clearTimeout(timer)
  timer = setTimeout(() => {
    notify()
    schedule()
  }, tickMs(readInputs(), Date.now()))
}

/** store 变了：通知订阅的组件，并按现在的情况重排定时器（刚来了接入请求就该开始按秒走）。 */
function onStoreChange(): void {
  schedule()
  notify()
}

export function subscribeInbox(listener: () => void): () => void {
  if (listeners.size === 0) {
    unsubs = [useSessions.subscribe(onStoreChange), useEnroll.subscribe(onStoreChange), useNodes.subscribe((s, prev) => s.order !== prev.order && onStoreChange()), usePrefs.subscribe((s, prev) => s.sidebarNodes !== prev.sidebarNodes && onStoreChange())]
    schedule()
  }
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
    if (listeners.size === 0) {
      for (const u of unsubs) u()
      unsubs = []
      if (timer) clearTimeout(timer)
      timer = null
    }
  }
}

/** 非 React 的调用方（应用角标）：「现在」段的条数，口径和导航项一样。 */
export function inboxNowCountFromStores(now: number = Date.now()): number {
  return inboxNowCount(getInboxSnapshot(now).model)
}
