import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { clearPendingPairCode, loginRedirectTarget, PENDING_CODE_TTL_MS, peekPendingPairCode, stashPairCode } from './loginRedirect'

beforeEach(() => sessionStorage.clear())
afterEach(() => {
  vi.useRealTimers()
  sessionStorage.clear()
})

describe('loginRedirectTarget', () => {
  it('never returns a hash, and stashes the #code= of a pairing link', () => {
    expect(loginRedirectTarget({ pathname: '/pair', searchStr: '', hash: '#code=ABCD-2345' })).toBe('/pair')
    expect(peekPendingPairCode('/pair')).toBe('ABCD-2345')
  })

  it('moves a legacy ?code= out of the query too, but keeps other search params', () => {
    expect(loginRedirectTarget({ pathname: '/setup', searchStr: '?code=ABCD-2345&x=1', hash: '' })).toBe('/setup?x=1')
    expect(peekPendingPairCode('/setup')).toBe('ABCD-2345')
  })

  it('drops the hash of other pages and does not touch code= params there', () => {
    expect(loginRedirectTarget({ pathname: '/n/abc/s/1', searchStr: '?code=zzz', hash: '#top' })).toBe('/n/abc/s/1?code=zzz')
    expect(sessionStorage.length).toBe(0)
  })
})

describe('pending pairing code', () => {
  it('is only handed to the route it was stashed for', () => {
    stashPairCode('/pair', 'ABCD-2345')
    expect(peekPendingPairCode('/setup')).toBe('')
    expect(peekPendingPairCode('/pair')).toBe('ABCD-2345')
    clearPendingPairCode()
    expect(peekPendingPairCode('/pair')).toBe('')
  })

  it('expires', () => {
    vi.useFakeTimers({ toFake: ['Date'] })
    stashPairCode('/pair', 'ABCD-2345')
    vi.setSystemTime(Date.now() + PENDING_CODE_TTL_MS + 1)
    expect(peekPendingPairCode('/pair')).toBe('')
  })

  it('ignores garbage in storage', () => {
    sessionStorage.setItem('anycon.pending_pair_code', '{not json')
    expect(peekPendingPairCode('/pair')).toBe('')
    sessionStorage.setItem('anycon.pending_pair_code', JSON.stringify({ to: '/pair', code: 5, at: 'x' }))
    expect(peekPendingPairCode('/pair')).toBe('')
  })
})
