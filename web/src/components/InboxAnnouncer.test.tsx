// 「待处理」个数变化的状态播报（spec/web.md § 14.10）：读一整句话，不是只读数字；第一次不播；连着变几次只读最后一句。
import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'
import { InboxAnnouncer } from './InboxAnnouncer'

const A = 'a'.repeat(32)
const req = (id: string): PendingPermission => ({ request_id: id, session_id: `s-${id}`, node_id: A, kind: 'tool', title: id, detail: {}, options: [], created_at_ms: 1, expires_at_ms: null })
const setPending = (...ids: string[]): void => useSessions.setState({ pending: Object.fromEntries(ids.map((i) => [i, req(i)])) })

beforeEach(() => {
  vi.useFakeTimers()
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {} })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})
afterEach(() => vi.useRealTimers())

const live = (): HTMLElement => screen.getByRole('status')

describe('inbox live region', () => {
  it('is a polite status region that is empty at first, even if things are already waiting', () => {
    setPending('1', '2')
    render(<InboxAnnouncer />)
    expect(live()).toHaveAttribute('aria-live', 'polite')
    expect(live()).toHaveTextContent('')
  })

  it('reads a whole sentence when the count changes', () => {
    render(<InboxAnnouncer />)
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
    act(() => setPending('1', '2', '3'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('3 things are waiting for you')
    act(() => setPending())
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('Nothing is waiting for you now')
  })

  it('merges a burst of changes into the last sentence', () => {
    render(<InboxAnnouncer />)
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(200))
    act(() => setPending('1', '2'))
    act(() => void vi.advanceTimersByTime(200))
    act(() => setPending('1', '2', '3', '4'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('4 things are waiting for you')
  })

  it('says nothing when the count does not change', () => {
    setPending('1')
    render(<InboxAnnouncer />)
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('')
  })
})
