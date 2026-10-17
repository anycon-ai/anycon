import { afterEach, describe, expect, it, vi } from 'vitest'
import { sha256Hex, sha256Js } from './sha256'
import { toHex } from '@/lib/bytes'

const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

describe('sha256', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('the JavaScript implementation matches the standard test vectors', () => {
    expect(toHex(sha256Js(new Uint8Array(0)))).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855')
    expect(toHex(sha256Js(enc('abc')))).toBe('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')
    expect(toHex(sha256Js(enc('abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq')))).toBe('248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1')
  })

  it('the JavaScript implementation agrees with crypto.subtle on awkward lengths', async () => {
    const subtle = globalThis.crypto.subtle
    for (const len of [1, 55, 56, 57, 63, 64, 65, 119, 120, 1000, 70_001]) {
      const bytes = Uint8Array.from({ length: len }, (_, i) => (i * 31 + 7) % 256)
      const expected = toHex(new Uint8Array(await subtle.digest('SHA-256', bytes)))
      expect(toHex(sha256Js(bytes))).toBe(expected)
    }
  })

  it('sha256Hex uses crypto.subtle when it exists and falls back to JavaScript when it does not (insecure context)', async () => {
    const bytes = enc('abc')
    const expected = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'
    expect(await sha256Hex(bytes)).toBe(expected)
    const digest = vi.fn()
    vi.stubGlobal('crypto', { subtle: undefined, digest })
    expect(await sha256Hex(bytes)).toBe(expected)
    expect(digest).not.toHaveBeenCalled()
  })

  it('falls back when crypto.subtle.digest rejects', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    vi.stubGlobal('crypto', { subtle: { digest: () => Promise.reject(new Error('nope')) } })
    expect(await sha256Hex(enc('abc'))).toBe('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')
  })
})
