import { describe, expect, it } from 'vitest'
import { base32Encode } from '@/lib/base32'
import { buildEnrollQr, ENROLL_QR_PREFIX, parseEnrollQr } from './enrollQr'

const NODE = 'd'.repeat(32)
const ENROLL = '0123456789abcdef0123456789abcdef'
const SECRET = Uint8Array.from({ length: 32 }, (_, i) => i * 7 + 1)

describe('enroll QR content', () => {
  it('is anycon-enroll:1:<node_id>:<enroll_id>:<base32(r)> with the lowercase unpadded alphabet, and is not a URL', () => {
    const text = buildEnrollQr(NODE, ENROLL, SECRET)
    expect(text).toBe(`anycon-enroll:1:${NODE}:${ENROLL}:${base32Encode(SECRET)}`)
    expect(text.split(':')).toHaveLength(5)
    expect(text.split(':')[4]).toMatch(/^[a-z2-7]{52}$/)
    expect(text).not.toContain('=')
    expect(/^https?:/.test(text)).toBe(false) // 不是链接：系统相机扫到只是一串文字
  })

  it('round-trips through the parser', () => {
    const parsed = parseEnrollQr(buildEnrollQr(NODE, ENROLL, SECRET))!
    expect(parsed.nodeId).toBe(NODE)
    expect(parsed.enrollId).toBe(ENROLL)
    expect(Array.from(parsed.secret)).toEqual(Array.from(SECRET))
  })

  it('refuses anything that is not an enroll code', () => {
    const good = buildEnrollQr(NODE, ENROLL, SECRET)
    expect(parseEnrollQr('https://hub.example/pair#code=ABCD-2345')).toBeNull()
    expect(parseEnrollQr('hello')).toBeNull()
    expect(parseEnrollQr(good.replace('anycon-enroll:1:', 'anycon-enroll:2:'))).toBeNull() // 版本不对
    expect(parseEnrollQr(good.replace(ENROLL_QR_PREFIX, 'ANYCON-ENROLL:1:'))).toBeNull()
    expect(parseEnrollQr(`${good}:extra`)).toBeNull() // 多一段
    expect(parseEnrollQr(`${ENROLL_QR_PREFIX}${NODE}:${ENROLL}`)).toBeNull() // 少一段
    expect(parseEnrollQr(`${ENROLL_QR_PREFIX}${NODE}:${ENROLL}:${base32Encode(SECRET.slice(0, 16))}`)).toBeNull() // 密钥不是 32 字节
    expect(parseEnrollQr(`${ENROLL_QR_PREFIX}${NODE}:${ENROLL}:${base32Encode(SECRET).toUpperCase()}`)).toBeNull() // 大写
    expect(parseEnrollQr(`${ENROLL_QR_PREFIX}${NODE}:bad id:${base32Encode(SECRET)}`)).toBeNull()
    expect(parseEnrollQr(` ${good}`)).toBeNull() // 不替扫到的内容修剪空白
  })
})
