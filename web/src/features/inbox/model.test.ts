// 「待处理」模型只算一份：输入没变（或只变了和待处理无关的字段）不重算，时间只在有东西会按时间过期时推进。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, summary } from '@/test/factories'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission, type SessionItem } from '@/stores/sessions'
import { inboxNowCount } from './derive'
import { getInboxSnapshot, inboxNowCountFromStores, subscribeInbox } from './model'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const row = (over: Partial<SessionItem> & { id: string }): SessionItem => ({ ...summary(over), node_id: A, cached_seq: 0, ...over })
const req = (id: string, node = A): PendingPermission => ({ request_id: id, session_id: `s-${id}`, node_id: node, kind: 'tool', title: id, detail: {}, options: [], created_at_ms: 1, expires_at_ms: null })

beforeEach(() => {
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {} })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }), [B]: nodeEntry({ id: B }) }, order: [A, B], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})
afterEach(() => vi.useRealTimers())

describe('inbox model', () => {
  it('is the same object while nothing relevant changes, even when a streamed event touches the session summary', () => {
    useSessions.setState({ list: { x: row({ id: 'x', status: 'error', updated_at_ms: Date.now() }) } })
    const first = getInboxSnapshot()
    expect(inboxNowCount(first.model)).toBe(1)
    // 打开的会话每来一条事件，`list` 里那一项的 cached_seq / last_seq 都会变
    useSessions.setState((s) => ({ list: { x: { ...s.list.x!, cached_seq: 9, last_seq: 9 } } }))
    expect(getInboxSnapshot()).toBe(first)
    useSessions.setState({ open: {} })
    expect(getInboxSnapshot()).toBe(first)
  })

  it('recomputes when something that matters changes: status, mark, pending permissions, requests, node filter', () => {
    useSessions.setState({ list: { x: row({ id: 'x', status: 'idle', follow_up: false }) } })
    const idle = getInboxSnapshot()
    expect(inboxNowCount(idle.model)).toBe(0)
    useSessions.setState((s) => ({ list: { x: { ...s.list.x!, follow_up: true } } }))
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(1)
    useSessions.setState((s) => ({ list: { x: { ...s.list.x!, status: 'running' } } }))
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(0)
    useSessions.getState().addPending(A, req('r'))
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(1)
    usePrefs.setState({ sidebarNodes: [B] })
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(0)
  })

  it('counts, for the node filter’s dot, what waits on the nodes outside the filter', () => {
    useSessions.getState().addPending(A, req('a'))
    useSessions.getState().addPending(B, req('b'))
    useSessions.getState().addPending(B, req('c'))
    usePrefs.setState({ sidebarNodes: [A] })
    const snap = getInboxSnapshot()
    expect(inboxNowCount(snap.model)).toBe(1)
    expect(snap.outside).toBe(2)
    usePrefs.setState({ sidebarNodes: null })
    expect(getInboxSnapshot().outside).toBe(0)
  })

  it('the app badge reads the same number', () => {
    useSessions.getState().addPending(A, req('a'))
    useSessions.getState().addPending(B, req('b'))
    usePrefs.setState({ sidebarNodes: [A] })
    expect(inboxNowCountFromStores()).toBe(1)
  })

  it('what the number counts: approvals (also plans and questions), device requests, stopped follow-ups, errors in the last 7 days, cached waiting sessions; not running sessions, unread or other nodes', () => {
    const now = Date.now()
    useSessions.setState({
      list: {
        f: row({ id: 'f', follow_up: true, status: 'idle' }),
        run: row({ id: 'run', follow_up: true, status: 'running' }),
        e: row({ id: 'e', status: 'error', updated_at_ms: now }),
        oldE: row({ id: 'oldE', status: 'error', updated_at_ms: now - 8 * 86_400_000 }),
        w: row({ id: 'w', status: 'waiting_permission' }),
        u: row({ id: 'u', last_seq: 5, read_seq: 0 }),
        other: row({ id: 'other', node_id: B, status: 'error', updated_at_ms: now }),
      },
    })
    useSessions.getState().addPending(A, { ...req('q'), kind: 'question' })
    useEnroll.getState().upsert(A, { enroll_id: 'en', expires_at_ms: now + 60_000, created_at_ms: now, status: 'wait_approval', method: 'code', approval: true })
    usePrefs.setState({ sidebarNodes: [A] })
    expect(inboxNowCountFromStores()).toBe(5)
  })
})

describe('timers', () => {
  it('ticks every second while a device request is counting down, every minute otherwise, and not at all without subscribers', () => {
    vi.useFakeTimers()
    const listener = vi.fn()
    const unsub = subscribeInbox(listener)
    vi.advanceTimersByTime(30_000)
    expect(listener).not.toHaveBeenCalled()
    vi.advanceTimersByTime(30_000)
    expect(listener).toHaveBeenCalledTimes(1)
    listener.mockClear()
    useEnroll.getState().upsert(A, { enroll_id: 'en', expires_at_ms: Date.now() + 60_000, created_at_ms: Date.now(), status: 'wait_approval', method: 'code', approval: true })
    // 来了接入请求：store 一变就按秒重排
    listener.mockClear()
    vi.advanceTimersByTime(3000)
    expect(listener).toHaveBeenCalledTimes(3)
    unsub()
    listener.mockClear()
    vi.advanceTimersByTime(120_000)
    expect(listener).not.toHaveBeenCalled()
    expect(vi.getTimerCount()).toBe(0)
  })

  it('an expired device request leaves the count without any store change', () => {
    vi.useFakeTimers()
    useEnroll.getState().upsert(A, { enroll_id: 'en', expires_at_ms: Date.now() + 5_000, created_at_ms: Date.now(), status: 'wait_approval', method: 'code', approval: true })
    expect(inboxNowCountFromStores()).toBe(1)
    expect(inboxNowCountFromStores(Date.now() + 10_000)).toBe(0)
  })
})
