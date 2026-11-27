// /settings/appearance：「禁止页面缩放」只在 iOS / Android 上出现，默认关；「侧栏显示最近会话」默认开；turn 完成的提示音默认关，震动只在 Android 上出现、默认关。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { usePrefs } from '@/stores/prefs'
import { AppearancePage } from './AppearancePage'

const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'

afterEach(() => {
  Reflect.deleteProperty(navigator, 'userAgent')
  usePrefs.setState({ blockZoom: false, sidebarRecent: true, sidebarFoldTodo: true })
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

describe('AppearancePage — sidebar Recent group', () => {
  it('is on by default, and the switch saves the preference', async () => {
    const user = userEvent.setup()
    render(<AppearancePage />)
    const sw = screen.getByTestId('sidebar-recent-switch')
    expect(sw).toHaveAttribute('aria-checked', 'true')
    await user.click(sw)
    expect(usePrefs.getState().sidebarRecent).toBe(false)
    expect(sw).toHaveAttribute('aria-checked', 'false')
  })
})

describe('AppearancePage — folding to-do sessions in the sidebar', () => {
  it('is on by default, and the switch saves the preference', async () => {
    const user = userEvent.setup()
    render(<AppearancePage />)
    const sw = screen.getByTestId('sidebar-fold-todo-switch')
    expect(sw).toHaveAttribute('aria-checked', 'true')
    await user.click(sw)
    expect(usePrefs.getState().sidebarFoldTodo).toBe(false)
  })
})

describe('AppearancePage — sound and vibration when a turn finishes', () => {
  const ANDROID = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36'
  afterEach(() => {
    usePrefs.setState({ turnSound: false, turnVibrate: false })
    Reflect.deleteProperty(navigator, 'vibrate')
  })

  it('sound is off by default, and the switch saves the preference', async () => {
    const user = userEvent.setup()
    render(<AppearancePage />)
    const sw = screen.getByTestId('turn-sound-switch')
    expect(sw).toHaveAttribute('aria-checked', 'false')
    await user.click(sw)
    expect(usePrefs.getState().turnSound).toBe(true)
  })

  it('vibration is not offered where it cannot work (desktop, iOS Safari without navigator.vibrate)', () => {
    render(<AppearancePage />)
    expect(screen.queryByTestId('turn-vibrate-switch')).toBeNull()
  })

  it('vibration is offered on Android, off by default', async () => {
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value: ANDROID })
    const vibrate = vi.fn(() => true)
    Object.defineProperty(navigator, 'vibrate', { configurable: true, value: vibrate })
    const user = userEvent.setup()
    render(<AppearancePage />)
    const sw = screen.getByTestId('turn-vibrate-switch')
    expect(sw).toHaveAttribute('aria-checked', 'false')
    expect(vibrate).not.toHaveBeenCalled()
    await user.click(sw)
    expect(usePrefs.getState().turnVibrate).toBe(true)
    // 打开时试一下
    expect(vibrate).toHaveBeenCalledTimes(1)
  })
})
