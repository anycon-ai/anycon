// 会话列表（跨节点）+ 已打开会话的事件与渲染视图。
// 这里只有同步的状态变更；网络操作在 features/session/actions.ts。
import { create } from 'zustand'
import type { PermissionRequest, SessionChanged, SessionEvent, SessionSettings, SessionSummary } from '@/protocol'
import { applyEvents, buildView, emptyView, planIngest, type SessionView } from '@/features/session/reducer'
import * as db from '@/db'
import { storageFailed } from '@/lib/log'

export interface SessionItem extends SessionSummary {
  node_id: string
  /** 本地事件缓存里最大的 seq。 */
  cached_seq: number
}

export interface PendingPermission extends PermissionRequest {
  node_id: string
}

export interface OpenSession {
  nodeId: string
  sessionId: string
  events: SessionEvent[]
  view: SessionView
  /** 缓存区间下界；向上翻页后变小。 */
  lowSeq: number
  hasMoreOlder: boolean
  phase: 'loading' | 'ready' | 'error'
  error?: string
  catchingUp: boolean
  /** 有缺口时暂存的、更靠后的事件。 */
  held: SessionEvent[]
  watching: boolean
  /** 会话刚创建时的设置（仅当这个浏览器见证了创建）：settings_changed 系统行据此算「从什么变成什么」。 */
  baseline: SessionSettings | null
}

interface SessionsState {
  list: Record<string, SessionItem>
  pending: Record<string, PendingPermission>
  open: Record<string, OpenSession>
  hydrateNode: (nodeId: string, recs: db.SessionRecord[]) => void
  upsertSummaries: (nodeId: string, sums: SessionSummary[]) => void
  /** `nodeId`：只删属于该节点的（节点发来的通知不能删别的节点的会话）。 */
  removeSessions: (ids: string[], nodeId?: string) => void
  applyChanged: (nodeId: string, ch: SessionChanged) => void
  setPending: (nodeId: string, reqs: PermissionRequest[]) => void
  addPending: (nodeId: string, req: PermissionRequest) => void
  /** 只清除属于 `nodeId` 的那条待审批。 */
  resolvePending: (nodeId: string, requestId: string) => void
  // 已打开会话
  openStart: (nodeId: string, sessionId: string, cached: SessionEvent[], baseline?: SessionSettings | null) => void
  openPatch: (sessionId: string, patch: Partial<OpenSession>) => void
  openClose: (sessionId: string) => void
  /** 把事件接到已有事件后面；返回是否有缺口以及需要暂存的事件。 */
  ingest: (sessionId: string, events: SessionEvent[], opts?: { allowGap?: boolean }) => { gap: boolean; applied: SessionEvent[] }
  prepend: (sessionId: string, older: SessionEvent[], hasMore: boolean) => void
  replaceAll: (sessionId: string, events: SessionEvent[]) => void
}

function toRecord(i: SessionItem, low: number): db.SessionRecord {
  return {
    session_id: i.id,
    node_id: i.node_id,
    project: i.project ?? null,
    agent: i.agent,
    title: i.title,
    status: i.status,
    options: i.settings,
    last_seq: i.last_seq,
    updated_at: i.updated_at_ms,
    low_seq: low,
    summary: i,
  }
}

/**
 * 节点只能动自己的东西：一个恶意的已配对节点知道别的节点的会话 id / 请求 id 时，
 * 不能借通知或列表把它们改写、删除或注入事件。这里判断一个会话 / 请求是否已经属于别的节点。
 */
function ownedByOther(list: Record<string, SessionItem>, sessionId: string, nodeId: string): boolean {
  const s = list[sessionId]
  return !!s && s.node_id !== nodeId
}

function pendingConflicts(s: Pick<SessionsState, 'list' | 'pending'>, nodeId: string, req: PermissionRequest): boolean {
  const other = s.pending[req.request_id]
  return ownedByOther(s.list, req.session_id, nodeId) || (!!other && other.node_id !== nodeId)
}

