import { describe, expect, it } from 'vitest'
import { safeHttpUrl } from './url'

describe('safeHttpUrl', () => {
  it('accepts http and https and returns the normalized address', () => {
    expect(safeHttpUrl('https://example.com/mock-auth?x=1#frag')).toBe('https://example.com/mock-auth?x=1#frag')
    expect(safeHttpUrl('HTTP://Example.com')).toBe('http://example.com/')
  })
  it('rejects every other scheme and anything unparsable', () => {
    for (const bad of ['javascript:alert(1)', 'data:text/html,x', 'file:///etc/passwd', 'ftp://example.com', 'vbscript:x', 'blob:https://example.com/x', '//example.com', 'example.com', 'not a url', '']) {
      expect(safeHttpUrl(bad), bad).toBeNull()
    }
  })
})
