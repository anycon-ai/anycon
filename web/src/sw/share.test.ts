// 分享目标的纯逻辑和暂存库（spec/web.md § 8、§ 14.5）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { deleteDB, openDB } from 'idb'
import { CAPTURE_PATH, FIELD_MAX, cut, STASH_TTL_MS, composeSharedText, handleShare, isShareRequest, readFields } from './share'
import { SHARE_DB, deleteShareDb, storeShared, takeShared } from './shareStore'

const ORIGIN = 'https://hub.example'
const form = (o: Record<string, unknown>) => ({ get: (n: string) => o[n] ?? null })

describe('isShareRequest', () => {
  it('is a same-origin POST to exactly /capture', () => {
    expect(isShareRequest('POST', `${ORIGIN}/capture`, ORIGIN)).toBe(true)
    expect(isShareRequest('post', `${ORIGIN}/capture`, ORIGIN)).toBe(true)
    expect(isShareRequest('GET', `${ORIGIN}/capture`, ORIGIN)).toBe(false)
    expect(isShareRequest('POST', `${ORIGIN}/capture/`, ORIGIN)).toBe(false)
    expect(isShareRequest('POST', `${ORIGIN}/capturex`, ORIGIN)).toBe(false)
    expect(isShareRequest('POST', `${ORIGIN}/api/capture`, ORIGIN)).toBe(false)
    expect(isShareRequest('POST', 'https://evil.example/capture', ORIGIN)).toBe(false)
    expect(isShareRequest('POST', 'not a url', ORIGIN)).toBe(false)
  })
})

describe('readFields', () => {
  it('keeps the three string fields, drops files and anything else, truncates huge values', () => {
    const f = readFields(form({ title: 'T', text: 'x'.repeat(FIELD_MAX + 5), url: 'https://a.test', file: new Blob(['x']), other: 'nope' }), 42)
    expect(f.title).toBe('T')
    expect(f.text).toHaveLength(FIELD_MAX)
    expect(f).toMatchObject({ url: 'https://a.test', at: 42 })
    expect(Object.keys(f).sort()).toEqual(['at', 'text', 'title', 'url'])
    expect(readFields(form({ title: new Blob(['x']) }), 1).title).toBe('')
  })
})

describe('truncation', () => {
  it('never splits a surrogate pair: an emoji straddling the limit is dropped whole', () => {
    const v = `${'a'.repeat(FIELD_MAX - 1)}\u{1F600}tail`
    const out = cut(v)
    expect(out).toBe('a'.repeat(FIELD_MAX - 1))
    expect(out.charCodeAt(out.length - 1)).toBe(97)
    expect(cut(`${'a'.repeat(FIELD_MAX - 2)}\u{1F600}x`)).toBe(`${'a'.repeat(FIELD_MAX - 2)}\u{1F600}`)
    expect(cut('short')).toBe('short')
  })
})

describe('handleShare', () => {
  it('stores the fields and answers with the /capture path (no content in it)', async () => {
    const store = vi.fn(async () => undefined)
    const path = await handleShare({ formData: async () => form({ title: 'secret title', text: 'secret text', url: 'https://secret.test/?q=1' }), store, now: () => 7 })
    expect(path).toBe(CAPTURE_PATH)
    expect(path).not.toMatch(/secret|\?/)
    expect(store).toHaveBeenCalledWith({ title: 'secret title', text: 'secret text', url: 'https://secret.test/?q=1', at: 7 })
  })

  it('still redirects when reading the form or storing fails, and logs nothing about the content', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    await expect(handleShare({ formData: async () => Promise.reject(new Error('secret text in the message')), store: vi.fn(), now: () => 1 })).resolves.toBe(CAPTURE_PATH)
    await expect(handleShare({ formData: async () => form({ text: 'secret text' }), store: async () => Promise.reject(new Error('quota secret text')), now: () => 1 })).resolves.toBe(CAPTURE_PATH)
    for (const call of warn.mock.calls) expect(JSON.stringify(call)).not.toContain('secret')
    warn.mockRestore()
  })

  it('does not store an empty share (it must not overwrite one that is waiting)', async () => {
    const store = vi.fn(async () => undefined)
    await handleShare({ formData: async () => form({}), store, now: () => 1 })
    expect(store).not.toHaveBeenCalled()
  })
})

describe('composeSharedText', () => {
  it('title first, then the text, then the link unless it is already there', () => {
    expect(composeSharedText({ title: 'Page', text: 'quote', url: 'https://a.test' })).toBe('Page\nquote\nhttps://a.test')
    expect(composeSharedText({ title: '', text: 'just text', url: '' })).toBe('just text')
    expect(composeSharedText({ title: 'Page', text: 'see https://a.test', url: 'https://a.test' })).toBe('Page\nsee https://a.test')
    expect(composeSharedText({ title: 'Same', text: 'Same', url: '' })).toBe('Same')
    expect(composeSharedText({ title: '', text: '', url: 'https://a.test' })).toBe('https://a.test')
    expect(composeSharedText({ title: '', text: '', url: '' })).toBe('')
  })
})

describe('stash (anycon-share)', () => {
  beforeEach(async () => deleteDB(SHARE_DB))
  afterEach(async () => deleteDB(SHARE_DB))

  it('the page takes it once: reading deletes it', async () => {
    await storeShared({ title: 'T', text: 'x', url: '', at: 1000 })
    expect(await takeShared(2000)).toEqual({ title: 'T', text: 'x', url: '', at: 1000 })
    expect(await takeShared(2000)).toBeNull()
  })

  it('only the latest share is kept', async () => {
    await storeShared({ title: 'first', text: '', url: '', at: 1 })
    await storeShared({ title: 'second', text: '', url: '', at: 2 })
    expect((await takeShared(3))?.title).toBe('second')
  })

  it('a stale or malformed stash is dropped (and removed), never shown', async () => {
    await storeShared({ title: 'old', text: '', url: '', at: 1000 })
    expect(await takeShared(1000 + STASH_TTL_MS + 1)).toBeNull()
    const db = await openDB(SHARE_DB, 1)
    await db.put('stash', { title: 5 }, 'latest')
    db.close()
    expect(await takeShared(1)).toBeNull()
    const again = await openDB(SHARE_DB, 1)
    expect(await again.get('stash', 'latest')).toBeUndefined()
    again.close()
  })

  it('reading never creates the database (nothing was ever shared)', async () => {
    expect(await takeShared()).toBeNull()
    const names = (await indexedDB.databases()).map((d) => d.name)
    expect(names).not.toContain(SHARE_DB)
  })

  it('“remove this device” deletes the whole database, including a share that was never taken', async () => {
    await storeShared({ title: 'T', text: 'secret', url: '', at: 1 })
    await deleteShareDb()
    expect((await indexedDB.databases()).map((d) => d.name)).not.toContain(SHARE_DB)
    expect(await takeShared(2)).toBeNull()
  })
})
