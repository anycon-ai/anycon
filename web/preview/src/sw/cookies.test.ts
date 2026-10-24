import { describe, expect, it } from 'vitest'
import { MemoryCookies } from '../test/memory'
import { CookieStore } from './cookies'

const NOW = 1_700_000_000_000

function make(storage = new MemoryCookies(), clock = { now: NOW }) {
  return { storage, clock, store: new CookieStore(storage, () => clock.now) }
}

describe('CookieStore', () => {
  it('stores Set-Cookie values and sends them back by path', async () => {
    const { store } = make()
    await store.storeSetCookies(['sid=abc; Path=/; HttpOnly', 'theme=dark; Path=/app'], '/login')
    expect(await store.header('/app/x')).toBe('theme=dark; sid=abc')
    expect(await store.header('/other')).toBe('sid=abc')
    expect(await store.documentCookie('/app/x')).toBe('theme=dark')
  })

  it('reports whether the cookies a page can see changed', async () => {
    const { store } = make()
    expect(await store.storeSetCookies(['sid=1; HttpOnly'], '/')).toBe(false) // 页面看不到
    expect(await store.storeSetCookies(['a=1'], '/')).toBe(true)
    expect(await store.storeSetCookies(['a=1'], '/')).toBe(false) // 没变
    expect(await store.storeSetCookies(['a=2'], '/')).toBe(true)
    expect(await store.storeSetCookies([], '/')).toBe(false)
    expect(await store.storeSetCookies(['a=; Max-Age=0'], '/')).toBe(true)
  })

  it('persists after every change and survives a restart, including HttpOnly cookies', async () => {
    const { storage, store } = make()
    await store.storeSetCookies(['sid=abc; Path=/; HttpOnly', 'a=1; Path=/'], '/')
    expect(storage.saved.map((c) => c.name).sort()).toEqual(['a', 'sid'])
    const again = new CookieStore(storage, () => NOW + 1000)
    expect(await again.header('/')).toBe('sid=abc; a=1')
  })

  it('drops expired cookies when it loads', async () => {
    const storage = new MemoryCookies()
    const first = new CookieStore(storage, () => NOW)
    await first.storeSetCookies(['short=1; Max-Age=10', 'long=1; Max-Age=1000'], '/')
    const later = new CookieStore(storage, () => NOW + 60_000)
    expect(await later.header('/')).toBe('long=1')
  })

  it('applies document.cookie writes, which cannot set or replace HttpOnly cookies', async () => {
    const { store } = make()
    await store.storeSetCookies(['sid=server; HttpOnly'], '/')
    expect(await store.setFromDocument('theme=light; path=/', '/page')).toBe(true)
    expect(await store.setFromDocument('sid=client', '/page')).toBe(false)
    expect(await store.setFromDocument('x=1; HttpOnly', '/page')).toBe(false)
    expect(await store.header('/page')).toBe('sid=server; theme=light')
  })

  it('writes without Path use the document directory', async () => {
    const { store } = make()
    await store.setFromDocument('k=v', '/docs/page')
    expect(await store.header('/docs/other')).toBe('k=v')
    expect(await store.header('/elsewhere')).toBe('')
  })

  it('gives the page a snapshot without HttpOnly cookies', async () => {
    const { store } = make()
    await store.storeSetCookies(['sid=1; HttpOnly', 'a=1; Path=/x'], '/')
    expect((await store.visible()).map((c) => [c.name, c.path])).toEqual([['a', '/x']])
  })

  it('starts empty when storage fails', async () => {
    const storage = { load: () => Promise.reject(new Error('idb')), save: () => Promise.reject(new Error('idb')) }
    const store = new CookieStore(storage, () => NOW)
    expect(await store.header('/')).toBe('')
    await expect(store.storeSetCookies(['a=1'], '/')).resolves.toBe(true)
  })
})
