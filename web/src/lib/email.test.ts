import { describe, expect, it } from 'vitest'
import { isPlausibleEmail } from './email'

describe('isPlausibleEmail', () => {
  it('accepts ordinary addresses', () => {
    for (const a of ['a@b.co', 'first.last+tag@example.com', 'x@sub.domain.example.org', 'USER@Example.COM']) expect(isPlausibleEmail(a)).toBe(true)
  })
  it('rejects what is obviously not an address', () => {
    for (const a of ['', 'plain', 'a@', '@b.co', 'a@b', 'a@b.', 'a@.co', 'a b@c.de', 'a@@b.co', 'a@b..co', `${'x'.repeat(250)}@b.co`]) expect(isPlausibleEmail(a)).toBe(false)
  })
})
