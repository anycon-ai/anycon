import { describe, expect, it } from 'vitest'
import { cleanHintText, displayHint } from './hint'

describe('cleanHintText', () => {
  it('strips control characters and bidi controls from what the other device claims to be', () => {
    expect(cleanHintText('Chrome\u{0}\u{1b}[31m · Win\u{202e}dows\u{2066}x\u{2069}')).toBe('Chrome [31m · Win dows x')
    expect(cleanHintText('a\nb\tc\u{2028}d')).toBe('a b c d')
  })
  it('truncates to 64 characters by code point, not UTF-16 unit', () => {
    const long = '😀'.repeat(100)
    expect([...cleanHintText(long)]).toHaveLength(64)
    expect(cleanHintText(long)).toBe('😀'.repeat(64))
    expect(cleanHintText('x'.repeat(100))).toHaveLength(64)
  })
  it('returns an empty string for anything that is not a string', () => {
    expect(cleanHintText(undefined)).toBe('')
    expect(cleanHintText(42)).toBe('')
  })
})

describe('displayHint', () => {
  it('is null where there is nothing to show, so the UI can say "unknown device"', () => {
    expect(displayHint(null)).toEqual({ name: null, platform: null })
    expect(displayHint({ name: '  \u{202e} ', kind: 'web', platform: '' })).toEqual({ name: null, platform: null })
    expect(displayHint({ name: 'iPhone', kind: 'web', platform: 'browser' })).toEqual({ name: 'iPhone', platform: 'browser' })
  })
})
