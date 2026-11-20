// 「待处理」个数变化的状态播报（spec/web.md § 7）：读一整句话，不是只读数字；第一次不播；连着变几次只读最后一句。
import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'
import { InboxAnnouncer, SETTLE_TIMEOUT_MS } from './InboxAnnouncer'

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

describe('boot and reconnect', () => {
  const setNode = (over: Partial<ReturnType<typeof nodeEntry>>): void => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, ...over }) }, order: [A], loaded: true })

  it('says nothing while the first sync is in progress, however the count moves, and takes its baseline when it is done', () => {
    setNode({ conn: 'noise_handshaking', synced: false })
    render(<InboxAnnouncer />)
    act(() => setPending('1', '2'))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('')
    // 同步完：2 件是基线，不播
    act(() => setNode({ conn: 'ready', synced: true }))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('')
    // 之后的真实变化才播
    act(() => setPending('1', '2', '3'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('3 things are waiting for you')
  })

  it('a reconnect re-sync is silent too, and so is syncing state (syncing = true)', () => {
    setNode({ conn: 'ready', synced: true })
    setPending('1')
    render(<InboxAnnouncer />)
    act(() => setNode({ conn: 'hub_connecting', synced: false }))
    act(() => setPending('1', '2', '3'))
    act(() => setNode({ conn: 'ready', synced: false, syncing: true }))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('')
    act(() => setNode({ conn: 'ready', synced: true, syncing: false }))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('')
    act(() => setPending())
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('Nothing is waiting for you now')
  })

  it('an offline node counts as settled (there will be no sync), so real changes from the others still get read', () => {
    setNode({ conn: 'peer_offline', synced: false })
    render(<InboxAnnouncer />)
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
  })

  it('nodes that will never sync (needs re-pairing, version mismatch) count as settled', () => {
    setNode({ conn: 'noise_failed', synced: false })
    render(<InboxAnnouncer />)
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
  })

  it('a node whose first sync never finishes does not keep the region silent for ever: after a while the baseline is taken anyway', () => {
    setNode({ conn: 'ready', synced: false, syncing: false })
    render(<InboxAnnouncer />)
    act(() => setPending('1', '2'))
    act(() => void vi.advanceTimersByTime(SETTLE_TIMEOUT_MS + 100))
    expect(live()).toHaveTextContent('')
    act(() => setPending('1', '2', '3'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('3 things are waiting for you')
  })

  it('the 15 s fallback is per node: a stuck node A is not waited for, but another node syncing still holds the announcements back', () => {
    const B = 'b'.repeat(32)
    const both = (a: Partial<ReturnType<typeof nodeEntry>>, b: Partial<ReturnType<typeof nodeEntry>>): void => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, ...a }), [B]: nodeEntry({ id: B, ...b }) }, order: [A, B], loaded: true })
    both({ conn: 'ready', synced: false }, { conn: 'ready', synced: true })
    render(<InboxAnnouncer />)
    act(() => void vi.advanceTimersByTime(SETTLE_TIMEOUT_MS + 100))
    // A 卡住超时了，B 是定下来的：基线已取，真实变化照播
    act(() => setPending('1'))
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
    // B 开始重连同步：数字怎么变都不播，直到 B 定下来（A 仍然卡着，也不再等它）
    act(() => both({ conn: 'ready', synced: false }, { conn: 'noise_handshaking', synced: false, syncing: false }))
    act(() => setPending('1', '2', '3'))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
    act(() => both({ conn: 'ready', synced: false }, { conn: 'ready', synced: true }))
    act(() => void vi.advanceTimersByTime(2000))
    expect(live()).toHaveTextContent('1 thing is waiting for you')
    act(() => setPending())
    act(() => void vi.advanceTimersByTime(700))
    expect(live()).toHaveTextContent('Nothing is waiting for you now')
  })
})

