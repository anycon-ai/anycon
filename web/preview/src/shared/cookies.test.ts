import { describe, expect, it } from 'vitest'
import { CookieJar, MAX_COOKIES, SESSION_COOKIE_TTL_MS, defaultPath, parseCookiePairs, parseSetCookie, pathMatches } from './cookies'

const NOW = 1_700_000_000_000

describe('defaultPath / pathMatches', () => {
  it('follows RFC 6265 default-path', () => {
    expect(defaultPath('/')).toBe('/')
    expect(defaultPath('/a')).toBe('/')
    expect(defaultPath('/a/b')).toBe('/a')
    expect(defaultPath('/a/b/')).toBe('/a/b')
    expect(defaultPath('relative')).toBe('/')
  })

  it('matches on path boundaries only', () => {
    expect(pathMatches('/a/b', '/a')).toBe(true)
    expect(pathMatches('/a', '/a')).toBe(true)
    expect(pathMatches('/ab', '/a')).toBe(false)
    expect(pathMatches('/a/b', '/a/')).toBe(true)
    expect(pathMatches('/anything', '/')).toBe(true)
    expect(pathMatches('/', '/a')).toBe(false)
  })
})

describe('parseSetCookie', () => {
  it('reads name, value, path and HttpOnly; ignores Domain, Secure and SameSite', () => {
    const p = parseSetCookie('sid=abc; Path=/app; HttpOnly; Secure; SameSite=Lax; Domain=example.com', '/x/y', NOW)!
    expect(p.entry).toMatchObject({ name: 'sid', value: 'abc', path: '/app', httpOnly: true, expiresAt: null })
    expect(p.remove).toBe(false)
  })

  it('uses the request directory when Path is missing or not absolute', () => {
    expect(parseSetCookie('a=1', '/x/y', NOW)!.entry.path).toBe('/x')
    expect(parseSetCookie('a=1; Path=nope', '/x/y', NOW)!.entry.path).toBe('/x')
  })

  it('prefers Max-Age over Expires', () => {
    const p = parseSetCookie('a=1; Max-Age=60; Expires=Wed, 21 Oct 2015 07:28:00 GMT', '/', NOW)!
    expect(p.entry.expiresAt).toBe(NOW + 60_000)
    expect(p.remove).toBe(false)
  })

  it('treats a past Expires and Max-Age <= 0 as removal', () => {
    expect(parseSetCookie('a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT', '/', NOW)!.remove).toBe(true)
    expect(parseSetCookie('a=1; Max-Age=0', '/', NOW)!.remove).toBe(true)
    expect(parseSetCookie('a=1; Max-Age=-5', '/', NOW)!.remove).toBe(true)
  })

  it('rejects cookies without a name, without =, with control characters or over 4096 bytes', () => {
    expect(parseSetCookie('novalue', '/', NOW)).toBeUndefined()
    expect(parseSetCookie('=x', '/', NOW)).toBeUndefined()
    expect(parseSetCookie('a=b\r\nSet-Cookie: c=d', '/', NOW)).toBeUndefined()
    expect(parseSetCookie(`a=${'x'.repeat(4096)}`, '/', NOW)).toBeUndefined()
    expect(parseSetCookie(`a=${'x'.repeat(4094)}`, '/', NOW)).toBeDefined()
  })

  it('keeps = signs inside the value and quoted values', () => {
    expect(parseSetCookie('t=a=b==; Path=/', '/', NOW)!.entry.value).toBe('a=b==')
    expect(parseSetCookie('t="quoted"', '/', NOW)!.entry.value).toBe('"quoted"')
  })
})