export const useSessions = create<SessionsState>()((set, get) => ({
  list: {},
  pending: {},
  open: {},

  hydrateNode: (nodeId, recs) =>
    set((s) => {
      const list = { ...s.list }
      for (const r of recs) {
        if (r.node_id !== nodeId || list[r.session_id]) continue
        // 后台任务数只反映 daemon 内存里的当前状态，缓存里的旧值不能当现状：等 session.list / session.changed 带来新值
        const { background_tasks: _stale, ...rest } = r.summary
        list[r.session_id] = { ...rest, node_id: nodeId, cached_seq: r.low_seq > 0 ? r.last_seq : 0 }
      }
      return { list }
    }),

  upsertSummaries: (nodeId, rawSums) => {
    const sums = rawSums.filter((x) => {
      const bad = ownedByOther(get().list, x.id, nodeId)
      if (bad) console.warn(`anycon: node ${nodeId.slice(0, 8)} listed a session that belongs to another node; ignored`)
      return !bad
    })
    set((s) => {
      const list = { ...s.list }
      for (const sum of sums) {
        const prev = list[sum.id]
        list[sum.id] = { ...sum, node_id: nodeId, cached_seq: prev?.cached_seq ?? 0 }
      }
      return { list }
    })
    const items = sums.map((x) => get().list[x.id]).filter((x): x is SessionItem => !!x)
    db.putSessions(items.map((i) => toRecord(i, get().open[i.id]?.lowSeq ?? 0))).catch(storageFailed('cache the session list'))
  },

  removeSessions: (rawIds, nodeId) => {
    const ids = nodeId ? rawIds.filter((id) => !ownedByOther(get().list, id, nodeId)) : rawIds
    set((s) => {
      const list = { ...s.list }
      const open = { ...s.open }
      const pending = { ...s.pending }
      for (const id of ids) {
        delete list[id]
        delete open[id]
        for (const [rid, p] of Object.entries(pending)) if (p.session_id === id) delete pending[rid]
      }
      return { list, open, pending }
    })
    for (const id of ids) db.deleteSession(id).catch(storageFailed('delete the cached session'))
  },

  applyChanged: (nodeId, ch) => {
    if (ownedByOther(get().list, ch.session_id, nodeId)) {
      console.warn(`anycon: node ${nodeId.slice(0, 8)} sent session.changed for a session of another node; ignored`)
      return
    }
    if (ch.deleted) {
      get().removeSessions([ch.session_id], nodeId)
      return
    }
    set((s) => {
      const prev = s.list[ch.session_id]
      if (!prev) return s // 新会话由下一次 session.list / session.get 补齐元数据
      const next: SessionItem = {
        ...prev,
        node_id: nodeId,
        title: ch.title,
        status: ch.status,
        archived: ch.archived ?? prev.archived,
        last_seq: ch.last_seq,
        pending_permissions: ch.pending_permissions,
        // 通知是后台任务数的完整快照；旧 daemon 不发这个字段，按 0 处理
        background_tasks: ch.background_tasks ?? 0,
        updated_at_ms: ch.updated_at_ms,
        // worktree 只会变状态、不会消失；通知里没带就保持原样
        worktree: ch.worktree ?? prev.worktree,
      }
      return { list: { ...s.list, [ch.session_id]: next } }
    })
    const item = get().list[ch.session_id]
    if (item) db.putSessions([toRecord(item, get().open[item.id]?.lowSeq ?? 0)]).catch(storageFailed('cache the session list'))
  },

  setPending: (nodeId, reqs) =>
    set((s) => {
      const keep = Object.fromEntries(Object.entries(s.pending).filter(([, p]) => p.node_id !== nodeId))
      for (const r of reqs) {
        if (pendingConflicts({ list: s.list, pending: keep }, nodeId, r)) {
          console.warn(`anycon: node ${nodeId.slice(0, 8)} listed a permission request that belongs to another node; ignored`)
          continue
        }
        keep[r.request_id] = { ...r, node_id: nodeId }
      }
      return { pending: keep }
    }),
  addPending: (nodeId, req) =>
    set((s) => {
      if (pendingConflicts(s, nodeId, req)) {
        console.warn(`anycon: node ${nodeId.slice(0, 8)} sent a permission request for another node's session or request id; ignored`)
        return s
      }
      return { pending: { ...s.pending, [req.request_id]: { ...req, node_id: nodeId } } }
    }),
  resolvePending: (nodeId, requestId) =>
    set((s) => {
      const p = s.pending[requestId]
      if (!p || p.node_id !== nodeId) return s
      const { [requestId]: _gone, ...rest } = s.pending
      return { pending: rest }
    }),

  openStart: (nodeId, sessionId, cached, baseline = null) => {
    const sorted = cached.slice().sort((a, b) => a.seq - b.seq)
    const view = sorted.length > 0 ? buildView(sorted, sorted[0]!.seq <= 1 ? baseline : null) : emptyView(baseline)
    set((s) => ({
      open: {
        ...s.open,
        [sessionId]: {
          nodeId,
          sessionId,
          events: sorted,
          view,
          lowSeq: sorted[0]?.seq ?? 0,
          hasMoreOlder: sorted.length > 0 && (sorted[0]?.seq ?? 0) > 1,
          phase: sorted.length > 0 ? 'ready' : 'loading',
          catchingUp: false,
          held: [],
          watching: false,
          baseline,
        },
      },
    }))
  },

  openPatch: (sessionId, patch) =>
    set((s) => {
      const cur = s.open[sessionId]
      return cur ? { open: { ...s.open, [sessionId]: { ...cur, ...patch } } } : s
    }),

  openClose: (sessionId) =>
    set((s) => {
      const { [sessionId]: _gone, ...rest } = s.open
      return { open: rest }
    }),

  ingest: (sessionId, incoming, opts) => {
    const cur = get().open[sessionId]
    if (!cur) return { gap: false, applied: [] }
    const plan = planIngest(cur.view.lastSeq, incoming, opts)
    if (plan.apply.length > 0) {
      const view = applyEvents(cur.view, plan.apply)
      const events = cur.events.concat(plan.apply)
      set((s) => {
        const o = s.open[sessionId]
        if (!o) return s
        const item = s.list[sessionId]
        return {
          open: { ...s.open, [sessionId]: { ...o, events, view, held: plan.gap ? plan.ahead : o.held } },
          list: item ? { ...s.list, [sessionId]: { ...item, cached_seq: view.lastSeq } } : s.list,
        }
      })
      db.putEvents(sessionId, plan.apply).catch(storageFailed('cache the session events'))
    } else if (plan.gap) {
      set((s) => {
        const o = s.open[sessionId]
        return o ? { open: { ...s.open, [sessionId]: { ...o, held: plan.ahead } } } : s
      })
    }
    return { gap: plan.gap, applied: plan.apply }
  },

  prepend: (sessionId, older, hasMore) => {
    const cur = get().open[sessionId]
    if (!cur) return
    const known = new Set(cur.events.map((e) => e.seq))
    const fresh = older.filter((e) => !known.has(e.seq))
    if (fresh.length === 0) {
      get().openPatch(sessionId, { hasMoreOlder: hasMore })
      return
    }
    const events = fresh.concat(cur.events).sort((a, b) => a.seq - b.seq)
    const view = buildView(events, events[0]!.seq <= 1 ? cur.baseline : null)
    set((s) => ({ open: { ...s.open, [sessionId]: { ...cur, events, view, lowSeq: events[0]!.seq, hasMoreOlder: hasMore } } }))
    db.putEvents(sessionId, fresh).catch(storageFailed('cache the session events'))
  },

  replaceAll: (sessionId, events) => {
    const cur = get().open[sessionId]
    if (!cur) return
    const sorted = events.slice().sort((a, b) => a.seq - b.seq)
    const view = buildView(sorted, (sorted[0]?.seq ?? 0) <= 1 ? cur.baseline : null)
    set((s) => ({
      open: {
        ...s.open,
        [sessionId]: { ...cur, events: sorted, view, lowSeq: sorted[0]?.seq ?? 0, hasMoreOlder: (sorted[0]?.seq ?? 0) > 1, phase: 'ready' },
      },
    }))
  },
}))

export function nodeSessions(list: Record<string, SessionItem>, nodeId: string): SessionItem[] {
  return Object.values(list)
    .filter((s) => s.node_id === nodeId)
    .sort((a, b) => b.updated_at_ms - a.updated_at_ms)
}
