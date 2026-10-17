import { describe, expect, it } from 'vitest'
import { extractPairCode, formatPairCodeInput, isCompletePairCode, normalizePairCode, pairCodeFromLocation } from './code'

describe('pair code input', () => {
  it('uppercases, drops dashes and spaces, and inserts a dash after every 4 characters', () => {
    expect(formatPairCodeInput('7k3p')).toBe('7K3P')
    expect(formatPairCodeInput('7k3pq')).toBe('7K3P-Q')
    expect(formatPairCodeInput(' 7k3p - q2mx ')).toBe('7K3P-Q2MX')
    expect(formatPairCodeInput('7k3pq2mxe')).toBe('7K3P-Q2MX-E')
    expect(formatPairCodeInput('7k3pq2mxabcdefghjkEXTRA')).toBe('7K3P-Q2MX-ABCD-EFGH')
  })
  it('ignores dashes and whitespace when pasted', () => {
    expect(normalizePairCode('7k3p-q2mx\n')).toBe('7K3PQ2MX')
  })
  it('accepts 8 or 16 characters from the unambiguous alphabet', () => {
    expect(isCompletePairCode('7K3P-Q2MX')).toBe(true)
    expect(isCompletePairCode('7K3P-Q2MX-ABCD-EFGH')).toBe(true)
    expect(isCompletePairCode('7K3P-Q2M')).toBe(false)
    expect(isCompletePairCode('7K3P-Q2MX-ABCD')).toBe(false) // 12 位不合法
    expect(isCompletePairCode('7K3P-Q2M0')).toBe(false) // 0 不在字母表里
    expect(isCompletePairCode('7K3P-Q2MI')).toBe(false)
    expect(isCompletePairCode('7K3P-Q2MX-ABCD-EFG1')).toBe(false)
  })
  it('extracts the code from a pairing link (fragment first, legacy query too) or raw text', () => {
    expect(extractPairCode('https://hub.example.com/pair#code=7K3P-Q2MX')).toBe('7K3P-Q2MX')
    expect(extractPairCode('https://hub.example.com/pair#code=7k3pq2mxabcdefgh')).toBe('7K3P-Q2MX-ABCD-EFGH')
    expect(extractPairCode('https://hub.example.com/pair?code=7K3P-Q2MX')).toBe('7K3P-Q2MX')
    expect(extractPairCode('#code=7K3P-Q2MX')).toBe('7K3P-Q2MX')
    expect(extractPairCode('7k3pq2mx')).toBe('7K3P-Q2MX')
  })
  it('does not throw on an invalid percent sequence pasted into the box (decodeURIComponent would)', () => {
    expect(() => extractPairCode('#code=7K3P%')).not.toThrow()
    expect(extractPairCode('#code=7K3P%')).toBe('7K3P-%') // 非法的 % 退回原文；字母表之外的字符由 isCompletePairCode 拦下
    expect(() => extractPairCode('code=%E4%BD%')).not.toThrow()
    expect(() => extractPairCode('https://hub.example.com/pair#code=%zz7K3P')).not.toThrow()
    expect(extractPairCode('code=7K%33P-Q2MX')).toBe('7K3P-Q2MX') // 合法的百分号编码照常解码
  })
  it('reads the code from location.hash, falling back to the legacy query', () => {
    expect(pairCodeFromLocation({ hash: '#code=7k3p-q2mx', search: '' })).toBe('7K3P-Q2MX')
    expect(pairCodeFromLocation({ hash: '', search: '?code=7k3pq2mx' })).toBe('7K3P-Q2MX')
    expect(pairCodeFromLocation({ hash: '#other=1', search: '' })).toBe('')
  })
})
