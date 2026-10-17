import { describe, expect, it } from 'vitest'
import { EnrollError } from '@/transport/enroll'
import { isExpiry, isRestartable, restartsByItself, unlockErrorKey, unlockErrorKeys, UNLOCK_ERROR_CODES } from './errors'

describe('unlockErrorKey', () => {
  it('wrong_secret means different things per entry', () => {
    const e = (left?: number) => new EnrollError('wrong_secret', undefined, left === undefined ? {} : { attemptsLeft: left })
    expect(unlockErrorKey(e(), 'password')).toBe('unlock.errors.wrongPassword')
    expect(unlockErrorKey(e(2), 'code')).toBe('unlock.errors.wrongCode')
    expect(unlockErrorKey(e(0), 'code')).toBe('unlock.errors.codeExhausted')
    expect(unlockErrorKey(e(0), 'approve')).toBe('unlock.errors.codeExhausted')
    expect(unlockErrorKey(e(), 'scan')).toBe('unlock.errors.wrongScan')
  })

  it('every other code has its own text', () => {
    expect(unlockErrorKey(new EnrollError('locked'), 'password')).toBe('unlock.errors.locked')
    expect(unlockErrorKey(new EnrollError('no_code'), 'code')).toBe('unlock.errors.no_code')
    expect(unlockErrorKeys()).toContain('unlock.errors.offline')
    expect(unlockErrorKeys()).not.toContain('unlock.errors.wrong_secret')
    expect(Object.keys(UNLOCK_ERROR_CODES).length).toBeGreaterThanOrEqual(24)
  })

  it('timeouts and expiry restart a QR by themselves; denied / peer_gone are restartable by hand', () => {
    expect(isExpiry('timeout')).toBe(true)
    expect(isExpiry('expired')).toBe(true)
    expect(isExpiry('denied')).toBe(false)
    expect(isRestartable('denied')).toBe(true)
    expect(isRestartable('timeout')).toBe(true)
    expect(isRestartable('wrong_secret')).toBe(false)
    expect(isRestartable('locked')).toBe(false)
  })
})

describe('expired', () => {
  it('talks about the code for the code entries and about the request for the others', () => {
    const e = new EnrollError('expired')
    expect(unlockErrorKey(e, 'code')).toBe('unlock.errors.codeExpired')
    expect(unlockErrorKey(e, 'approve')).toBe('unlock.errors.codeExpired')
    expect(unlockErrorKey(e, 'scan')).toBe('unlock.errors.expired')
    expect(unlockErrorKey(e, 'password')).toBe('unlock.errors.expired')
  })
})

describe('restartsByItself', () => {
  it('a QR that ran out, or a hub connection that dropped, is replaced silently; a real refusal is not', () => {
    expect(restartsByItself('timeout')).toBe(true)
    expect(restartsByItself('expired')).toBe(true)
    expect(restartsByItself('hub_unavailable')).toBe(true)
    for (const c of ['denied', 'wrong_secret', 'method_disabled', 'forbidden', 'not_found', 'quota', 'rate_limited', 'busy', 'offline'] as const) expect(restartsByItself(c)).toBe(false)
  })

  it('no answer to enroll_begin is restartable by hand, never by itself (a retry would hit the rate limit again), and has its own text', () => {
    expect(restartsByItself('hub_no_reply')).toBe(false)
    expect(isExpiry('hub_no_reply')).toBe(false)
    expect(isRestartable('hub_no_reply')).toBe(true)
    expect(unlockErrorKey(new EnrollError('hub_no_reply'), 'scan')).toBe('unlock.errors.hub_no_reply')
    expect(unlockErrorKeys()).toContain('unlock.errors.hub_no_reply')
  })
})
