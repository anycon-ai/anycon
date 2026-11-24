// 升级后第一次打开的说明（spec/web.md § 4.11）：老用户看一次，新用户不看，关掉就记住。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { resetViewport, setViewport } from '@/test/viewport'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { HomeNotice } from './HomeNotice'

const A = 'a'.repeat(32)

beforeEach(() => {
  resetViewport()
  usePrefs.setState({ homeNoticeSeen: false })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
})

describe('home notice', () => {
  it('appears once for someone who has not seen it, saying the home page changed and where the node home went', () => {
    render(<HomeNotice />)
    const dialog = screen.getByRole('dialog', { name: 'Your home page is now Inbox' })
    expect(dialog).toHaveTextContent('Node home')
  })

  it('on a phone it points at the menu button instead of the sidebar', () => {
    setViewport('phone')
    render(<HomeNotice />)
    expect(screen.getByRole('dialog')).toHaveTextContent('the button at the top left')
  })

  it('“Got it” closes it and remembers, so it does not come back', async () => {
    const user = userEvent.setup()
    const { unmount } = render(<HomeNotice />)
    await user.click(screen.getByTestId('home-notice-ok'))
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(usePrefs.getState().homeNoticeSeen).toBe(true)
    expect(JSON.parse(localStorage.getItem('anycon.prefs') ?? '{}').state.homeNoticeSeen).toBe(true)
    unmount()
    render(<HomeNotice />)
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('Esc counts as having seen it', async () => {
    const user = userEvent.setup()
    render(<HomeNotice />)
    await user.keyboard('{Escape}')
    expect(usePrefs.getState().homeNoticeSeen).toBe(true)
  })

  it('does not show for someone who has seen it, or who has no paired node (nothing to explain yet)', () => {
    usePrefs.setState({ homeNoticeSeen: true })
    const { unmount } = render(<HomeNotice />)
    expect(screen.queryByRole('dialog')).toBeNull()
    unmount()
    usePrefs.setState({ homeNoticeSeen: false })
    useNodes.setState({ byId: {}, order: [], loaded: true })
    render(<HomeNotice />)
    expect(screen.queryByRole('dialog')).toBeNull()
  })
})
