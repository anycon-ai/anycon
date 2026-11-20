import { describe, expect, it } from 'vitest'
import { bodyError, cleanOneLine, codePoints, draftError, newTodoId, parseCapture, projectError, titleError, UUID_RE, utf8Bytes } from './validate'

const LS = String.fromCharCode(0x2028)
const PS = String.fromCharCode(0x2029)
const RLO = String.fromCharCode(0x202e)

describe('title', () => {
  it('is 1–200 Unicode code points after trimming, counted in code points not UTF-16 units', () => {
    expect(titleError('x')).toBeNull()
    expect(titleError('   ')).toEqual({ field: 'title', reason: 'empty' })
    expect(titleError('a'.repeat(200))).toBeNull()
    expect(titleError('a'.repeat(201))).toEqual({ field: 'title', reason: 'too_long' })
    // 200 个 emoji：UTF-16 里是 400 个码元，但只有 200 个码点，合法
    const emoji = String.fromCodePoint(0x1f600)
    expect(emoji.length).toBe(2)
    expect(titleError(emoji.repeat(200))).toBeNull()
    expect(titleError(emoji.repeat(201))).toEqual({ field: 'title', reason: 'too_long' })
    expect(codePoints(emoji.repeat(3))).toBe(3)
  })

  it('has no control characters or line / paragraph separators', () => {
    for (const bad of ['a\nb', 'a\tb', 'a\u0000b', `a${LS}b`, `a${PS}b`]) expect(titleError(bad)).toEqual({ field: 'title', reason: 'control' })
    // 首尾的空白先去掉
    expect(titleError('  ok \n')).toBeNull()
  })
})

describe('body', () => {
  it('is at most 16 KiB of UTF-8, not 16384 characters', () => {
    expect(bodyError('a'.repeat(16 * 1024))).toBeNull()
    expect(bodyError('a'.repeat(16 * 1024 + 1))).toEqual({ field: 'body', reason: 'too_long' })
    // 汉字 3 字节：5462 个 = 16386 字节，超了；5461 个 = 16383 字节
    expect(bodyError('汉'.repeat(5461))).toBeNull()
    expect(bodyError('汉'.repeat(5462))).toEqual({ field: 'body', reason: 'too_long' })
    expect(utf8Bytes('汉')).toBe(3)
    expect(bodyError('')).toBeNull()
    expect(bodyError('line\nbreaks\tare fine')).toBeNull()
  })
})

describe('project', () => {
  it('blank means not given; given means a valid single line', () => {
    expect(projectError(undefined)).toBeNull()
    expect(projectError('   ')).toBeNull()
    expect(projectError('api')).toBeNull()
    expect(projectError('a\nb')).toEqual({ field: 'project', reason: 'control' })
    expect(projectError('p'.repeat(201))).toEqual({ field: 'project', reason: 'too_long' })
  })
})

describe('draft', () => {
  it('reports the first problem, due times must be non-negative integers', () => {
    expect(draftError({ title: 'ok', body: '' })).toBeNull()
    expect(draftError({ title: '', body: '' })?.field).toBe('title')
    expect(draftError({ title: 'ok', body: 'x'.repeat(20000) })?.field).toBe('body')
    expect(draftError({ title: 'ok', body: '', due_ms: -1 })?.field).toBe('due_ms')
    expect(draftError({ title: 'ok', body: '', due_ms: 1.5 })?.field).toBe('due_ms')
    expect(draftError({ title: 'ok', body: '', due_ms: 1_700_000_000_000 })).toBeNull()
  })
})

describe('ids', () => {
  it('are lowercase hyphenated UUIDs, and nothing else matches', () => {
    for (let i = 0; i < 20; i++) expect(newTodoId()).toMatch(UUID_RE)
    const good = newTodoId()
    for (const bad of [good.toUpperCase(), `{${good}}`, good.replaceAll('-', ''), `urn:uuid:${good}`]) expect(UUID_RE.test(bad)).toBe(false)
  })
})

describe('parseCapture', () => {
  it('first line is the title, the rest the body', () => {
    expect(parseCapture('buy milk')).toEqual({ title: 'buy milk', body: '' })
    expect(parseCapture('  buy milk  \nfrom the shop\n\nwhole milk')).toEqual({ title: 'buy milk', body: 'from the shop\n\nwhole milk' })
    expect(parseCapture('title\n\n\nbody')).toEqual({ title: 'title', body: 'body' })
    expect(parseCapture('a\r\nb')).toEqual({ title: 'a', body: 'b' })
    expect(parseCapture('')).toEqual({ title: '', body: '' })
  })
})

describe('cleanOneLine (untrusted agent text)', () => {
  it('strips control and bidi characters, collapses whitespace and truncates by code points', () => {
    expect(cleanOneLine(`a${RLO}b\nc\td`)).toBe('a b c d')
    expect(cleanOneLine(null)).toBe('')
    const emoji = String.fromCodePoint(0x1f600)
    expect(cleanOneLine(emoji.repeat(10), 5)).toBe(`${emoji.repeat(5)}…`)
  })
})
