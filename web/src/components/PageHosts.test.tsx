// 手机上「待处理」「清单」常驻（spec/web.md § 3.3）：去会话页再回来，滚动位置和没发出去的输入还在；藏着时读屏 / 键盘够不到。
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
vi.mock('@/pages/TodosPage', () => ({ TodosPage: ({ active }: { active?: boolean }) => <div data-testid="page-todos" data-active={String(active)} /> }))

import { keptPageOf, KeptRoute, PhonePages, ToInbox } from './PageHosts'

beforeEach(() => {
  resetViewport()
  state.pathname = '/inbox'
})

describe('keptPageOf', () => {
  it('is the inbox and the to-do list themselves, not the pages under them', () => {
    expect(keptPageOf('/inbox')).toBe('inbox')
    expect(keptPageOf('/todos/')).toBe('todos')
    expect(keptPageOf('/todos/abc/t1')).toBeNull()
    expect(keptPageOf('/n/abc/s/xyz')).toBeNull()
    expect(keptPageOf('/sessions')).toBeNull()
  })
})

describe('PhonePages', () => {
  it('mounts a page the first time it is visited, not before', () => {
    render(<PhonePages />)
    expect(screen.getByTestId('page-inbox')).toBeInTheDocument()
    expect(screen.queryByTestId('page-todos')).toBeNull()
  })

  it('keeps the inbox mounted but hidden and inert while another page is shown, so its input and scroll position survive', async () => {
    const user = userEvent.setup()
    const { rerender } = render(<PhonePages />)
    await user.type(screen.getByLabelText('draft'), 'half an answer')
    // 滚动位置在这个 DOM 节点上：只要节点还是同一个（没有卸载重建），位置就还在
    const scroller = screen.getByTestId('scroller')
    state.pathname = '/n/abc/s/xyz'
    rerender(<PhonePages />)
    const inbox = screen.getByTestId('kept-page-inbox')
    expect(inbox).toHaveAttribute('inert')
    expect(inbox.className).toMatch(/invisible/)
    expect(screen.getByTestId('page-inbox')).toHaveAttribute('data-active', 'false')
    state.pathname = '/inbox'
    rerender(<PhonePages />)
    expect(screen.getByLabelText('draft')).toHaveValue('half an answer')
    expect(screen.getByTestId('scroller')).toBe(scroller)
    expect(screen.getByTestId('kept-page-inbox')).not.toHaveAttribute('inert')
  })

  it('switching between the inbox and the to-do list keeps both', () => {
    const { rerender } = render(<PhonePages />)
    state.pathname = '/todos'
    rerender(<PhonePages />)
    expect(screen.getByTestId('kept-page-inbox')).toHaveAttribute('inert')
    expect(screen.getByTestId('kept-page-todos')).not.toHaveAttribute('inert')
    expect(screen.getByTestId('page-todos')).toHaveAttribute('data-active', 'true')
  })
})

describe('routes', () => {
  it('on a phone the route draws nothing: the shell hosts the page', () => {
    setViewport('phone')
    render(<KeptRoute page="inbox" />)
    expect(screen.queryByTestId('page-inbox')).toBeNull()
  })

  it('on a wide screen the route draws the page itself', () => {
    render(<KeptRoute page="inbox" />)
    expect(screen.getByTestId('page-inbox')).toBeInTheDocument()
  })

  it('/sessions and /more, the old phone tabs, go to the inbox', () => {
    render(<ToInbox />)
    expect(screen.getByTestId('navigate')).toHaveAttribute('data-to', '/inbox')
    act(() => undefined)
  })
})
