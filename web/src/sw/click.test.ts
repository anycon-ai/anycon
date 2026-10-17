import { describe, expect, it, vi } from 'vitest'
import { handleNotificationClick, NAVIGATE_MESSAGE, pickWindow, safeClickPath } from './click'
import type { WindowClientLike } from './types'

const ORIGIN = 'https://hub.example'

function win(url: string, over: Partial<WindowClientLike> = {}) {
  return { url, focus: vi.fn(async () => undefined), postMessage: vi.fn(), ...over } satisfies WindowClientLike
}

function setup(windows: WindowClientLike[]) {
  const openWindow = vi.fn(async () => undefined)
  return { openWindow, deps: { origin: ORIGIN, windows: async () => windows, openWindow } }
}

describe('safeClickPath', () => {
  it('keeps same-origin absolute paths (with query)', () => {
    expect(safeClickPath({ url: '/n/abcd1234/s/sess_1' }, ORIGIN)).toBe('/n/abcd1234/s/sess_1')
    expect(safeClickPath({ url: '/n/abcd1234/settings?tab=enroll&review=1' }, ORIGIN)).toBe('/n/abcd1234/settings?tab=enroll&review=1')
  })
  it.each([undefined, null, 'x', {}, { url: 3 }, { url: 'https://evil.example/' }, { url: '//evil.example' }, { url: '/\\evil.example' }, { url: 'javascript:alert(1)' }, { url: 'n/relative' }])('anything else goes home: %j', (data) => {
    expect(safeClickPath(data, ORIGIN)).toBe('/')
  })
  it.each(['/\t/evil.example', '/\n/evil.example', '/\r\\evil.example', '/ /evil.example', '/\u00a0/evil.example'])('a path the browser would turn into another site (tab / newline / space inside) goes home: %j', (url) => {
    expect(safeClickPath({ url }, ORIGIN)).toBe('/')
  })
})

describe('pickWindow', () => {
  it('only considers windows of this origin', () => {
    expect(pickWindow([win('https://evil.example/'), win('not a url')], ORIGIN)).toBeUndefined()
  })
  it('prefers the focused window, then a visible one, then the first', () => {
    const a = win(`${ORIGIN}/a`)
    const b = win(`${ORIGIN}/b`, { visibilityState: 'visible' })
    const c = win(`${ORIGIN}/c`, { focused: true })
    expect(pickWindow([a, b, c], ORIGIN)).toBe(c)
    expect(pickWindow([a, b], ORIGIN)).toBe(b)
    expect(pickWindow([a], ORIGIN)).toBe(a)
  })
})

describe('handleNotificationClick', () => {
  it('focuses an open window of this origin and tells the page to navigate (no reload, no new window)', async () => {
    const w = win(`${ORIGIN}/n/abcd1234`)
    const { deps, openWindow } = setup([win('https://evil.example/'), w])
    await handleNotificationClick({ url: '/n/abcd1234/s/sess_1' }, deps)
    expect(w.focus).toHaveBeenCalledTimes(1)
    expect(w.postMessage).toHaveBeenCalledWith({ type: NAVIGATE_MESSAGE, url: '/n/abcd1234/s/sess_1' })
    expect(openWindow).not.toHaveBeenCalled()
  })

  it('opens a new window at the absolute URL when no window of this origin is open', async () => {
    const { deps, openWindow } = setup([win('https://evil.example/')])
    await handleNotificationClick({ url: '/n/abcd1234/settings?tab=enroll&review=1' }, deps)
    expect(openWindow).toHaveBeenCalledWith(`${ORIGIN}/n/abcd1234/settings?tab=enroll&review=1`)
  })

  it('opens a new window when focusing the existing one fails', async () => {
    const w = win(`${ORIGIN}/`, { focus: vi.fn(async () => { throw new Error('not allowed') }) })
    const { deps, openWindow } = setup([w])
    await handleNotificationClick({ url: '/settings/notifications' }, deps)
    expect(w.postMessage).not.toHaveBeenCalled()
    expect(openWindow).toHaveBeenCalledWith(`${ORIGIN}/settings/notifications`)
  })

  it('a hostile or missing URL in the notification data opens the home page, never another origin', async () => {
    const w = win(`${ORIGIN}/`)
    const { deps } = setup([w])
    await handleNotificationClick({ url: '//evil.example/x' }, deps)
    expect(w.postMessage).toHaveBeenCalledWith({ type: NAVIGATE_MESSAGE, url: '/' })
    await handleNotificationClick({ url: '/\t/evil.example' }, deps)
    expect(w.postMessage).toHaveBeenLastCalledWith({ type: NAVIGATE_MESSAGE, url: '/' })
    const none0 = setup([])
    await handleNotificationClick({ url: '/\n/evil.example' }, none0.deps)
    expect(none0.openWindow).toHaveBeenCalledWith(`${ORIGIN}/`)
    const none = setup([])
    await handleNotificationClick(undefined, none.deps)
    expect(none.openWindow).toHaveBeenCalledWith(`${ORIGIN}/`)
  })
})
