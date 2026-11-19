// 「待处理」里「现在」段的推导：每一种条目、标了待处理的会话的规则、出错会话的 7 天规则、节点过滤、排序和数字。
// 这些规则原来分散在左栏的两个置顶组和应用角标里（待审批 / 待处理的会话 / 计数不重复），现在都在这里。
import { describe, expect, it } from 'vitest'
import type { EnrollInfo, PermissionKind } from '@/protocol'
import { summary } from '@/test/factories'
import type { EnrollItem } from '@/stores/enroll'
import type { PendingPermission, SessionItem } from '@/stores/sessions'
import { ERROR_WINDOW_MS, deriveInbox, inboxNowCount, isArmed, isRecentError, nowItems, type InboxInputs } from './derive'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const NOW = 10 * 24 * 3600 * 1000
const DAY = 24 * 3600 * 1000

const session = (over: Partial<SessionItem> & { id: string }): SessionItem => ({ ...summary({ title: over.id, ...over }), node_id: A, cached_seq: 0, ...over })
const request = (id: string, over: Partial<PendingPermission> = {}): PendingPermission => ({
  request_id: id,
  session_id: `s-${id}`,
  node_id: A,
  kind: 'tool',
  title: `Run ${id}`,
  detail: {},
  options: [],
  created_at_ms: NOW - 1000,
  expires_at_ms: null,
  ...over,
})
const enrollItem = (id: string, over: Partial<EnrollItem> = {}, info: Partial<EnrollInfo> = {}): EnrollItem => ({
  nodeId: A,
  info: { enroll_id: id, expires_at_ms: NOW + 60_000, created_at_ms: NOW - 1000, status: 'wait_approval', method: 'code', approval: true, ...info },
  expiresAt: NOW + 60_000,
  receivedAt: NOW - 1000,
  createdAt: NOW - 1000,
  ...over,
})

function derive(over: { sessions?: SessionItem[]; pending?: PendingPermission[]; enroll?: EnrollItem[]; selected?: string[]; resolved?: InboxInputs['enroll']['resolved'] } = {}) {
  return deriveInbox({
    sessions: Object.fromEntries((over.sessions ?? []).map((s) => [s.id, s])),
    pending: Object.fromEntries((over.pending ?? []).map((p) => [p.request_id, p])),
    enroll: { items: Object.fromEntries((over.enroll ?? []).map((e) => [`${e.nodeId}:${e.info.enroll_id}`, e])), resolved: over.resolved ?? {} },
    selected: over.selected ?? [A],
    now: NOW,
  })
}
const keys = (m: ReturnType<typeof derive>): string[] => nowItems(m).map((i) => i.key)

describe('inbox items', () => {
  it('turns every kind of pending request into an item, with plans and questions apart from approvals', () => {
    const kinds: PermissionKind[] = ['tool', 'mode_change', 'other', 'plan', 'question']
    const m = derive({ pending: kinds.map((kind, i) => request(`r${i}`, { kind, created_at_ms: NOW - 1000 * (10 - i) })) })
    expect(nowItems(m).map((i) => i.kind)).toEqual(['permission', 'permission', 'permission', 'plan', 'question'])
  })

  it('lists enroll requests that wait for approval, not scan requests, resolved or expired ones', () => {
    const m = derive({
      enroll: [
        enrollItem('ok'),
        enrollItem('scan', {}, { method: 'scan' }),
        enrollItem('resolved'),
        enrollItem('expired', { expiresAt: NOW - 1 }),
        enrollItem('waiting', {}, { status: 'waiting_hello' }),
      ],
      resolved: { [`${A}:resolved`]: { result: 'approved', byDevice: 'x', at: NOW } },
    })
    expect(keys(m)).toEqual([`enroll:${A}:ok`])
  })

  it('lists sessions marked for follow-up that have stopped, not running, archived or unmarked ones', () => {
    const m = derive({
      sessions: [
        session({ id: 'idle', follow_up: true, status: 'idle' }),
        session({ id: 'ended', follow_up: true, status: 'ended' }),
        session({ id: 'run', follow_up: true, status: 'running' }),
        session({ id: 'arch', follow_up: true, archived: true }),
        session({ id: 'plain' }),
      ],
    })
    expect(keys(m).sort()).toEqual(['session:ended', 'session:idle'])
    expect(nowItems(m).every((i) => i.kind === 'follow_up')).toBe(true)
  })

  it('does not list a follow-up session again when it is already waiting for approval', () => {
    const m = derive({
      sessions: [
        session({ id: 'w', follow_up: true, status: 'idle', pending_permissions: 1 }),
        session({ id: 'w2', follow_up: true, status: 'waiting_permission' }),
        session({ id: 'r', follow_up: true, status: 'idle' }),
      ],
      pending: [request('q', { session_id: 'r' })],
    })
    expect(keys(m)).toEqual(['request:q'])
  })

  it('unread sessions are not items', () => {
    const m = derive({ sessions: [session({ id: 'u', status: 'idle', last_seq: 9, read_seq: 1 })] })
    expect(nowItems(m)).toEqual([])
  })
})

