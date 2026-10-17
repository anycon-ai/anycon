import type { TFunction } from 'i18next'
import { describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import { rateLimitedText } from './rateLimit'

describe('rateLimitedText', () => {
  const t = i18n.getFixedT('en') as unknown as TFunction

  it('keeps short waits in seconds', () => {
    expect(rateLimitedText(t, 4)).toBe('Too many attempts. Try again in 4 s.')
    expect(rateLimitedText(t, 119)).toContain('119 s')
  })

  it('formats long waits as hours and minutes', () => {
    const text = rateLimitedText(t, 4 * 3600 - 60)
    expect(text).toContain('3h 59m')
    expect(text).not.toMatch(/\d{4,}/)
  })
})
