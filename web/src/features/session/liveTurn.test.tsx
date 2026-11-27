// 「用户看着时实时结束」的判定，以及提示音 / 震动两个偏好（默认关）。
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionEvent } from '@/protocol'
import { ev, resetSeq } from '@/test/factories'
import { usePrefs } from '@/stores/prefs'
import type { OpenSession } from '@/stores/sessions'
import { applyEvents, buildView, emptyView } from './reducer'
import { FINISH_ANIMATE_MS, useLiveTurnEnd } from './liveTurn'

const h = vi.hoisted(() => ({ chime: vi.fn(), vibrate: vi.fn() }))
vi.mock('./feedback', () => ({ playChime: h.chime, vibrateOnce: h.vibrate }))

function openOf(events: SessionEvent[], over: Partial<OpenSession> = {}): OpenSession {
  return { nodeId: 'n', sessionId: 'sess_a', events, view: events.length ? buildView(events) : emptyView(), lowSeq: events[0]?.seq ?? 0, hasMoreOlder: false, phase: 'ready', catchingUp: false, held: [], watching: true, baseline: null, ...over }
}
const next = (o: OpenSession, events: Event[], over: Partial<OpenSession> = {}): OpenSession => {
  const added = events.map((e) => ev(e))
  return { ...o, events: o.events.concat(added), view: applyEvents(o.view, added), ...over }
}
const START: Event = { type: 'turn_start', turn_id: 1, message_id: 'm1' }
const DONE: Event = { type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 1000 } }

beforeEach(() => {
  resetSeq()
  h.chime.mockReset()
  h.vibrate.mockReset()
  usePrefs.setState({ turnSound: false, turnVibrate: false })
})
afterEach(() => vi.useRealTimers())

describe('useLiveTurnEnd', () => {
  it('marks a turn that ends while the user watches, then stops animating but keeps the check', () => {
    vi.useFakeTimers()
    const running = openOf([ev(START), ev({ type: 'text_delta', text: 'hi' })])
    const { result, rerender } = renderHook(({ o }) => useLiveTurnEnd(o), { initialProps: { o: running } })
    expect(result.current).toBeNull()
    const ended = next(running, [DONE])
    rerender({ o: ended })
    const key = ended.view.blocks.at(-1)!.key
    expect(result.current).toEqual({ key, animate: true })
    act(() => vi.advanceTimersByTime(FINISH_ANIMATE_MS + 10))
    expect(result.current).toEqual({ key, animate: false })
  })

  it('does not mark a turn that was already over when the history was opened', () => {
    const { result } = renderHook(() => useLiveTurnEnd(openOf([ev(START), ev(DONE)])))
    expect(result.current).toBeNull()
  })

  it('does not mark a turn whose end arrives while catching up (reconnect, first sync), even if it looked running in the cache', () => {
    const cached = openOf([ev(START)])
    const { result, rerender } = renderHook(({ o }) => useLiveTurnEnd(o), { initialProps: { o: cached } })
    const syncing = { ...cached, catchingUp: true }
    rerender({ o: syncing })
    const caught = next(syncing, [DONE])
    rerender({ o: caught })
    rerender({ o: { ...caught, catchingUp: false } })
    expect(result.current).toBeNull()
  })

  it('does not mark older turns loaded by scrolling up', () => {
    const idle = openOf([ev(START), ev(DONE)])
    const { result, rerender } = renderHook(({ o }) => useLiveTurnEnd(o), { initialProps: { o: idle } })
    // 往前翻页：视图重建，多了更早的块，但没有进行中的 turn
    rerender({ o: { ...idle, view: buildView(idle.events) } })
    expect(result.current).toBeNull()
  })

  it('does not mark a turn that ended while the tab was in the background', () => {
    const vis = vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden')
    const running = openOf([ev(START)])
    const { result, rerender } = renderHook(({ o }) => useLiveTurnEnd(o), { initialProps: { o: running } })
    rerender({ o: next(running, [DONE]) })
    expect(result.current).toBeNull()
    vis.mockRestore()
  })
})

describe('sound and vibration', () => {
  const endLive = (done: Event = DONE): void => {
    const running = openOf([ev(START)])
    const { rerender } = renderHook(({ o }) => useLiveTurnEnd(o), { initialProps: { o: running } })
    rerender({ o: next(running, [done]) })
  }

  it('are off by default: a live finish makes no sound and no vibration', () => {
    usePrefs.persist.clearStorage()
    expect(usePrefs.getInitialState().turnSound).toBe(false)
    expect(usePrefs.getInitialState().turnVibrate).toBe(false)
    endLive()
    expect(h.chime).not.toHaveBeenCalled()
    expect(h.vibrate).not.toHaveBeenCalled()
  })

  it('play once each when turned on', () => {
    usePrefs.setState({ turnSound: true, turnVibrate: true })
    endLive()
    expect(h.chime).toHaveBeenCalledTimes(1)
    expect(h.vibrate).toHaveBeenCalledTimes(1)
  })

  it('stay quiet for a stopped turn', () => {
    usePrefs.setState({ turnSound: true, turnVibrate: true })
    endLive({ type: 'turn_done', turn_id: 1, reason: 'cancelled', usage: { duration_ms: 1000 } })
    expect(h.chime).not.toHaveBeenCalled()
    expect(h.vibrate).not.toHaveBeenCalled()
  })
})
