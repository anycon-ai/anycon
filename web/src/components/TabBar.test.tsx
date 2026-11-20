// 手机底部标签栏（spec/web.md § 14.2）：三个标签、当前页标 `aria-current`、「待处理」带个数、标签栏只在标签页的根页面出现。
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate))

import { TABS, TabBar, tabOfPath } from './TabBar'

describe('tabOfPath', () => {
  it('knows the root page of each tab, with or without a trailing slash', () => {
    expect(tabOfPath('/inbox')).toBe('inbox')
    expect(tabOfPath('/sessions')).toBe('sessions')
    expect(tabOfPath('/more/')).toBe('more')
  })

  it('treats every other page as a full-screen secondary page without the bar', () => {
    for (const p of ['/', '/n/abc', '/n/abc/s/xyz', '/n/abc/new', '/n/abc/settings', '/settings', '/settings/notifications', '/inbox/x', '/admin', '/n/abc/files/a/b']) expect(tabOfPath(p)).toBeNull()
  })
})

describe('TabBar', () => {
  it('is a labelled navigation with Inbox, Sessions and More, in that order', () => {
    render(<TabBar current="sessions" inboxCount={0} />)
    const nav = screen.getByRole('navigation', { name: 'Main navigation' })
    expect(within(nav).getAllByRole('link').map((a) => a.textContent)).toEqual(['Inbox', 'Sessions', 'More'])
    expect(TABS.map((t) => t.to)).toEqual(['/inbox', '/sessions', '/more'])
  })

  it('marks the current tab with aria-current (and an underline, so it is not colour alone)', () => {
    render(<TabBar current="more" inboxCount={0} />)
    expect(screen.getByTestId('tab-more')).toHaveAttribute('aria-current', 'page')
    expect(screen.getByTestId('tab-inbox')).not.toHaveAttribute('aria-current')
    expect(screen.getByTestId('tab-sessions')).not.toHaveAttribute('aria-current')
    expect(screen.getByText('More').className).toMatch(/underline/)
  })

  it('shows the Now count on Inbox, and reads it in the accessible name', () => {
    render(<TabBar current="inbox" inboxCount={3} />)
    expect(screen.getByTestId('tab-inbox-count')).toHaveTextContent('3')
    expect(screen.getByTestId('tab-inbox')).toHaveAccessibleName('Inbox, 3 waiting')
  })

  it('has no number when nothing waits, and caps a big one', () => {
    const { rerender } = render(<TabBar current="inbox" inboxCount={0} />)
    expect(screen.queryByTestId('tab-inbox-count')).toBeNull()
    expect(screen.getByTestId('tab-inbox')).toHaveAccessibleName('Inbox')
    rerender(<TabBar current="inbox" inboxCount={250} />)
    expect(screen.getByTestId('tab-inbox-count')).toHaveTextContent('99+')
  })

  it('is phone-only and keeps 44px touch targets and the safe-area inset', () => {
    render(<TabBar current="inbox" inboxCount={0} />)
    expect(screen.getByTestId('tab-bar').className).toMatch(/md:hidden/)
    expect(screen.getByTestId('tab-bar').className).toMatch(/pb-safe/)
    for (const t of TABS) expect(screen.getByTestId(`tab-${t.id}`).className).toMatch(/min-h-14/)
  })

  it('has no Todos tab or centre “+” yet: nothing that leads nowhere', () => {
    render(<TabBar current="inbox" inboxCount={0} />)
    expect(screen.getAllByRole('link')).toHaveLength(3)
    expect(screen.queryByRole('button')).toBeNull()
  })
})
