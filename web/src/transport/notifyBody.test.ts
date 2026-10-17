import { encode } from '@msgpack/msgpack'
import { describe, expect, it } from 'vitest'
import { decodeNotifyBody } from './notifyBody'

describe('decodeNotifyBody', () => {
  it('reads category and session id (the shape crates/protocol serializes)', () => {
    expect(decodeNotifyBody(encode({ category: 'permission_requested', session_id: 'sess_abc', detail: null }))).toEqual({ category: 'permission_requested', session_id: 'sess_abc' })
  })
  it('the enrollment categories have no session', () => {
    expect(decodeNotifyBody(encode({ category: 'enroll_requested', session_id: null, detail: null }))).toEqual({ category: 'enroll_requested', session_id: null })
    expect(decodeNotifyBody(encode({ category: 'security' }))).toEqual({ category: 'security', session_id: null })
  })
  it('does not touch the encrypted detail', () => {
    expect(decodeNotifyBody(encode({ category: 'error', session_id: 's', detail: 'AAAA' }))).toEqual({ category: 'error', session_id: 's' })
  })
  it.each([[encode('x')], [encode(null)], [encode([1, 2])], [encode({ category: 'nope' })], [encode({ category: 3 })], [encode({})], [new Uint8Array([0xc1])], [new Uint8Array(0)]])('unreadable body → null (%#)', (bytes) => {
    expect(decodeNotifyBody(bytes)).toBeNull()
  })
})
