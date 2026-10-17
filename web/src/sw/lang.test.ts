import { describe, expect, it } from 'vitest'
import { resolveLanguage } from './lang'

describe('service worker language', () => {
  it('uses the language the page stored', () => {
    expect(resolveLanguage('zh', 'en-US')).toBe('zh')
    expect(resolveLanguage('en', 'zh-CN')).toBe('en')
  })
  it('falls back to the browser language: zh* is Chinese, everything else English', () => {
    expect(resolveLanguage(undefined, 'zh-TW')).toBe('zh')
    expect(resolveLanguage(undefined, 'ZH')).toBe('zh')
    expect(resolveLanguage(undefined, 'fr-FR')).toBe('en')
    expect(resolveLanguage(undefined, undefined)).toBe('en')
    expect(resolveLanguage('xx', 'zh-CN')).toBe('zh')
  })
})
