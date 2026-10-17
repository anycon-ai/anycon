import { describe, expect, it } from 'vitest'
import { base64ToBytes, base64UrlToBytes, bytesToBase64, bytesToBase64Url } from './base64'

describe('base64', () => {
  it('round-trips every byte value, including lengths that need padding', () => {
    for (const len of [0, 1, 2, 3, 255, 256, 0x2000, 0x2001, 20_000]) {
      const bytes = Uint8Array.from({ length: len }, (_, i) => (i * 7 + 3) % 256)
      const b64 = bytesToBase64(bytes)
      expect(base64ToBytes(b64)).toEqual(bytes)
      expect(b64.length % 4).toBe(0)
    }
  })

  it('matches the standard alphabet and padding', () => {
    expect(bytesToBase64(new TextEncoder().encode('Man'))).toBe('TWFu')
    expect(bytesToBase64(new TextEncoder().encode('Ma'))).toBe('TWE=')
    expect(bytesToBase64(new TextEncoder().encode('M'))).toBe('TQ==')
    expect(bytesToBase64(Uint8Array.from([0xfb, 0xff]))).toBe('+/8=')
  })

  it('rejects text that is not base64', () => {
    expect(() => base64ToBytes('not base64!')).toThrow()
  })
})

describe('base64url', () => {
  it('uses - and _ instead of + and /, and drops the padding', () => {
    expect(bytesToBase64Url(Uint8Array.from([0xfb, 0xff]))).toBe('-_8')
    expect(base64UrlToBytes('-_8')).toEqual(Uint8Array.from([0xfb, 0xff]))
    expect(bytesToBase64Url(new TextEncoder().encode('M'))).toBe('TQ')
  })

  it('round-trips a 65-byte uncompressed P-256 point (a VAPID public key)', () => {
    const key = Uint8Array.from({ length: 65 }, (_, i) => (i * 37 + 4) % 256)
    key[0] = 4
    const text = bytesToBase64Url(key)
    expect(text).toMatch(/^[A-Za-z0-9_-]+$/)
    expect(base64UrlToBytes(text)).toEqual(key)
  })

  it('rejects text that is not base64url', () => {
    expect(() => base64UrlToBytes('not base64url!')).toThrow()
  })
})
