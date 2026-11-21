// 规则文字的本地校验和 daemon 同一套规则（docs/spec/daemon.md § 23.2）。
import { describe, expect, it } from 'vitest'
import { codePoints, newRuleId, normalizeRuleText, ruleProjectError, ruleTextError } from './validate'

describe('rule text', () => {
  it('needs 1 to 600 code points after trimming, counted as code points not UTF-16 units', () => {
    expect(ruleTextError('')).toBe('empty')
    expect(ruleTextError('  \n ')).toBe('empty')
    expect(ruleTextError('a'.repeat(600))).toBeNull()
    expect(ruleTextError('a'.repeat(601))).toBe('too_long')
    expect(ruleTextError('中'.repeat(600))).toBeNull()
    // an emoji is one code point but two UTF-16 units
    expect(ruleTextError('😀'.repeat(600))).toBeNull()
    expect(ruleTextError('😀'.repeat(601))).toBe('too_long')
    expect(codePoints('😀')).toBe(1)
    // surrounding whitespace does not count
    expect(ruleTextError(` ${'a'.repeat(600)} \n`)).toBeNull()
  })

  it('allows line breaks but no other control characters', () => {
    expect(ruleTextError('first\n\nsecond')).toBeNull()
    for (const bad of ['a\tb', 'a\u0000b', 'a\u001bb', 'a\u007fb', 'a\u0085b', `a${String.fromCodePoint(0x2028)}b`, `a${String.fromCodePoint(0x2029)}b`]) expect(ruleTextError(bad), JSON.stringify(bad)).toBe('control')
  })

  it('refuses invisible characters: bidirectional controls and zero-width characters', () => {
    for (const cp of [0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067, 0x2068, 0x2069, 0x200e, 0x200f, 0x061c, 0x200b, 0x200c, 0x200d, 0x2060, 0xfeff]) {
      expect(ruleTextError(`a${String.fromCodePoint(cp)}b`), cp.toString(16)).toBe('invisible')
    }
    expect(ruleTextError(`${String.fromCodePoint(0x200b)}leading`)).toBe('invisible')
    expect(ruleTextError('normal 中文 text, emoji 🙂 and “quotes”')).toBeNull()
  })

  it('normalises line endings and trims like the node does', () => {
    expect(normalizeRuleText('  a\r\nb  ')).toBe('a\nb')
  })
})

describe('project name', () => {
  it('is optional; when given it is one line of at most 200 code points', () => {
    expect(ruleProjectError(undefined)).toBeNull()
    expect(ruleProjectError('   ')).toBeNull()
    expect(ruleProjectError('web')).toBeNull()
    expect(ruleProjectError('a\nb')).toBe('control')
    expect(ruleProjectError('a'.repeat(201))).toBe('too_long')
  })
})

describe('ids', () => {
  it('are lowercase hyphenated UUIDs', () => {
    expect(newRuleId()).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/)
  })
})