describe('CookieJar', () => {
  it('sends cookies by path match, longest path first, then oldest first', () => {
    const jar = new CookieJar()
    jar.set('a=root; Path=/', '/', NOW)
    jar.set('b=deep; Path=/app/admin', '/', NOW + 1)
    jar.set('c=app; Path=/app', '/', NOW + 2)
    jar.set('d=root2; Path=/', '/', NOW + 3)
    expect(jar.header('/app/admin/x', NOW + 10)).toBe('b=deep; c=app; a=root; d=root2')
    expect(jar.header('/app', NOW + 10)).toBe('c=app; a=root; d=root2')
    expect(jar.header('/other', NOW + 10)).toBe('a=root; d=root2')
  })

  it('replaces by (name, path) and keeps the original creation order', () => {
    const jar = new CookieJar()
    jar.set('a=1; Path=/', '/', NOW)
    jar.set('b=1; Path=/', '/', NOW + 1)
    jar.set('a=2; Path=/', '/', NOW + 2)
    expect(jar.header('/', NOW + 3)).toBe('a=2; b=1')
    jar.set('a=other; Path=/sub', '/', NOW + 4)
    expect(jar.header('/sub/x', NOW + 5)).toBe('a=other; a=2; b=1')
  })

  it('deletes on an expired Set-Cookie and reports whether anything changed', () => {
    const jar = new CookieJar()
    expect(jar.set('a=1; Path=/', '/', NOW)).toBe(true)
    expect(jar.set('a=; Max-Age=0; Path=/', '/', NOW + 1)).toBe(true)
    expect(jar.header('/', NOW + 2)).toBe('')
    expect(jar.set('a=; Max-Age=0; Path=/', '/', NOW + 3)).toBe(false)
  })

  it('hides HttpOnly cookies from document.cookie but sends them', () => {
    const jar = new CookieJar()
    jar.set('sid=s; Path=/; HttpOnly', '/', NOW)
    jar.set('theme=dark; Path=/', '/', NOW)
    expect(jar.header('/', NOW)).toBe('sid=s; theme=dark')
    expect(jar.documentCookie('/', NOW)).toBe('theme=dark')
    expect(jar.visible(NOW).map((e) => e.name)).toEqual(['theme'])
  })

  it('does not let scripts set HttpOnly cookies or overwrite them', () => {
    const jar = new CookieJar()
    expect(jar.set('x=1; HttpOnly', '/', NOW, true)).toBe(false)
    jar.set('sid=server; Path=/; HttpOnly', '/', NOW)
    expect(jar.set('sid=script; Path=/', '/', NOW + 1, true)).toBe(false)
    expect(jar.set('sid=script; Path=/; Max-Age=0', '/', NOW + 1, true)).toBe(false)
    expect(jar.header('/', NOW + 2)).toBe('sid=server')
  })

  it('stops sending expired cookies and prunes them', () => {
    const jar = new CookieJar()
    jar.set('a=1; Path=/; Max-Age=10', '/', NOW)
    jar.set('b=1; Path=/', '/', NOW)
    expect(jar.header('/', NOW + 9_000)).toBe('a=1; b=1')
    expect(jar.header('/', NOW + 10_000)).toBe('b=1')
    expect(jar.prune(NOW + 10_000)).toBe(true)
    expect(jar.size).toBe(1)
    expect(jar.prune(NOW + 10_001)).toBe(false)
  })

  it('lets session cookies live for 24 hours', () => {
    const jar = new CookieJar()
    jar.set('s=1; Path=/', '/', NOW)
    expect(jar.header('/', NOW + SESSION_COOKIE_TTL_MS - 1)).toBe('s=1')
    expect(jar.header('/', NOW + SESSION_COOKIE_TTL_MS)).toBe('')
  })

  it('evicts the oldest cookies beyond the cap', () => {
    const jar = new CookieJar()
    for (let i = 0; i < MAX_COOKIES + 5; i++) jar.set(`c${i}=1; Path=/`, '/', NOW + i)
    expect(jar.size).toBe(MAX_COOKIES)
    expect(jar.header('/', NOW + 1000)).not.toContain('c0=')
    expect(jar.header('/', NOW + 1000)).toContain(`c${MAX_COOKIES + 4}=`)
  })

  it('round-trips through all() and replace()', () => {
    const jar = new CookieJar()
    jar.set('a=1; Path=/x', '/', NOW)
    jar.set('b=2; Path=/; HttpOnly', '/', NOW)
    const copy = new CookieJar(jar.all(NOW))
    expect(copy.header('/x', NOW)).toBe(jar.header('/x', NOW))
    const view = new CookieJar()
    view.replace(jar.visible(NOW))
    expect(view.header('/x', NOW)).toBe('a=1')
  })

  it('signature changes only when visible cookies change', () => {
    const jar = new CookieJar()
    const empty = jar.visibleSignature(NOW)
    jar.set('sid=s; Path=/; HttpOnly', '/', NOW)
    expect(jar.visibleSignature(NOW)).toBe(empty)
    jar.set('a=1; Path=/', '/', NOW)
    expect(jar.visibleSignature(NOW)).not.toBe(empty)
  })
})

describe('parseCookiePairs', () => {
  it('reads document.cookie style text as root-path session cookies', () => {
    const entries = parseCookiePairs('a=1; b=two=2; ; bad; =x', NOW)
    expect(entries.map((e) => [e.name, e.value, e.path])).toEqual([
      ['a', '1', '/'],
      ['b', 'two=2', '/'],
    ])
    expect(parseCookiePairs('', NOW)).toEqual([])
  })
})
