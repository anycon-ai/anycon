// 手机底部标签栏（spec/web.md § 14.2）：三个标签、当前页标 `aria-current`、「待处理」带个数、标签栏只在标签页的根页面出现。
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate))

import { useCapture } from '@/features/todos/capture'
import { TABS, TabBar, tabOfPath } from './TabBar'

describe('tabOfPath', () => {
  it('knows the root page of each tab, with or without a trailing slash', () => {
    expect(tabOfPath('/inbox')).toBe('inbox')
    expect(tabOfPath('/sessions')).toBe('sessions')
    expect(tabOfPath('/more/')).toBe('more')
    expect(tabOfPath('/todos')).toBe('todos')
  })

  it('treats every other page as a full-screen secondary page without the bar', () => {
    for (const p of ['/', '/n/abc', '/n/abc/s/xyz', '/n/abc/new', '/n/abc/settings', '/settings', '/settings/notifications', '/inbox/x', '/todos/aaaaaaaa/abc', '/capture', '/admin', '/n/abc/files/a/b']) expect(tabOfPath(p)).toBeNull()
  })
})

describe('TabBar', () => {
  it('is a labelled navigation with the five final positions: Inbox, To-dos, + (an action), Sessions, More', () => {
    render(<TabBar current="sessions" inboxCount={0} />)
    const nav = screen.getByRole('navigation', { name: 'Main navigation' })
    expect(within(nav).getAllByRole('link').map((a) => a.textContent)).toEqual(['Inbox', 'To-dos', 'Sessions', 'More'])
    expect(TABS.map((t) => t.to)).toEqual(['/inbox', '/todos', '/sessions', '/more'])
    // 位置：＋ 在正中，夹在清单和会话之间
    const order = [...nav.querySelectorAll('li')].map((li) => li.querySelector('[data-testid]')?.getAttribute('data-testid'))
    expect(order).toEqual(['tab-inbox', 'tab-todos', 'tab-capture', 'tab-sessions', 'tab-more'])
  })

  it('the centre + is a button that opens the capture sheet, not a link: no href, no aria-current, named “Add a to-do”', async () => {
    useCapture.setState({ open: false })
    render(<TabBar current="inbox" inboxCount={0} />)
    const plus = screen.getByTestId('tab-capture')
    expect(plus.tagName).toBe('BUTTON')
    expect(plus).not.toHaveAttribute('href')
    expect(plus).not.toHaveAttribute('aria-current')
    expect(plus).toHaveAccessibleName('Add a to-do')
    expect(screen.getByRole('button', { name: 'Add a to-do' })).toBe(plus)
    plus.click()
    expect(useCapture.getState().open).toBe(true)
  })

  it('shows the open to-do count on the To-dos tab, read in its name, and not in the Inbox number', () => {
    render(<TabBar current="inbox" inboxCount={1} todoCount={4} />)
    expect(screen.getByTestId('tab-todos')).toHaveAccessibleName('To-dos, 4 open')
    expect(screen.getByTestId('tab-todos-count')).toHaveTextContent('4')
    expect(screen.getByTestId('tab-inbox-count')).toHaveTextContent('1')
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
    expect(screen.getByTestId('tab-capture').className).toMatch(/min-h-14/)
  })


})
