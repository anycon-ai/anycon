import { describe, expect, it } from 'vitest'
import { TOKEN_MAX_AGE_MS } from '@/lib/constants'
import { tokenRotationDue } from './tokenRotation'

const NOW = 1_800_000_000_000

describe('tokenRotationDue', () => {
  it('is due when the issue time is unknown (records from before rotation existed rotate once)', () => {
    expect(tokenRotationDue(undefined, NOW)).toBe(true)
    expect(tokenRotationDue(Number.NaN, NOW)).toBe(true)
  })

  it('is due exactly at 30 days, not before', () => {
    expect(TOKEN_MAX_AGE_MS).toBe(30 * 24 * 3600 * 1000)
    expect(tokenRotationDue(NOW - TOKEN_MAX_AGE_MS + 1, NOW)).toBe(false)
    expect(tokenRotationDue(NOW - TOKEN_MAX_AGE_MS, NOW)).toBe(true)
    expect(tokenRotationDue(NOW - 1000 * TOKEN_MAX_AGE_MS, NOW)).toBe(true)
  })

  it('is not due for a token issued just now', () => {
    expect(tokenRotationDue(NOW, NOW)).toBe(false)
    expect(tokenRotationDue(NOW - 60_000, NOW)).toBe(false)
  })

  it('treats an issue time in the future (the clock went backwards) as unknown', () => {
    expect(tokenRotationDue(NOW + 1, NOW)).toBe(true)
  })
})