describe('errored sessions', () => {
  it('counts a session in error state within the last 7 days, and nothing else', () => {
    const m = derive({
      sessions: [
        session({ id: 'fresh', status: 'error', updated_at_ms: NOW - DAY }),
        session({ id: 'edge', status: 'error', updated_at_ms: NOW - ERROR_WINDOW_MS }),
        session({ id: 'old', status: 'error', updated_at_ms: NOW - ERROR_WINDOW_MS - 1 }),
        session({ id: 'arch', status: 'error', archived: true, updated_at_ms: NOW - DAY }),
        session({ id: 'int', status: 'interrupted', updated_at_ms: NOW - DAY }),
        session({ id: 'idle', status: 'idle', updated_at_ms: NOW - DAY }),
      ],
    })
    expect(keys(m).sort()).toEqual(['session:edge', 'session:fresh'])
    expect(nowItems(m).every((i) => i.kind === 'error')).toBe(true)
  })

  it('isRecentError is the same rule', () => {
    expect(isRecentError({ status: 'error', archived: false, updated_at_ms: NOW }, NOW)).toBe(true)
    expect(isRecentError({ status: 'error', archived: false, updated_at_ms: NOW - 8 * DAY }, NOW)).toBe(false)
    expect(isRecentError({ status: 'interrupted', archived: false, updated_at_ms: NOW }, NOW)).toBe(false)
  })

  it('a session that is both marked and errored is one item, a follow-up one', () => {
    const m = derive({ sessions: [session({ id: 'both', status: 'error', follow_up: true, updated_at_ms: NOW - DAY })] })
    expect(nowItems(m).map((i) => i.kind)).toEqual(['follow_up'])
    // 清掉标记之后，它还在 7 天内，就以出错的身份留在这里
    expect(nowItems(derive({ sessions: [session({ id: 'both', status: 'error', follow_up: false, updated_at_ms: NOW - DAY })] })).map((i) => i.kind)).toEqual(['error'])
  })
})

describe('node filter and in-progress', () => {
  it('only shows the selected nodes, for every kind of item', () => {
    const m = derive({
      selected: [A],
      sessions: [session({ id: 'mine', status: 'error', updated_at_ms: NOW }), session({ id: 'theirs', node_id: B, status: 'error', updated_at_ms: NOW })],
      pending: [request('a'), request('b', { node_id: B })],
      enroll: [enrollItem('a'), enrollItem('b', { nodeId: B })],
    })
    expect(keys(m).sort()).toEqual([`enroll:${A}:a`, 'request:a', 'session:mine'])
    expect(keys(derive({ selected: [A, B], pending: [request('a'), request('b', { node_id: B })] })).sort()).toEqual(['request:a', 'request:b'])
  })

  it('running sessions go to in-progress, flagged ones too, and never into Now', () => {
    const m = derive({
      sessions: [
        session({ id: 'run', status: 'running', updated_at_ms: 1 }),
        session({ id: 'armed', status: 'queued', follow_up: true, updated_at_ms: 2 }),
        session({ id: 'arch', status: 'running', archived: true }),
        session({ id: 'other', node_id: B, status: 'running' }),
        session({ id: 'asking', status: 'waiting_permission' }),
      ],
      pending: [request('q', { session_id: 'asking' })],
    })
    expect(nowItems(m).map((i) => i.key)).toEqual(['request:q'])
    expect(m.inProgress.map((s) => s.id)).toEqual(['armed', 'run'])
    expect(isArmed(m.inProgress[0]!)).toBe(true)
    expect(isArmed(m.inProgress[1]!)).toBe(false)
  })
})

describe('order of the Now section', () => {
  it('puts items that expire first, soonest first, then the rest by how long they have waited', () => {
    const m = derive({
      pending: [
        request('late', { expires_at_ms: NOW + 50_000 }),
        request('soon', { expires_at_ms: NOW + 5_000 }),
        request('older-no-expiry', { created_at_ms: NOW - 90_000 }),
      ],
      enroll: [enrollItem('e', { expiresAt: NOW + 20_000 })],
      sessions: [session({ id: 'f', follow_up: true, status: 'idle', updated_at_ms: NOW - 500_000 }), session({ id: 'e', status: 'error', updated_at_ms: NOW - 100_000 })],
    })
    expect(keys(m)).toEqual(['request:soon', `enroll:${A}:e`, 'request:late', 'session:f', 'session:e', 'request:older-no-expiry'])
  })

  it('follow-up sessions: the one that has waited longest comes first', () => {
    const m = derive({ sessions: [session({ id: 'new', follow_up: true, updated_at_ms: 2_000 }), session({ id: 'old', follow_up: true, updated_at_ms: 1_000 })] })
    expect(keys(m)).toEqual(['session:old', 'session:new'])
  })

  it('is stable: equal times fall back to the key', () => {
    const m = derive({ pending: [request('b'), request('a')] })
    expect(keys(m)).toEqual(['request:a', 'request:b'])
  })
})

describe('what the badge counts', () => {
  it('is the number of items in Now: approvals, enroll requests, follow-up sessions that stopped and errored sessions', () => {
    const m = derive({
      pending: [request('1'), request('2', { session_id: 's-1' })],
      enroll: [enrollItem('e')],
      sessions: [session({ id: 'f', follow_up: true }), session({ id: 'run', follow_up: true, status: 'running' }), session({ id: 'x', status: 'error', updated_at_ms: NOW }), session({ id: 'u', last_seq: 5, read_seq: 0 })],
    })
    // 两条请求（同一个会话也各算一条）+ 一条接入请求 + 一个待处理会话 + 一个出错会话；运行中的和未读的不算
    expect(inboxNowCount(m)).toBe(5)
  })

  it('is zero for an empty inbox', () => {
    expect(inboxNowCount(derive())).toBe(0)
  })
})
