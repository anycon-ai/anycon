// 从搜索命中打开会话（`?seq=`，spec/web.md § 9）：第一次同步完之后把历史加载到那个 seq、滚到那条消息并高亮，
// 然后把 `?seq=` 从地址里去掉；加载不到 / 失败也不挡住打开会话。
import { act, render, screen } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
const loadHistoryTo = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
  loadHistoryTo,
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
const clearedParam = { to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: SID }, search: {}, replace: true }
const scrolled = vi.fn()

function seed(phase: 'loading' | 'ready' = 'ready'): void {
  const events = [
    ev({ type: 'user_message', message_id: 'm1', content: 'list files', by_device: 'me' }),
    ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }),
    ev({ type: 'text_delta', text: 'done' }),
  ]
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events)
  useSessions.getState().openPatch(SID, { phase, watching: true })
}

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  loadHistoryTo.mockReset()
  loadHistoryTo.mockResolvedValue('reached')
  scrolled.mockReset()
  vi.spyOn(Element.prototype, 'scrollIntoView').mockImplementation(function (this: Element) {
    scrolled(this)
  })
  setRpc(new FakeRpc())
  useUi.setState({ toasts: [] })
  seed()
})

describe('SessionPage — opened from a search hit', () => {
  it('loads the history up to the seq, scrolls to that message and highlights it, then removes ?seq= from the address', async () => {
    await act(async () => void render(<SessionPage sessionId={SID} focusSeq={1} />))
    expect(loadHistoryTo).toHaveBeenCalledWith(SID, 1)
    expect(scrolled).toHaveBeenCalledTimes(1)
    const row = screen.getByText('list files').closest('[data-index]')
    expect(scrolled.mock.calls[0]![0]).toBe(row)
    expect(row).toHaveClass('search-flash')
    expect(navigate).toHaveBeenCalledWith(clearedParam)
    expect(useUi.getState().toasts).toEqual([])
  })

  it('shows the busy overlay while the history is loading, then not', async () => {
    let done!: (v: 'reached') => void
    loadHistoryTo.mockReturnValue(new Promise((resolve) => (done = resolve)))
    await act(async () => void render(<SessionPage sessionId={SID} focusSeq={1} />))
    expect(screen.getByTestId('message-list-finding')).toBeInTheDocument()
    expect(scrolled).not.toHaveBeenCalled()
    await act(async () => done('reached'))
    expect(screen.queryByTestId('message-list-finding')).toBeNull()
    expect(scrolled).toHaveBeenCalledTimes(1)
  })

  it('waits for the first sync of the session to finish before it loads anything', async () => {
    seed('loading')
    await act(async () => void render(<SessionPage sessionId={SID} focusSeq={1} />))
    expect(loadHistoryTo).not.toHaveBeenCalled()
    act(() => useSessions.getState().openPatch(SID, { catchingUp: true }))
    act(() => useSessions.getState().openPatch(SID, { phase: 'ready' }))
    expect(loadHistoryTo).not.toHaveBeenCalled() // 追赶中的历史会盖掉刚并进去的
    await act(async () => useSessions.getState().openPatch(SID, { catchingUp: false }))
    expect(loadHistoryTo).toHaveBeenCalledWith(SID, 1)
    expect(navigate).toHaveBeenCalledWith(clearedParam)
  })

  it('tells the user when the message is too far back, and still opens the session', async () => {
    loadHistoryTo.mockResolvedValue('too_far')
    await act(async () => void render(<SessionPage sessionId={SID} focusSeq={1} />))
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual(['That message is too far back to jump to. Scroll up to load more.'])
    expect(navigate).toHaveBeenCalledWith(clearedParam)
    expect(screen.getByText('list files')).toBeInTheDocument()
  })

  it('reports a failure to load (for example the node went offline), does not scroll, and still removes ?seq=', async () => {
    loadHistoryTo.mockRejectedValue(new RpcCallError('offline', 'session.history: node link not ready'))
    await act(async () => void render(<SessionPage sessionId={SID} focusSeq={1} />))
    expect(useUi.getState().toasts.filter((t) => t.kind === 'error')).toHaveLength(1)
    expect(scrolled).not.toHaveBeenCalled()
    expect(navigate).toHaveBeenCalledWith(clearedParam)
  })

  it('does nothing special without ?seq=', async () => {
    await act(async () => void render(<SessionPage sessionId={SID} />))
    expect(loadHistoryTo).not.toHaveBeenCalled()
    expect(navigate).not.toHaveBeenCalled()
    expect(scrolled).not.toHaveBeenCalled()
  })
})
