import { afterEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { deletePushSubscription, parseTestResult, sendTestPush } from './notifications'
import { resendAfterSecs } from './queries'

afterEach(() => vi.unstubAllGlobals())

describe('parseTestResult', () => {
  it('reads ok / gone / error from a bare string or an object', () => {
    for (const r of ['ok', 'gone', 'error'] as const) {
      expect(parseTestResult(r)).toEqual({ result: r })
      expect(parseTestResult({ result: r })).toEqual({ result: r })
      expect(parseTestResult({ status: r })).toEqual({ result: r })
      expect(parseTestResult({ outcome: r })).toEqual({ result: r })
    }
  })
  it('keeps the HTTP status the push service answered with', () => {
    expect(parseTestResult({ result: 'error', status: 403 })).toEqual({ result: 'error', status: 403 })
    expect(parseTestResult({ result: 'error', status: 503 })).toEqual({ result: 'error', status: 503 })
    expect(parseTestResult({ result: 'error' })).toEqual({ result: 'error' }) // 连不上推送服务：没有状态码
    // 不是合理的 HTTP 状态码就丢掉；ok / gone 不带状态码
    for (const status of [0, 99, 600, 40.5, '403', null, Number.NaN]) expect(parseTestResult({ result: 'error', status })).toEqual({ result: 'error' })
    expect(parseTestResult({ result: 'gone', status: 410 })).toEqual({ result: 'gone' })
    expect(parseTestResult({ result: 'ok', status: 201 })).toEqual({ result: 'ok' })
  })
  it('an empty success is ok; anything unrecognised is an error', () => {
    expect(parseTestResult(undefined)).toEqual({ result: 'ok' })
    expect(parseTestResult(null)).toEqual({ result: 'ok' })
    expect(parseTestResult('')).toEqual({ result: 'ok' })
    expect(parseTestResult({ result: 'weird' })).toEqual({ result: 'error' })
    expect(parseTestResult(42)).toEqual({ result: 'error' })
  })
})

describe('push API calls', () => {
  it('sendTestPush posts the device id', async () => {
    const api = installFakeApi({ 'POST /api/push/test': () => ({ result: 'gone' }) })
    expect(await sendTestPush('dev1')).toEqual({ result: 'gone' })
    expect(api.callsTo('POST /api/push/test')[0]!.body).toEqual({ device_id: 'dev1' })
  })

  it('deleting a subscription the hub no longer has is fine; other errors are not', async () => {
    installFakeApi({ 'DELETE /api/push/subscriptions/gone': () => ({ status: 404, body: errorBody('not_found') }), 'DELETE /api/push/subscriptions/broken': () => ({ status: 500, body: errorBody('internal') }) })
    await expect(deletePushSubscription('gone')).resolves.toBeUndefined()
    await expect(deletePushSubscription('broken')).rejects.toMatchObject({ status: 500 })
  })

  it('encodes the id into the path', async () => {
    const api = installFakeApi({ 'DELETE /api/push/subscriptions/*': () => undefined })
    await deletePushSubscription('a/b')
    expect(api.calls[0]!.path).toBe('/api/push/subscriptions/a%2Fb')
  })
})

describe('resendAfterSecs', () => {
  it('reads the hub’s resend_after_secs; anything odd falls back to the documented 60', () => {
    expect(resendAfterSecs({ ok: true, resend_after_secs: 45 })).toBe(45)
    expect(resendAfterSecs({ resend_after_secs: 12.2 })).toBe(13)
    for (const raw of [undefined, null, {}, 'x', { resend_after_secs: 0 }, { resend_after_secs: -3 }, { resend_after_secs: 'soon' }, { resend_after_secs: Number.NaN }, { resend_after_secs: 99_999 }]) expect(resendAfterSecs(raw)).toBe(60)
  })
})
