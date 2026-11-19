// 已读上报：会话页开着、页面可见、会话停下来之后，把显示到的位置报给 daemon。
import { renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useSessions } from '@/stores/sessions'
import { useMarkRead } from './useMarkRead'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc
const reports = () => rpc.calls.filter((c) => c.method === 'session.mark_read').map((c) => c.params)

function setVisibility(v: 'visible' | 'hidden'): void {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
  document.dispatchEvent(new Event('visibilitychange'))
}

type Args = Parameters<typeof useMarkRead>[0]
const args = (over: Partial<Args> = {}): Args => ({ nodeId: NODE, sessionId: SID, lastSeq: 9, settled: true, ready: true, ...over })

beforeEach(() => {
  vi.useFakeTimers()
  resetSeq()
  rpc = new FakeRpc()
  rpc.handlers['session.mark_read'] = () => ({})
  setRpc(rpc)
  setVisibility('visible')
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, last_seq: 9, read_seq: 2 }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
})
afterEach(() => {
  vi.useRealTimers()
  setRpc(null)
})

describe('useMarkRead', () => {
  it('reports the newest displayed seq once the session has stopped, after a short pause', async () => {
    renderHook(() => useMarkRead(args()))
    expect(reports()).toEqual([])
    await vi.advanceTimersByTimeAsync(400)
    expect(reports()).toEqual([{ session_id: SID, seq: 9 }])
    expect(useSessions.getState().list[SID]!.read_seq).toBe(9)
  })

  it('does not report while the session runs, and reports when it stops', async () => {
    const { rerender } = renderHook((p: Args) => useMarkRead(p), { initialProps: args({ settled: false, lastSeq: 20 }) })
    await vi.advanceTimersByTimeAsync(2000)
    expect(reports()).toEqual([])
    rerender(args({ settled: true, lastSeq: 25 }))
    await vi.advanceTimersByTimeAsync(400)
    expect(reports()).toEqual([{ session_id: SID, seq: 25 }])
  })

  it('does not report while the page is hidden, or before the history is ready, or when nothing is new', async () => {
    setVisibility('hidden')
    const hidden = renderHook(() => useMarkRead(args()))
    await vi.advanceTimersByTimeAsync(1000)
    expect(reports()).toEqual([])
    hidden.unmount()
    setVisibility('visible')
    renderHook(() => useMarkRead(args({ ready: false })))
    renderHook(() => useMarkRead(args({ lastSeq: 2 })))
    await vi.advanceTimersByTimeAsync(1000)
    expect(reports()).toEqual([])
  })

  it('reports the last position when the page is left while the session still runs', async () => {
    const { unmount } = renderHook(() => useMarkRead(args({ settled: false, lastSeq: 15 })))
    await vi.advanceTimersByTimeAsync(1000)
    expect(reports()).toEqual([])
    unmount()
    expect(reports()).toEqual([{ session_id: SID, seq: 15 }])
  })
})
