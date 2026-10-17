import { afterEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { ApiError, apiFetch, setUnauthorizedHandler } from './client'

afterEach(() => {
  setUnauthorizedHandler(null)
  vi.unstubAllGlobals()
})

describe('apiFetch — 401 handling', () => {
  it('a 401 from an authenticated call sends the user to the login page', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    installFakeApi({ 'GET /api/me': () => ({ status: 401, body: errorBody('unauthorized') }) })
    await expect(apiFetch('/api/me')).rejects.toMatchObject({ status: 401, code: 'unauthorized' })
    expect(handler).toHaveBeenCalledTimes(1)
  })

  it('401 invalid_code (a wrong email code) is an answer to the user, not a signed-out session', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    installFakeApi({ 'POST /api/me/email/verify': () => ({ status: 401, body: errorBody('invalid_code') }) })
    const err = await apiFetch('/api/me/email/verify', { method: 'POST', body: { email: 'a@b.co', code: '000000' } }).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err).toMatchObject({ status: 401, code: 'invalid_code' })
    expect(handler).not.toHaveBeenCalled()
  })

  it('noRedirect never triggers it (login requests)', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    installFakeApi({ 'POST /auth/token': () => ({ status: 401, body: errorBody('invalid_token') }) })
    await expect(apiFetch('/auth/token', { method: 'POST', body: {}, noRedirect: true })).rejects.toMatchObject({ code: 'invalid_token' })
    expect(handler).not.toHaveBeenCalled()
  })

  it('reads Retry-After on a 429', async () => {
    installFakeApi({ 'POST /auth/email/send': () => ({ status: 429, body: errorBody('rate_limited'), headers: { 'Retry-After': '42' } }) })
    await expect(apiFetch('/auth/email/send', { method: 'POST', body: {} })).rejects.toMatchObject({ status: 429, code: 'rate_limited', retryAfter: 42 })
  })
})
