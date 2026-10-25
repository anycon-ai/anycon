import { afterEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { ApiError, apiFetch, apiFetchRaw, setUnauthorizedHandler } from './client'

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

describe('apiFetch — bearer (device token)', () => {
  it('sends Authorization: Bearer and X-Anycon, and no cookie', async () => {
    const api = installFakeApi({ 'POST /api/preview/tokens': () => ({ token: 't', origin: 'https://x', expires_at_ms: 1 }) })
    await apiFetch('/api/preview/tokens', { method: 'POST', body: { node: 'n', label: 'l' }, bearer: 'dev-token' })
    const call = api.callsTo('POST /api/preview/tokens')[0]!
    expect(call.headers.Authorization).toBe('Bearer dev-token')
    expect(call.headers['X-Anycon']).toBe('1')
    expect((fetch as unknown as { mock: { calls: [string, RequestInit][] } }).mock.calls[0]![1].credentials).toBe('omit')
  })

  it('without bearer the cookie is used and no Authorization header is sent', async () => {
    const api = installFakeApi({ 'GET /api/me': () => ({ id: 'u' }) })
    await apiFetch('/api/me')
    expect(api.calls[0]!.headers.Authorization).toBeUndefined()
    expect((fetch as unknown as { mock: { calls: [string, RequestInit][] } }).mock.calls[0]![1].credentials).toBe('same-origin')
  })

  it('a 401 on a bearer request is about the device, not the login: it does not redirect to the login page', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    installFakeApi({ 'POST /api/preview/tokens': () => ({ status: 401, body: errorBody('unauthorized') }) })
    await expect(apiFetch('/api/preview/tokens', { method: 'POST', body: {}, bearer: 'dev-token' })).rejects.toMatchObject({ status: 401, code: 'unauthorized' })
    expect(handler).not.toHaveBeenCalled()
  })
})

describe('apiFetchRaw', () => {
  it('returns the body together with the response headers (X-Next-Cursor paging)', async () => {
    installFakeApi({ 'GET /api/admin/accounts': () => ({ body: [{ id: 'a' }], headers: { 'X-Next-Cursor': '123.abc' } }) })
    const { data, headers } = await apiFetchRaw<Array<{ id: string }>>('/api/admin/accounts')
    expect(data).toEqual([{ id: 'a' }])
    expect(headers.get('X-Next-Cursor')).toBe('123.abc')
  })

  it('has no cursor header on the last page, and a 204 has no body', async () => {
    installFakeApi({ 'GET /api/x': () => [], 'DELETE /api/y': () => undefined })
    expect((await apiFetchRaw('/api/x')).headers.get('X-Next-Cursor')).toBeNull()
    expect((await apiFetchRaw('/api/y', { method: 'DELETE' })).data).toBeUndefined()
  })
})
