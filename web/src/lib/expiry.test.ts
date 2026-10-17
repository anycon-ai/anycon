import { describe, expect, it } from 'vitest'
import { ENROLL_WINDOW_MS, formatCountdown, localExpiry } from './expiry'

describe('localExpiry', () => {
  const now = 1_700_000_000_000
  it('trusts a timestamp whose remaining time is plausible', () => {
    expect(localExpiry(now + 200_000, now)).toBe(now + 200_000)
  })
  it('falls back to the whole window from now when the peer clock is far off', () => {
    expect(localExpiry(now - 10 * 60_000, now)).toBe(now + ENROLL_WINDOW_MS) // 对方的时钟慢了 / 本机快了
    expect(localExpiry(now + 3 * 3600_000, now)).toBe(now + ENROLL_WINDOW_MS) // 对方的时钟快了
    expect(localExpiry(null, now)).toBe(now + ENROLL_WINDOW_MS)
  })
})

describe('formatCountdown', () => {
  it('formats m:ss and never goes negative', () => {
    expect(formatCountdown(300_000)).toBe('5:00')
    expect(formatCountdown(61_001)).toBe('1:02')
    expect(formatCountdown(-5)).toBe('0:00')
  })
})
