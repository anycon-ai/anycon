import type { PreviewResponseHead } from '@anycon/protocol'
import { describe, expect, it, vi } from 'vitest'
import {
  filterResponseHeaders,
  frameAncestorsPolicy,
  hasNullBody,
  isAsciiCompatibleCharset,
  isHtml,
  isUsableStatus,
  tapStream,
  transformBody,
  withFrameAncestors,
} from './response'

const head = (headers: [string, string][], over: Partial<PreviewResponseHead> = {}): PreviewResponseHead => ({ status: 200, headers, encoding: 'identity', ...over })

async function drain(stream: ReadableStream<Uint8Array>): Promise<string> {
  return new Response(stream).text()
}
const streamOf = (...parts: Array<string | Uint8Array>): ReadableStream<Uint8Array> =>
  new ReadableStream({
    start(c) {
      for (const p of parts) c.enqueue(typeof p === 'string' ? new TextEncoder().encode(p) : p)
      c.close()
    },
  })

describe('filterResponseHeaders', () => {
  it('moves set-cookie out, drops x-frame-options and hop-by-hop headers, keeps the rest in order', () => {
    const { headers, setCookies } = filterResponseHeaders(
      head([
        ['content-type', 'text/html'],
        ['set-cookie', 'a=1; Path=/'],
        ['x-frame-options', 'DENY'],
        ['set-cookie', 'b=2; HttpOnly'],
        ['connection', 'close'],
        ['etag', '"x"'],
        ['content-length', '12'],
      ]),
      false,
    )
    expect(setCookies).toEqual(['a=1; Path=/', 'b=2; HttpOnly'])
    expect([...headers.keys()]).toEqual(['content-length', 'content-type', 'etag'])
    expect(headers.has('x-frame-options')).toBe(false)
  })

  it('drops content-length when the body is rewritten or was gzip', () => {
    expect(filterResponseHeaders(head([['content-length', '5']]), true).headers.has('content-length')).toBe(false)
    expect(filterResponseHeaders(head([['content-length', '5']], { encoding: 'gzip' }), false).headers.has('content-length')).toBe(false)
    expect(filterResponseHeaders(head([['content-length', '5']]), false).headers.get('content-length')).toBe('5')
  })

  it('combines repeated headers and skips values the browser would refuse', () => {
    const { headers } = filterResponseHeaders(head([['link', '</a>; rel=preload'], ['link', '</b>; rel=preload'], ['bad name', 'x'], ['x-ok', 'line\nbreak'], ['x-fine', '1']]), false)
    expect(headers.get('link')).toBe('</a>; rel=preload, </b>; rel=preload')
    expect(headers.get('x-fine')).toBe('1')
    expect(headers.has('x-ok')).toBe(false)
  })
})

describe('frame-ancestors', () => {
  it('allows the hub and the preview itself, nobody else', () => {
    expect(frameAncestorsPolicy('https://hub.example')).toBe("frame-ancestors 'self' https://hub.example")
    expect(frameAncestorsPolicy(undefined)).toBe("frame-ancestors 'self'")
  })

  it('is added next to the dev server’s own CSP, which stays', () => {
    const base = new Headers({ 'content-security-policy': "default-src 'self'" })
    const out = withFrameAncestors(base, 'https://hub.example')
    expect(out.get('content-security-policy')).toBe("default-src 'self', frame-ancestors 'self' https://hub.example")
    expect(base.get('content-security-policy')).toBe("default-src 'self'") // 原来的不被改
    expect(withFrameAncestors(new Headers(), 'https://hub.example').get('content-security-policy')).toBe("frame-ancestors 'self' https://hub.example")
  })
})

describe('status and content type helpers', () => {
  it('knows which statuses a Response can carry and which have no body', () => {
    expect([100, 101, 199, 200, 404, 599, 600].map(isUsableStatus)).toEqual([false, false, false, true, true, true, false])
    expect([101, 103, 204, 205, 304, 200, 404].map(hasNullBody)).toEqual([true, true, true, true, true, false, false])
  })

  it('recognises html and ascii-compatible charsets', () => {
    expect(isHtml('text/html; charset=utf-8')).toBe(true)
    expect(isHtml('application/xhtml+xml')).toBe(true)
    expect(isHtml('text/plain')).toBe(false)
    expect(isHtml(null)).toBe(false)
    expect(isAsciiCompatibleCharset('text/html')).toBe(true)
    expect(isAsciiCompatibleCharset('text/html; charset=UTF-8')).toBe(true)
    expect(isAsciiCompatibleCharset('text/html; charset="gbk"')).toBe(true)
    expect(isAsciiCompatibleCharset('text/html; charset=utf-16le')).toBe(false)
    expect(isAsciiCompatibleCharset('text/html; charset=UTF-32')).toBe(false)
  })
})

describe('transformBody', () => {
  it('decompresses gzip', async () => {
    const gz = new Blob(['hello gzip world']).stream().pipeThrough(new CompressionStream('gzip')) as ReadableStream<Uint8Array>
    expect(await drain(transformBody(gz, { gzip: true }))).toBe('hello gzip world')
  })

  it('decompresses gzip split across arbitrary chunks, then injects', async () => {
    const compressed = new Uint8Array(await new Response(new Blob(['<html><head></head><body>x</body></html>']).stream().pipeThrough(new CompressionStream('gzip'))).arrayBuffer())
    const parts: Uint8Array[] = []
    for (let i = 0; i < compressed.length; i += 5) parts.push(compressed.subarray(i, i + 5))
    const out = await drain(transformBody(streamOf(...parts), { gzip: true, inject: { cookies: 'a=1' } }))
    expect(out).toBe('<html><head><script src="/__anycon/shim.js" data-cookies="a=1"></script></head><body>x</body></html>')
  })

  it('passes identity bodies untouched', async () => {
    expect(await drain(transformBody(streamOf('plain'), { gzip: false }))).toBe('plain')
  })

  it('fails the stream on corrupt gzip', async () => {
    await expect(drain(transformBody(streamOf('this is not gzip at all'), { gzip: true }))).rejects.toThrow()
  })
})

describe('tapStream', () => {
  it('reports every chunk and the end', async () => {
    const seen: string[] = []
    const out = tapStream(streamOf('a', 'b'), { chunk: (c) => seen.push(new TextDecoder().decode(c)), end: () => seen.push('end'), abort: () => seen.push('abort') })
    expect(await drain(out)).toBe('ab')
    expect(seen).toEqual(['a', 'b', 'end'])
  })

  it('reports an abort when the reader cancels or the source fails', async () => {
    const events: string[] = []
    const tap = { chunk: () => {}, end: () => events.push('end'), abort: () => events.push('abort') }
    const reader = tapStream(streamOf('a', 'b'), tap).getReader()
    await reader.read()
    await reader.cancel()
    expect(events).toEqual(['abort'])
    const failing = new ReadableStream<Uint8Array>({
      pull() {
        throw new Error('boom')
      },
    })
    const spy = vi.fn()
    await expect(drain(tapStream(failing, { ...tap, abort: spy }))).rejects.toThrow('boom')
    expect(spy).toHaveBeenCalledOnce()
  })
})
