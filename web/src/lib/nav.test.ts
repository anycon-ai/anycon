import { describe, expect, it } from 'vitest'
import { safeRedirect } from './nav'

describe('safeRedirect', () => {
  it('keeps same-origin absolute paths, including search and the pairing link fragment', () => {
    expect(safeRedirect('/pair#code=7K3P-Q2MX')).toBe('/pair#code=7K3P-Q2MX')
    expect(safeRedirect('/n/abcd/s/s1?x=1')).toBe('/n/abcd/s/s1?x=1')
    expect(safeRedirect('/')).toBe('/')
  })
  it('falls back to the home page for anything that could leave the app', () => {
    for (const bad of [undefined, '', '//evil.example/x', '/\\evil.example', 'https://evil.example', 'javascript:alert(1)', 'pair']) expect(safeRedirect(bad)).toBe('/')
  })
})
