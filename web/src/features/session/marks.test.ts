import { describe, expect, it } from 'vitest'
import type { SessionStatus } from '@/protocol'
import { attentionCount, isFollowUp, isFollowUpArmed, isFollowUpReady, isUnread } from './marks'

const s = (over: Partial<{ status: SessionStatus; archived: boolean; follow_up: boolean; read_seq: number; last_seq: number }> = {}) => ({ status: 'idle' as SessionStatus, archived: false, follow_up: false, last_seq: 10, read_seq: 10, ...over })

describe('follow-up', () => {
  it('is armed while the session runs and ready once it stops', () => {
    for (const status of ['running', 'starting', 'queued', 'waiting_permission'] as const) {
      expect(isFollowUpArmed(s({ follow_up: true, status }))).toBe(true)
      expect(isFollowUpReady(s({ follow_up: true, status }))).toBe(false)
    }
    for (const status of ['idle', 'error', 'ended', 'interrupted'] as const) {
      expect(isFollowUpReady(s({ follow_up: true, status }))).toBe(true)
      expect(isFollowUpArmed(s({ follow_up: true, status }))).toBe(false)
    }
  })

  it('ignores archived sessions and unmarked ones', () => {
    expect(isFollowUp(s({ follow_up: true, archived: true }))).toBe(false)
    expect(isFollowUpReady(s({ follow_up: true, archived: true }))).toBe(false)
    expect(isFollowUp(s())).toBe(false)
    expect(isFollowUp({ archived: false })).toBe(false)
  })
})

describe('isUnread', () => {
  it('lights up when a stopped session has output past the read watermark', () => {
    expect(isUnread(s({ last_seq: 12, read_seq: 10 }))).toBe(true)
    expect(isUnread(s({ last_seq: 12, read_seq: 12 }))).toBe(false)
    expect(isUnread(s({ status: 'error', last_seq: 12, read_seq: 3 }))).toBe(true)
  })

  it('stays dark while the session runs, and for archived sessions', () => {
    expect(isUnread(s({ status: 'running', last_seq: 99, read_seq: 1 }))).toBe(false)
    expect(isUnread(s({ status: 'waiting_permission', last_seq: 99, read_seq: 1 }))).toBe(false)
    expect(isUnread(s({ archived: true, last_seq: 99, read_seq: 1 }))).toBe(false)
  })

  it('treats a missing watermark as read', () => {
    const { read_seq: _drop, ...rest } = s({ last_seq: 50 })
    expect(isUnread(rest)).toBe(false)
  })
})

describe('attentionCount', () => {
  const item = (id: string, over: Parameters<typeof s>[0] = {}, pending = 0) => ({ id, pending_permissions: pending, ...s(over) })

  it('adds approvals and ready follow-ups, without counting a session twice', () => {
    const pending = { r1: { session_id: 'a' }, r2: { session_id: 'a' } }
    const sessions = [item('a', { follow_up: true }, 2), item('b', { follow_up: true }), item('c', { follow_up: true, status: 'running' }), item('d'), item('e', { follow_up: true, archived: true })]
    expect(attentionCount(pending, sessions)).toBe(3)
  })

  it('does not count unread sessions', () => {
    expect(attentionCount({}, [item('a', { last_seq: 20, read_seq: 1 })])).toBe(0)
  })
})
