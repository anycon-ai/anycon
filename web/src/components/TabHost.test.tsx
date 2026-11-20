// 手机上标签页常驻（spec/web.md § 14.2）：切走再切回来，滚动位置和没发出去的输入还在；进二级页时藏起来、不能被读屏 / 键盘够到。
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'

const state = vi.hoisted(() => ({ pathname: '/inbox' }))
const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, {})
  return { ...m, useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: state.pathname } }) : null), Navigate: (p: { to: string }) => <span data-testid="navigate" data-to={p.to} /> }
})
vi.mock('@/pages/InboxPage', () => ({ InboxPage: ({ active }: { active?: boolean }) => <div data-testid="page-inbox" data-active={String(active)}><input aria-label="draft" /><div data-testid="scroller" /></div> }))
vi.mock('@/pages/SessionsPage', () => ({ SessionsPage: () => <div data-testid="page-sessions" /> }))
vi.mock('@/pages/MorePage', () => ({ MorePage: () => <div data-testid="page-more" /> }))

import { PhoneTabs, TabRoute } from './TabHost'

beforeEach(() => {
  resetViewport()
  state.pathname = '/inbox'
})

describe('PhoneTabs', () => {
  it('mounts a tab the first time it is visited, not before', () => {
    render(<PhoneTabs />)
    expect(screen.getByTestId('page-inbox')).toBeInTheDocument()
    expect(screen.queryByTestId('page-sessions')).toBeNull()
    expect(screen.queryByTestId('page-more')).toBeNull()
  })

  it('keeps a tab mounted but hidden and inert when another tab is shown, so its input and scroll position survive', async () => {
    const user = userEvent.setup()
    const { rerender } = render(<PhoneTabs />)
    await user.type(screen.getByLabelText('draft'), 'half an answer')
    // 滚动位置在这个 DOM 节点上：只要节点还是同一个（没有卸载重建），位置就还在
    const scroller = screen.getByTestId('scroller')
    state.pathname = '/sessions'
    rerender(<PhoneTabs />)
    const inbox = screen.getByTestId('tab-pane-inbox')
    expect(inbox).toHaveAttribute('inert')
    expect(inbox.className).toMatch(/invisible/)
    expect(screen.getByTestId('tab-pane-sessions')).not.toHaveAttribute('inert')
    state.pathname = '/inbox'
    rerender(<PhoneTabs />)
    expect(screen.getByLabelText('draft')).toHaveValue('half an answer')
    expect(screen.getByTestId('scroller')).toBe(scroller)
    expect(screen.getByTestId('tab-pane-inbox')).not.toHaveAttribute('inert')
  })

  it('hides every tab while a secondary page (the session page, say) is shown, and the inbox is told it is not active', () => {
    const { rerender } = render(<PhoneTabs />)
    state.pathname = '/n/abc/s/xyz'
    rerender(<PhoneTabs />)
    expect(screen.getByTestId('tab-pane-inbox')).toHaveAttribute('inert')
    expect(screen.getByTestId('page-inbox')).toHaveAttribute('data-active', 'false')
    state.pathname = '/inbox'
    rerender(<PhoneTabs />)
    expect(screen.getByTestId('tab-pane-inbox')).not.toHaveAttribute('inert')
  })
})

describe('TabRoute', () => {
  it('on a phone draws nothing: the shell hosts the tab', () => {
    setViewport('phone')
    render(<TabRoute tab="inbox" />)
    expect(screen.queryByTestId('page-inbox')).toBeNull()
  })

  it('on a wide screen draws the inbox itself, and sends /sessions and /more back to it (the sidebar has those)', () => {
    const { unmount } = render(<TabRoute tab="inbox" />)
    expect(screen.getByTestId('page-inbox')).toBeInTheDocument()
    unmount()
    render(<TabRoute tab="more" />)
    expect(screen.getByTestId('navigate')).toHaveAttribute('data-to', '/inbox')
    act(() => undefined)
  })
})
