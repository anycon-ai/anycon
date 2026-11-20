// 「首页换成待处理」说明的默认值：第一次启动时 localStorage 里没有任何偏好 = 新用户，算看过；
// 已经有偏好（但没有这一项）= 升级上来的老用户，没看过。
import { beforeEach, describe, expect, it, vi } from 'vitest'

async function freshPrefs() {
  vi.resetModules()
  return (await import('./prefs')).usePrefs
}

beforeEach(() => localStorage.clear())

describe('homeNoticeSeen default', () => {
  it('is true for a browser that never stored preferences', async () => {
    const prefs = await freshPrefs()
    expect(prefs.getState().homeNoticeSeen).toBe(true)
  })

  it('is false for a browser that stored preferences before this field existed', async () => {
    localStorage.setItem('anycon.prefs', JSON.stringify({ state: { theme: 'dark', lastNode: 'abc' }, version: 1 }))
    const prefs = await freshPrefs()
    expect(prefs.getState().homeNoticeSeen).toBe(false)
    expect(prefs.getState().theme).toBe('dark')
  })

  it('keeps what was stored', async () => {
    localStorage.setItem('anycon.prefs', JSON.stringify({ state: { homeNoticeSeen: true }, version: 1 }))
    const prefs = await freshPrefs()
    expect(prefs.getState().homeNoticeSeen).toBe(true)
  })

  it('dismissing it is written back', async () => {
    localStorage.setItem('anycon.prefs', JSON.stringify({ state: { theme: 'dark' }, version: 1 }))
    const prefs = await freshPrefs()
    prefs.getState().set({ homeNoticeSeen: true })
    expect(JSON.parse(localStorage.getItem('anycon.prefs')!).state.homeNoticeSeen).toBe(true)
  })
})
