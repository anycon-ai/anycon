import { describe, expect, it } from 'vitest'
import { MIN_PASSWORD_CHARS, passwordLongEnough } from './password'

describe('passwordLongEnough', () => {
  it('needs at least 8 characters, counted after NFC like the daemon does', () => {
    expect(MIN_PASSWORD_CHARS).toBe(8)
    expect(passwordLongEnough('1234567')).toBe(false)
    expect(passwordLongEnough('12345678')).toBe(true)
    // 分解形式的 é（e + 组合重音）NFC 之后是 1 个字符：原样数有 8 个码点，规范化之后只有 7 个字符
    expect([...'cafe\u{301}caf']).toHaveLength(8)
    expect(passwordLongEnough('cafe\u{301}caf')).toBe(false)
    expect(passwordLongEnough('cafe\u{301}cafe')).toBe(true)
    expect(passwordLongEnough('密码密码密码密码')).toBe(true) // 8 个字符（不是字节）
    expect(passwordLongEnough('😀'.repeat(7))).toBe(false)
    expect(passwordLongEnough('😀'.repeat(8))).toBe(true)
  })
  it('does not apply complexity rules', () => {
    expect(passwordLongEnough('aaaaaaaa')).toBe(true)
  })
})
