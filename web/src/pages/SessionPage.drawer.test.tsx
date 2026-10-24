// 右侧信息面板：≥1200px 内联，更窄时是抽屉。拖动窗口宽度跨过断点不能让抽屉自己弹出来。
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'

/** 可以「拖动窗口」的 matchMedia：min-width 查询按当前宽度求值，宽度变了就通知订阅者。 */
function fakeViewport(initial: number) {
  let width = initial
  const listeners = new Map<string, Set<() => void>>()
  const spy = vi.spyOn(window, 'matchMedia').mockImplementation((query: string) => {
    const min = Number(/min-width:\s*(\d+)px/.exec(query)?.[1] ?? Number.NaN)
    const subs = listeners.get(query) ?? new Set<() => void>()
    listeners.set(query, subs)
    return {
      get matches() {
        return Number.isNaN(min) ? false : width >= min
      },
      media: query,
      addEventListener: (_: string, cb: () => void) => subs.add(cb),
      removeEventListener: (_: string, cb: () => void) => subs.delete(cb),
    } as unknown as MediaQueryList
  })
  return {
    resize(w: number) {
      width = w
      act(() => listeners.forEach((subs) => subs.forEach((cb) => cb())))
    },
    restore: () => spy.mockRestore(),
  }
}

beforeEach(() => {
  resetSeq()
  setRpc(new FakeRpc())
  usePrefs.setState({ infoPanelOpen: true })
  useUi.setState({ toasts: [], rightDrawerOpen: false })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [])
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
})
afterEach(() => setRpc(null))

describe('SessionPage: right info drawer across the 1200px breakpoint', () => {
  it('does not pop up when the window is narrowed again after it was left open and widened past the breakpoint', async () => {
    const vp = fakeViewport(1000)
    try {
      const user = userEvent.setup()
      render(<SessionPage sessionId={SID} />)
      await user.click(await screen.findByTestId('info-toggle'))
      expect(useUi.getState().rightDrawerOpen).toBe(true)

      vp.resize(1400) // 变宽：面板内联，抽屉被卸载
      expect(useUi.getState().rightDrawerOpen).toBe(false)

      vp.resize(1000) // 再变窄：不该有抽屉
      expect(screen.queryByRole('dialog')).toBeNull()
    } finally {
      vp.restore()
    }
  })
})
