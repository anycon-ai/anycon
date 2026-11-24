// 「待处理」的模型只算一份，导航项、窄条、页面、应用角标共用（`useSyncExternalStore` 的外部 store）。
// 只在它的输入真的变了才重算：会话摘要里和「待处理」页有关的字段、待答复的请求、接入请求、节点过滤器的勾选。打开的会话每来一条流式事件，
// `list` 里那一项的 `cached_seq` 都会变，但那和待处理无关，不触发重算。
// 定时器：有还没过期的接入请求（按秒倒计时）时每秒一次，否则每分钟一次（出错会话的 7 天期限）；没人订阅时不起定时器。
import { supportsTwin, twinQuestions, useTwin } from '@/features/twin/store'
import { cleanHintText } from '@/lib/hint'
import { deriveSpaces, groupOf, resolveSelection } from '@/lib/spaces'
import { useEnroll, type EnrollItem } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { nodeSupportsRules, rulesOf, useRules } from '@/features/rules/store'
import { nodeSupportsTodo, useTodos, viewOf } from '@/features/todos/store'
import { allItems, deriveInbox, inboxNowCount, nowItems, type InboxModel } from './derive'

export interface InboxSnapshot {
  /** 左栏节点过滤器勾选的节点上的待处理。 */
  model: InboxModel
  /** 没勾选的节点上「现在」段的条数：节点过滤按钮上的小点据此显示。 */
  outside: number
  /**
   * `outside` 的去处：没选中的节点上「现在」段的条数，按空间（没有分组的节点按节点）分。`label` 是显示名（清理过，不是身份）。
   * 小点的可访问名称据此写「别的空间里有几件在等」。
   */
  outsideWhere: Array<{ label: string; space: boolean; count: number }>
}

interface Inputs {
  twinNodes: ReturnType<typeof useTwin.getState>['nodes']
  list: Record<string, SessionItem>
  pending: ReturnType<typeof useSessions.getState>['pending']
  enrollItems: Record<string, EnrollItem>
  enrollResolved: ReturnType<typeof useEnroll.getState>['resolved']
  order: string[]
  saved: string[] | null
  space: string | null
  /** 各节点的分组（身份）：分组变了选中的节点就变，所以进输入里。 */
  groups: string
  byId: ReturnType<typeof useNodes.getState>['byId']
  todoNodes: ReturnType<typeof useTodos.getState>['nodes']
  todoOutbox: ReturnType<typeof useTodos.getState>['outbox']
  ruleNodes: ReturnType<typeof useRules.getState>['nodes']
}

function groupsSig(s: ReturnType<typeof useNodes.getState>): string {
  return s.order.map((id) => `${id}=${groupOf(s.byId[id]?.info)}:twin=${s.byId[id]?.info?.capabilities.twin === true}`).join('\u0000')
}

function readInputs(): Inputs {
  const s = useSessions.getState()
  const e = useEnroll.getState()
  return { twinNodes: useTwin.getState().nodes, list: s.list, pending: s.pending, enrollItems: e.items, enrollResolved: e.resolved, order: useNodes.getState().order, saved: usePrefs.getState().sidebarNodes, space: usePrefs.getState().sidebarSpace, byId: useNodes.getState().byId, groups: groupsSig(useNodes.getState()), todoNodes: useTodos.getState().nodes, todoOutbox: useTodos.getState().outbox, ruleNodes: useRules.getState().nodes }
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
  return a.twinNodes !== b.twinNodes || listRelevantChanged(a.list, b.list) || a.pending !== b.pending || a.enrollItems !== b.enrollItems || a.enrollResolved !== b.enrollResolved || a.order !== b.order || a.saved !== b.saved || a.space !== b.space || a.groups !== b.groups || a.todoNodes !== b.todoNodes || a.todoOutbox !== b.todoOutbox || a.ruleNodes !== b.ruleNodes
}

function compute(i: Inputs, now: number): InboxSnapshot {
  const selected = resolveSelection(i.order, i.byId, i.saved, i.space)
  const todos = viewOf({ nodes: i.todoNodes, outbox: i.todoOutbox }, selected.filter(nodeSupportsTodo))
  const rules = rulesOf(i.ruleNodes, selected.filter(nodeSupportsRules))
  const base = { twinQuestions: twinQuestions(i.twinNodes, i.order.filter(supportsTwin)), sessions: i.list, pending: i.pending, enroll: { items: i.enrollItems, resolved: i.enrollResolved }, now, todos, rules }
  const outsideNodes = i.order.filter((id) => !selected.includes(id))
  const spaces = deriveSpaces(i.order, i.byId)
  const outsideWhere: InboxSnapshot['outsideWhere'] = []
  for (const id of outsideNodes) {
    const count = nowItems(deriveInbox({ ...base, selected: [id] })).length
    if (count === 0) continue
    const sp = spaces.spaces.find((x) => x.nodeIds.includes(id))
    const label = sp ? sp.label : cleanHintText(i.byId[id]?.name) || id.slice(0, 8)
    const hit = outsideWhere.find((w) => w.label === label && w.space === !!sp)
    if (hit) hit.count += count
    else outsideWhere.push({ label, space: !!sp, count })
  }
  return {
    model: deriveInbox({ ...base, selected }),
    outside: outsideWhere.reduce((n, w) => n + w.count, 0),
    outsideWhere,
  }
}

function sameKeys(a: InboxSnapshot, b: InboxSnapshot): boolean {
  const ka = allItems(a.model).map((x) => x.key)
  const kb = allItems(b.model).map((x) => x.key)
  return a.outside === b.outside && JSON.stringify(a.outsideWhere) === JSON.stringify(b.outsideWhere) && ka.length === kb.length && ka.every((k, n) => k === kb[n]) && a.model.inProgress.length === b.model.inProgress.length && a.model.inProgress.every((s, n) => s.id === b.model.inProgress[n]!.id) && a.model.inProgressTodos.length === b.model.inProgressTodos.length && a.model.inProgressTodos.every((t, n) => t.id === b.model.inProgressTodos[n]!.id && t.node_id === b.model.inProgressTodos[n]!.node_id)
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
    unsubs = [useTwin.subscribe(onStoreChange), useTodos.subscribe(onStoreChange), useRules.subscribe(onStoreChange), useSessions.subscribe(onStoreChange), useEnroll.subscribe(onStoreChange), useNodes.subscribe((s, prev) => (s.order !== prev.order || groupsSig(s) !== groupsSig(prev)) && onStoreChange()), usePrefs.subscribe((s, prev) => (s.sidebarNodes !== prev.sidebarNodes || s.sidebarSpace !== prev.sidebarSpace) && onStoreChange())]
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
