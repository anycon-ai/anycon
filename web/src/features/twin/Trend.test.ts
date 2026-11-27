import { describe, expect, it } from 'vitest'
import { niceMax } from './Trend'

describe('niceMax', () => {
  it('rounds the axis up so the middle tick is a whole number', () => {
    expect(niceMax(0)).toBe(2)
    expect(niceMax(1)).toBe(2)
    expect(niceMax(3)).toBe(4)
    expect(niceMax(6)).toBe(10)
    expect(niceMax(8)).toBe(10)
    expect(niceMax(15)).toBe(20)
    expect(niceMax(23)).toBe(40)
    expect(niceMax(120)).toBe(200)
  })
  it('never ends below the largest value', () => {
    for (let m = 1; m <= 500; m++) {
      const top = niceMax(m)
      expect(top).toBeGreaterThanOrEqual(m)
      expect((top / 2) % 1).toBe(0)
    }
  })
})
