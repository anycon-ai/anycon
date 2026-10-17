import { decode, encode } from '@msgpack/msgpack'
import { describe, expect, it } from 'vitest'
import { decodeControl, decodeEnvelope, decodeRpc, encodeControl, encodeEnvelope, encodeRpc, makeEnvelope, makeNotification, makeRequest } from './wire'

describe('envelope codec (shared with @anycon/protocol)', () => {
  it('round-trips an envelope and keeps body as bin', () => {
    const env = makeEnvelope('a'.repeat(32), 'b'.repeat(32), 'data', 7, Uint8Array.of(1, 2, 3, 255))
    const bytes = encodeEnvelope(env)
    expect(decodeEnvelope(bytes)).toEqual(env)
    // 线上格式：字符串键的 map，body 是 msgpack bin（不是数组）
    const raw = decode(bytes) as Record<string, unknown>
    expect(Object.keys(raw)).toEqual(['v', 'from', 'to', 'kind', 'seq', 'body'])
    expect(raw.body).toBeInstanceOf(Uint8Array)
  })

  it('decodes an envelope produced by a plain msgpack encoder', () => {
    const bytes = encode({ v: 1, from: 'x', to: '', kind: 'control', seq: 1, body: new Uint8Array([9]) })
    expect(decodeEnvelope(bytes)).toMatchObject({ kind: 'control', seq: 1 })
  })

  it('rejects garbage and bad kinds', () => {
    expect(() => decodeEnvelope(Uint8Array.of(0xc1))).toThrow()
    expect(() => decodeEnvelope(encode({ v: 1, from: 'x', to: 'y', kind: 'nope', seq: 1, body: new Uint8Array() }))).toThrow()
    expect(() => decodeEnvelope(encode({ v: 1, from: 'x', to: 'y', kind: 'data', seq: 1, body: [1, 2] }))).toThrow()
  })

  it('encodes the auth control frame the way the hub expects (client info, no null token key)', () => {
    const bytes = encodeControl({ type: 'auth', device_token: null, protocol_version: 1, client: { name: 'Chrome · macOS · host', kind: 'web', platform: 'browser', version: '0.1.0', origin: 'http://localhost:5173' } })
    const raw = decode(bytes) as Record<string, unknown>
    expect(raw.type).toBe('auth')
    expect('device_token' in raw).toBe(false)
    expect(decodeControl(bytes)).toMatchObject({ type: 'auth', client: { kind: 'web', platform: 'browser' } })
  })

  it('round-trips pair / peer control frames', () => {
    for (const c of [
      { type: 'pair_join', match_id: '0123456789abcdef' },
      { type: 'pair_registered', expires_at_ms: 1_700_000_000_000 },
      { type: 'dropped', seq: 5, reason: 'too_large' },
      { type: 'pair_confirm', peer: 'p'.repeat(32) },
      { type: 'peer_online', device: 'd'.repeat(32) },
      { type: 'undeliverable', to: 'd'.repeat(32), seq: 9 },
    ] as const) {
      expect(decodeControl(encodeControl(c))).toEqual(c)
    }
  })
})

describe('JSON-RPC codec', () => {
  it('round-trips requests and notifications', () => {
    expect(decodeRpc(encodeRpc(makeRequest(3, 'session.send', { a: 1 })))).toEqual({ jsonrpc: '2.0', id: 3, method: 'session.send', params: { a: 1 } })
    expect(decodeRpc(encodeRpc(makeNotification('session.event', { x: 1 })))).toEqual({ jsonrpc: '2.0', method: 'session.event', params: { x: 1 } })
  })
})
