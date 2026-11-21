// 「待处理」页的数据（spec/web.md § 4.11）：从 store 里已有的数据算出所有等用户的事。这里只有纯函数，不碰 DOM 和网络；
// 页面和徽标（导航项、应用角标、标签页标题）都用 `deriveInbox` 的结果，数字和列表不会各算各的。
//
// 加新的段（待验收、稍后）或新的条目类型时：在 `InboxSectionId` / `InboxItem` 里加一项，在 `deriveInbox` 里往对应的段里放，
// 页面按 `SECTIONS` 的顺序逐段渲染，不需要改页面的骨架。
import type { PermissionKind } from '@/protocol'
import { isActiveStatus } from '@/features/session/group'
import { isFollowUp, isFollowUpReady } from '@/features/session/marks'
import { pendingApprovals, type EnrollItem, type EnrollResolution } from '@/stores/enroll'
import type { PendingPermission, SessionItem } from '@/stores/sessions'
import { isReview } from '@/features/todos/activity'
import { isClosed, type ViewTodo } from '@/features/todos/types'

/** 出错的会话在「现在」里留多久（按 `updated_at_ms` 算），超过就不再算等用户的事。 */
export const ERROR_WINDOW_MS = 7 * 24 * 60 * 60 * 1000

/** 页面上的段，从上到下：现在（卡着正在跑的活，计入数字和角标）、待验收（做完的清单项，数量只写在段标题上）、稍后。 */
export const SECTIONS = ['now', 'review', 'later'] as const
export type InboxSectionId = (typeof SECTIONS)[number]

export type InboxItemKind = 'permission' | 'plan' | 'question' | 'enroll' | 'follow_up' | 'error' | 'waiting' | 'review' | 'later'

interface ItemBase {
  /** 稳定的 key：列表、选中、焦点都靠它。 */
  key: string
  section: InboxSectionId
  nodeId: string
  /** 开始等的时刻（本机或 daemon 的时钟，毫秒）：同类条目里等得最久的在前。 */
  sinceMs: number
  /** 到期时刻：有的话比没有的更靠前，先到期的在前。 */
  expiresAtMs: number | null
}

export interface RequestItem extends ItemBase {
  kind: 'permission' | 'plan' | 'question'
  sessionId: string
  request: PendingPermission
}

export interface EnrollInboxItem extends ItemBase {
  kind: 'enroll'
  enroll: EnrollItem
}

export interface SessionInboxItem extends ItemBase {
  /**
   * `waiting`：会话摘要说它在等审批，但这个页面还没有那条请求（刷新后还没同步完、节点离线）：来自缓存的摘要，
   * 没有请求详情、没有答复按钮，只能打开会话；请求到了之后由真正的请求条目取代。
   */
  kind: 'follow_up' | 'error' | 'waiting'
  sessionId: string
  session: SessionItem
}

/** 「待验收」段：派发过、`activity = review` 的清单项（daemon 算的）。动作：通过、再改改、打开改动（验收页，spec/web.md § 4.12）。不计入数字，不推送。 */
export interface ReviewInboxItem extends ItemBase {
  kind: 'review'
  todo: ViewTodo
}

/** 「稍后」段：agent 记的、还没人收下的清单项（`unreviewed`）。动作：收下、丢掉（不是删除）。不计入数字，不推送。 */
export interface LaterInboxItem extends ItemBase {
  kind: 'later'
  todo: ViewTodo
}

export type InboxItem = RequestItem | EnrollInboxItem | SessionInboxItem | ReviewInboxItem | LaterInboxItem

export interface InboxInputs {
  sessions: Readonly<Record<string, SessionItem>>
  pending: Readonly<Record<string, PendingPermission>>
  enroll: { items: Readonly<Record<string, EnrollItem>>; resolved: Readonly<Record<string, EnrollResolution>> }
  /** 左栏节点过滤器勾选的节点。 */
  selected: readonly string[]
  now: number
  /** 勾选的、支持清单的节点上的清单项（缓存叠上待发改动）。 */
  todos?: readonly ViewTodo[]
}

export interface InboxModel {
  sections: { id: InboxSectionId; items: InboxItem[] }[]
  /** 正在跑的会话（含「完成后提醒」的）：页面底部折叠成一行。 */
  inProgress: SessionItem[]
}

const REQUEST_KIND: Record<PermissionKind, RequestItem['kind']> = {
  tool: 'permission',
  mode_change: 'permission',
  other: 'permission',
  plan: 'plan',
  question: 'question',
}

/** 等审批：摘要里写着有请求在等，或状态就是 waiting_permission。 */
function isWaiting(s: Pick<SessionItem, 'pending_permissions' | 'status'>): boolean {
  return s.pending_permissions > 0 || s.status === 'waiting_permission'
}

/**
 * 「现在」段的顺序：有到期时间的在前（先到期的在前），其余按等待时长（等得最久的在前）；时间一样按 key，顺序稳定。
 * 到期时间来自请求自己的 `expires_at_ms` 或接入请求的有效期，两者都是「再不处理就失效」。
 */
export function compareNow(a: InboxItem, b: InboxItem): number {
  if ((a.expiresAtMs !== null) !== (b.expiresAtMs !== null)) return a.expiresAtMs !== null ? -1 : 1
  if (a.expiresAtMs !== null && b.expiresAtMs !== null && a.expiresAtMs !== b.expiresAtMs) return a.expiresAtMs - b.expiresAtMs
  if (a.sinceMs !== b.sinceMs) return a.sinceMs - b.sinceMs
  return a.key < b.key ? -1 : a.key > b.key ? 1 : 0
}

