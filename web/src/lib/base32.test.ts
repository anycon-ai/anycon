import { describe, expect, it } from 'vitest'
import { base32Decode, base32Encode } from './base32'

const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

describe('base32 (RFC 4648, lowercase, no padding)', () => {
  // RFC 4648 § 10 的测试向量，去掉填充、转小写
  it.each([
    ['', ''],
    ['f', 'my'],
    ['fo', 'mzxq'],
    ['foo', 'mzxw6'],
    ['foob', 'mzxw6yq'],
    ['fooba', 'mzxw6ytb'],
    ['foobar', 'mzxw6ytboi'],
  ])('encodes %j as %j and decodes it back', (plain, text) => {
    expect(base32Encode(enc(plain))).toBe(text)
    expect(Array.from(base32Decode(text)!)).toEqual(Array.from(enc(plain)))
  })

  it('32 secret bytes become 52 characters from the lowercase alphabet', () => {
    const bytes = Uint8Array.from({ length: 32 }, (_, i) => (i * 37 + 11) & 0xff)
    const text = base32Encode(bytes)
    expect(text).toHaveLength(52)
    expect(text).toMatch(/^[a-z2-7]{52}$/)
    expect(Array.from(base32Decode(text)!)).toEqual(Array.from(bytes))
  })

  it('rejects anything but the canonical lowercase form', () => {
    expect(base32Decode('MZXW6YTBOI')).toBeNull() // 大写
    expect(base32Decode('mzxw6ytboi======')).toBeNull() // 带填充
    expect(base32Decode('mzxw 6ytb')).toBeNull() // 空白
    expect(base32Decode('mzxw1ytb')).toBeNull() // 1 不在字母表里
    expect(base32Decode('m')).toBeNull() // 长度不可能
    expect(base32Decode('mz')).toBeNull() // 末尾的位不是 0（f 编码成 my）
  })
})
