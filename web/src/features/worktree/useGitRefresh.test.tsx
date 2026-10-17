import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event } from '@/protocol'
import { ev, gitStatus, resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useGit } from '@/stores/git'
import { useSessions } from '@/stores/sessions'
import { FILE_CHANGED_DEBOUNCE_MS } from './git'
import { refreshKind, useGitRefresh } from './useGitRefresh'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc
const statusCalls = () => rpc.calls.filter((c) => c.method === 'session.git_status')

function openWith(events: Event[]): void {
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events.map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready' })
}
const push = (...events: Event[]): void => {
  act(() => {
    useSessions.getState().ingest(SID, events.map((e) => ev(e)))
  })
}

beforeEach(() => {
  resetSeq()
  rpc = new FakeRpc()
  rpc.handlers['session.git_status'] = () => gitStatus()
  setRpc(rpc)
  useGit.setState({ bySession: {} })
})
afterEach(() => {
  setRpc(null)
  vi.useRealTimers()
})

describe('refreshKind', () => {
  it('turn_done and git events refresh at once; file_changed alone is debounced; everything else does nothing', () => {
    const e = (event: Event) => ev(event)
    expect(refreshKind([e({ type: 'turn_done', turn_id: 1, reason: 'completed' })])).toBe('now')
    expect(refreshKind([e({ type: 'git', action: 'sync_merged', branch: 'b' })])).toBe('now')
    expect(refreshKind([e({ type: 'file_changed', path: 'a', kind: 'modified' })])).toBe('debounced')
    expect(refreshKind([e({ type: 'file_changed', path: 'a', kind: 'modified' }), e({ type: 'turn_done', turn_id: 1, reason: 'completed' })])).toBe('now')
    expect(refreshKind([e({ type: 'text_delta', text: 'x' }), e({ type: 'status', state: 'running' })])).toBeNull()
  })
})

describe('useGitRefresh (no polling)', () => {
  it('pulls once when entering the session, and not at all while disabled', async () => {
    openWith([{ type: 'user_message', message_id: 'm', content: 'hi', by_device: 'me' }])
    const { rerender } = renderHook(({ enabled }) => useGitRefresh({ nodeId: NODE, sessionId: SID, enabled }), { initialProps: { enabled: false } })
    await act(async () => {})
    expect(statusCalls()).toHaveLength(0)
    rerender({ enabled: true }) // 比如 worktree 建好了
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
  })

  it('refreshes right after turn_done and after a git event', async () => {
    openWith([{ type: 'turn_start', turn_id: 1, message_id: 'm' }])
    renderHook(() => useGitRefresh({ nodeId: NODE, sessionId: SID, enabled: true }))
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    push({ type: 'text_delta', text: 'working' }, { type: 'status', state: 'running' })
    await act(async () => {})
    expect(statusCalls()).toHaveLength(1)
    push({ type: 'turn_done', turn_id: 1, reason: 'completed' })
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(2))
    push({ type: 'git', action: 'sync_merged', branch: 'b', base: 'main', count: 1 })
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(3))
  })

  it('debounces file_changed by 2 seconds: a burst of edits makes one refresh', async () => {
    openWith([{ type: 'turn_start', turn_id: 1, message_id: 'm' }])
    renderHook(() => useGitRefresh({ nodeId: NODE, sessionId: SID, enabled: true }))
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    vi.useFakeTimers()
    push({ type: 'file_changed', path: 'a.ts', kind: 'modified' })
    await vi.advanceTimersByTimeAsync(1500)
    push({ type: 'file_changed', path: 'b.ts', kind: 'created' })
    await vi.advanceTimersByTimeAsync(FILE_CHANGED_DEBOUNCE_MS - 1)
    expect(statusCalls()).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(statusCalls()).toHaveLength(2)
    await vi.advanceTimersByTimeAsync(30_000)
    expect(statusCalls()).toHaveLength(2)
  })

  it('does not refresh when older history is loaded above (scrolling up is not a change)', async () => {
    useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
    const older = [ev({ type: 'turn_done', turn_id: 1, reason: 'completed' }), ev({ type: 'file_changed', path: 'old.ts', kind: 'modified' })]
    useSessions.getState().openStart(NODE, SID, [ev({ type: 'turn_start', turn_id: 2, message_id: 'm2' })])
    useSessions.getState().openPatch(SID, { phase: 'ready' })
    // 把 seq 更小的旧事件排到前面
    const newest = useSessions.getState().open[SID]!.events
    const reseq = older.map((e, i) => ({ ...e, seq: i - 10 }))
    expect(newest[0]!.seq).toBeGreaterThan(reseq[1]!.seq)
    renderHook(() => useGitRefresh({ nodeId: NODE, sessionId: SID, enabled: true }))
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    act(() => useSessions.getState().prepend(SID, reseq, false))
    vi.useFakeTimers()
    await vi.advanceTimersByTimeAsync(FILE_CHANGED_DEBOUNCE_MS + 500)
    expect(statusCalls()).toHaveLength(1)
  })

  it('stops reacting and drops its state when the page is left', async () => {
    openWith([{ type: 'turn_start', turn_id: 1, message_id: 'm' }])
    const { unmount } = renderHook(() => useGitRefresh({ nodeId: NODE, sessionId: SID, enabled: true }))
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    expect(useGit.getState().bySession[SID]).toBeDefined()
    vi.useFakeTimers()
    push({ type: 'file_changed', path: 'a.ts', kind: 'modified' })
    unmount()
    await vi.advanceTimersByTimeAsync(FILE_CHANGED_DEBOUNCE_MS * 2)
    expect(statusCalls()).toHaveLength(1)
    expect(useGit.getState().bySession[SID]).toBeUndefined()
  })
})
