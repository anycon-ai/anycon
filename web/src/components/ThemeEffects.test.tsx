// 界面语言生效时，也记给 service worker（它读不到 localStorage），推送通知才用同一种语言（ADR-37）。
import { render, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import { usePrefs } from '@/stores/prefs'
import { readSwLanguage } from '@/sw/directory'
import { ThemeEffects } from './ThemeEffects'

afterEach(async () => {
  usePrefs.setState({ language: 'auto' })
  await i18n.changeLanguage('en')
  indexedDB.deleteDatabase('anycon-sw')
})

describe('ThemeEffects — language for the service worker', () => {
  it('stores a language the user picked, and follows it when it changes', async () => {
    usePrefs.setState({ language: 'zh' })
    render(<ThemeEffects />)
    await waitFor(async () => expect(await readSwLanguage()).toBe('zh'))
    usePrefs.setState({ language: 'en' })
    await waitFor(async () => expect(await readSwLanguage()).toBe('en'))
  })

  it('"auto" stores nothing and leaves no database behind: the service worker follows the browser language by the same rule', async () => {
    Object.defineProperty(navigator, 'language', { configurable: true, value: 'zh-CN' })
    try {
      usePrefs.setState({ language: 'auto' })
      render(<ThemeEffects />)
      await waitFor(() => expect(document.documentElement.lang).toBe('zh'))
      await new Promise((r) => setTimeout(r, 30))
      expect((await indexedDB.databases()).map((d) => d.name)).not.toContain('anycon-sw')
      expect(await readSwLanguage()).toBeUndefined()
    } finally {
      Reflect.deleteProperty(navigator, 'language')
    }
  })

  it('going back from a picked language to "auto" removes the stored one', async () => {
    usePrefs.setState({ language: 'zh' })
    render(<ThemeEffects />)
    await waitFor(async () => expect(await readSwLanguage()).toBe('zh'))
    usePrefs.setState({ language: 'auto' })
    await waitFor(async () => expect((await indexedDB.databases()).map((d) => d.name)).not.toContain('anycon-sw'))
  })
})