/**
 * 出错的会话算等用户的事：没归档、`status = error`、最近 7 天内（`updated_at_ms`）。`interrupted`（daemon 重启留下的）不算，
 * 下一次发送会自动续上。`error` 是会话的持久状态、没有「已读」：重新发消息、归档或过了期限它自己就不在了。
 */
export function isRecentError(s: Pick<SessionItem, 'status' | 'archived' | 'updated_at_ms'>, now: number): boolean {
  return s.status === 'error' && !s.archived && s.updated_at_ms >= now - ERROR_WINDOW_MS
}

export function deriveInbox(i: InboxInputs): InboxModel {
  const picked = new Set(i.selected)
  const now: InboxItem[] = []
  // 已经以权限请求的身份列在「现在」里的会话：不再作为「待处理」会话重复列，也不算「进行中」
  const requested = new Set<string>()

  for (const p of Object.values(i.pending)) {
    if (!picked.has(p.node_id)) continue
    requested.add(p.session_id)
    now.push({
      key: `request:${p.request_id}`,
      section: 'now',
      kind: REQUEST_KIND[p.kind],
      nodeId: p.node_id,
      sessionId: p.session_id,
      request: p,
      sinceMs: p.created_at_ms,
      expiresAtMs: p.expires_at_ms ?? null,
    })
  }

  for (const e of pendingApprovals(i.enroll, i.now)) {
    if (!picked.has(e.nodeId)) continue
    now.push({ key: `enroll:${e.nodeId}:${e.info.enroll_id}`, section: 'now', kind: 'enroll', nodeId: e.nodeId, enroll: e, sinceMs: e.createdAt, expiresAtMs: e.expiresAt })
  }

  const inProgress: SessionItem[] = []
  for (const s of Object.values(i.sessions)) {
    if (!picked.has(s.node_id) || s.archived) continue
    // 已经以权限请求列在「现在」里了：不重复列，也不算「进行中」或「待处理」
    if (requested.has(s.id)) continue
    if (isWaiting(s)) {
      now.push({ key: `waiting:${s.id}`, section: 'now', kind: 'waiting', nodeId: s.node_id, sessionId: s.id, session: s, sinceMs: s.updated_at_ms, expiresAtMs: null })
      continue
    }
    if (isActiveStatus(s.status)) {
      inProgress.push(s)
      continue
    }
    // 标了「待处理」又出错的会话只列一次，按「待处理」算：用户明确标过它；清掉标记后它仍在 7 天内就以出错的身份回来
    if (isFollowUpReady(s)) {
      now.push({ key: `session:${s.id}`, section: 'now', kind: 'follow_up', nodeId: s.node_id, sessionId: s.id, session: s, sinceMs: s.updated_at_ms, expiresAtMs: null })
    } else if (isRecentError(s, i.now)) {
      now.push({ key: `session:${s.id}`, section: 'now', kind: 'error', nodeId: s.node_id, sessionId: s.id, session: s, sinceMs: s.updated_at_ms, expiresAtMs: null })
    }
  }

  now.sort(compareNow)
  inProgress.sort((a, b) => b.updated_at_ms - a.updated_at_ms)
  // 待验收：派发过的项 `activity = review`，等得最久的在前（没有「开始等」的时刻，用最近一次修改的时间）。验收（通过 / 不要了）之后状态变成关闭，自然不在这里
  const review: InboxItem[] = (i.todos ?? [])
    .filter((t) => isReview(t) && picked.has(t.node_id))
    .map((t): InboxItem => ({ key: `review:${t.node_id}:${t.id}`, section: 'review', kind: 'review', nodeId: t.node_id, todo: t, sinceMs: t.updated_ms, expiresAtMs: null }))
    .sort(compareNow)
  // 稍后：agent 记的、还没人收下的项，等得最久的在前。收下或丢掉之后它们就不在这里了（丢掉 = 状态 dropped，不物理删除）
  const later: InboxItem[] = (i.todos ?? [])
    .filter((t) => t.unreviewed && !isClosed(t.state) && picked.has(t.node_id))
    .map((t): InboxItem => ({ key: `later:${t.node_id}:${t.id}`, section: 'later', kind: 'later', nodeId: t.node_id, todo: t, sinceMs: t.created_ms, expiresAtMs: null }))
    .sort(compareNow)
  return { sections: [{ id: 'now', items: now }, { id: 'review', items: review }, { id: 'later', items: later }], inProgress }
}

/** 「现在」段的条目。 */
export function nowItems(model: InboxModel): InboxItem[] {
  return model.sections.find((s) => s.id === 'now')?.items ?? []
}

/** 所有段里的条目，按段的顺序：键盘移动、选中和焦点都在这一串上。 */
export function allItems(model: InboxModel): InboxItem[] {
  return model.sections.flatMap((s) => s.items)
}

/** 导航项、应用角标和标签页标题的数字：只算「现在」段（「待验收」「稍后」不计入：它们不卡着正在跑的活，数量写在各自的段标题上）。 */
export function inboxNowCount(model: InboxModel): number {
  return nowItems(model).length
}

/** 「进行中」里标了「完成后提醒」的会话。 */
export function isArmed(s: SessionItem): boolean {
  return isFollowUp(s) && isActiveStatus(s.status)
}
