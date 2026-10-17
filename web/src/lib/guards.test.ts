import { describe, expect, it } from 'vitest'
import { isRecord, nonEmptyString } from './guards'

describe('guards', () => {
  it('isRecord accepts plain objects only', () => {
    expect(isRecord({})).toBe(true)
    expect(isRecord({ a: 1 })).toBe(true)
    for (const v of [null, undefined, [], [1], 'x', 1, true]) expect(isRecord(v)).toBe(false)
  })
  it('nonEmptyString', () => {
    expect(nonEmptyString('a')).toBe('a')
    for (const v of ['', null, undefined, 1, {}]) expect(nonEmptyString(v)).toBeUndefined()
  })
})
