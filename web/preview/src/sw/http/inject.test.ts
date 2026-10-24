import { describe, expect, it } from 'vitest'
import { MAX_INJECT_BUFFER, createInjector, escapeAttribute, findInsertionPoint, shimTag } from './inject'

const enc = new TextEncoder()
const dec = new TextDecoder('utf-8', { ignoreBOM: true })
const TAG = '<script src="/s.js"></script>'

async function run(chunks: Array<string | Uint8Array>, tag = TAG): Promise<{ out: string; pieces: number }> {
  const t = createInjector(enc.encode(tag))
  const writer = t.writable.getWriter()
  const reader = t.readable.getReader()
  const got: Uint8Array[] = []
  const reading = (async () => {
    for (;;) {
      const r = await reader.read()
      if (r.done) return
      got.push(r.value)
    }
  })()
  for (const c of chunks) await writer.write(typeof c === 'string' ? enc.encode(c) : c)
  await writer.close()
  await reading
  return { out: dec.decode(Buffer.concat(got)), pieces: got.length }
}

describe('where the tag goes', () => {
  it.each([
    ['<!doctype html><html><head><title>x</title></head><body></body></html>', '<!doctype html><html><head>' + TAG + '<title>x</title></head><body></body></html>'],
    ['<!DOCTYPE html>\n<HTML lang="en">\n<HEAD profile="p">\n<meta charset="utf-8">', '<!DOCTYPE html>\n<HTML lang="en">\n<HEAD profile="p">' + TAG + '\n<meta charset="utf-8">'],
    ['<html><head data-x=">"><script>', '<html><head data-x=">">' + TAG + '<script>'],
    ['<!-- <head> --><!doctype html><head></head>', '<!-- <head> --><!doctype html><head>' + TAG + '</head>'],
    ['<?xml version="1.0"?><html xmlns="x"><head/>', '<?xml version="1.0"?><html xmlns="x"><head/>' + TAG],
  ])('after the first <head>: %s', async (input, expected) => {
    expect((await run([input])).out).toBe(expected)
  })

  it('without <head> goes after <html>', async () => {
    expect((await run(['<!doctype html><html lang="en"><body>hi</body></html>'])).out).toBe(`<!doctype html><html lang="en">${TAG}<body>hi</body></html>`)
  })

  it('without <head> or <html> goes after the doctype, never before it', async () => {
    expect((await run(['<!doctype html>\n<div id="root"></div>'])).out).toBe(`<!doctype html>${TAG}\n<div id="root"></div>`)
  })

  it('with nothing to anchor on goes first', async () => {
    expect((await run(['<div>x</div>'])).out).toBe(`${TAG}<div>x</div>`)
    expect((await run(['just text'])).out).toBe(`${TAG}just text`)
    expect((await run([''])).out).toBe(TAG)
  })

  it('keeps a UTF-8 BOM in front', async () => {
    const bytes = Uint8Array.from([0xef, 0xbb, 0xbf, ...enc.encode('<p>x</p>')])
    const { out } = await run([bytes])
    expect(out).toBe(`\uFEFF${TAG}<p>x</p>`)
  })

  it('does not mistake <header> for <head>', async () => {
    expect((await run(['<html><header>h</header><head></head>'])).out).toBe(`<html>${TAG}<header>h</header><head></head>`)
  })

  it('works on non-ASCII pages byte for byte', async () => {
    const page = '<!doctype html><html><head><title>中文</title></head><body>é</body></html>'
    expect((await run([page])).out).toBe(page.replace('<head>', `<head>${TAG}`))
  })
})

describe('streaming', () => {
  const page = '<!doctype html><html lang="en"><head><meta charset="utf-8"><title>t</title></head><body>' + 'x'.repeat(5_000) + '</body></html>'
  const expected = page.replace('<head>', `<head>${TAG}`)

  it('gives the same result however the bytes are cut', async () => {
    for (const size of [1, 2, 3, 7, 16, 64, 1000]) {
      const bytes = enc.encode(page)
      const chunks: Uint8Array[] = []
      for (let i = 0; i < bytes.length; i += size) chunks.push(bytes.subarray(i, i + size))
      expect((await run(chunks)).out, `chunk size ${size}`).toBe(expected)
    }
  })

  it('releases the buffered start as soon as the insertion point is known, and passes the rest through untouched', async () => {
    const t = createInjector(enc.encode(TAG))
    const w = t.writable.getWriter()
    const r = t.readable.getReader()
    const first = w.write(enc.encode('<html><head>'))
    const out1 = await r.read()
    await first
    expect(dec.decode(out1.value)).toBe(`<html><head>${TAG}`)
    const later = enc.encode('later chunk')
    void w.write(later)
    const out2 = await r.read()
    expect(out2.value).toBe(later) // 没有拷贝，原样
  })

  it('stops waiting after 64 KiB and flushes with the best anchor it has', async () => {
    const filler = `<!-- ${'x'.repeat(MAX_INJECT_BUFFER)} -->`
    const { out } = await run([`<!doctype html>${filler}`, '<head></head>'])
    expect(out.startsWith(`<!doctype html>${TAG}<!-- `)).toBe(true)
    expect(out.endsWith('<head></head>')).toBe(true)
  })

  it('emits something at the end of a stream that never reached a decision', async () => {
    expect((await run(['<html><he'])).out).toBe(`<html>${TAG}<he`)
  })
})

describe('findInsertionPoint', () => {
  it('asks for more data instead of guessing', () => {
    expect(findInsertionPoint(enc.encode('<!doctype html><ht'), false)).toBeUndefined()
    expect(findInsertionPoint(enc.encode('<!doctype html><html'), false)).toBeUndefined()
    expect(findInsertionPoint(enc.encode('<!-- unterminated'), false)).toBeUndefined()
    expect(findInsertionPoint(enc.encode('   '), false)).toBeUndefined()
    expect(findInsertionPoint(enc.encode('<html><body>'), false)).toBe(6)
  })
})

describe('the tag', () => {
  it('carries the cookies HTML-escaped', () => {
    expect(dec.decode(shimTag('a=1; b="x"&<y>'))).toBe('<script src="/__anycon/shim.js" data-cookies="a=1; b=&quot;x&quot;&amp;&lt;y&gt;"></script>')
    expect(escapeAttribute(`'"&<>`)).toBe('&#39;&quot;&amp;&lt;&gt;')
  })
})
