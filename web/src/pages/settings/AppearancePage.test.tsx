// /settings/appearance：「禁止页面缩放」只在 iOS / Android 上出现，默认关。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it } from 'vitest'
import { usePrefs } from '@/stores/prefs'
import { AppearancePage } from './AppearancePage'

const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'

afterEach(() => {
  Reflect.deleteProperty(navigator, 'userAgent')
  usePrefs.setState({ blockZoom: false })
})

describe('AppearancePage — block page zoom', () => {
  it('is not offered on desktop', () => {
    render(<AppearancePage />)
    expect(screen.queryByTestId('block-zoom')).toBeNull()
  })

  it('is offered on a phone, off by default, and the switch saves the preference', async () => {
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value: IPHONE })
    const user = userEvent.setup()
    render(<AppearancePage />)
    const sw = screen.getByTestId('block-zoom')
    expect(sw).toHaveAttribute('aria-checked', 'false')
    expect(screen.getByText(/no longer zooms the page in/)).toBeInTheDocument()
    await user.click(sw)
    expect(usePrefs.getState().blockZoom).toBe(true)
  })
})
