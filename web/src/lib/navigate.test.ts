import { afterEach, describe, expect, it, vi } from 'vitest'
import { navigateTo, setNavigator } from './navigate'

afterEach(() => {
  setNavigator(null)
  vi.unstubAllGlobals()
})

describe('navigateTo', () => {
  it('goes through the registered router, only for same-site absolute paths', () => {
    const go = vi.fn()
    setNavigator(go)
    navigateTo('/n/abc/settings?tab=enroll')
    navigateTo('//evil.example/x')
    navigateTo('https://evil.example/x')
    navigateTo('javascript:alert(1)')
    expect(go.mock.calls).toEqual([['/n/abc/settings?tab=enroll']])
  })

  it('refuses the paths the browser would resolve to another site once it strips tabs and newlines', () => {
    const go = vi.fn()
    setNavigator(go)
    for (const evil of ['/\t/evil.example', '/\n/evil.example', '/\r\\evil.example', '/\\evil.example', '/\u00a0/evil.example', '/ /evil.example']) navigateTo(evil)
    expect(go).not.toHaveBeenCalled()
    navigateTo('/%09/evil') // 百分号编码的是字面字符，仍是本站的一条路径
    expect(go.mock.calls).toEqual([['/%09/evil']])
  })

  it('without a registered router it still never leaves the site', () => {
    const assign = vi.fn()
    vi.stubGlobal('location', { origin: 'http://localhost', assign })
    navigateTo('/\t/evil.example')
    navigateTo('//evil.example')
    expect(assign).not.toHaveBeenCalled()
    navigateTo('/n/abc')
    expect(assign).toHaveBeenCalledExactlyOnceWith('/n/abc')
  })
})
