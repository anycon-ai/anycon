import { describe, expect, it } from 'vitest'
import { textKeys } from '../shared/i18n'
import { errorPageHtml, errorResponse, type ErrorKind } from './pages'

const KINDS: ErrorKind[] = [
  'no_launch',
  'expired',
  'unauthorized',
  'revoked',
  'version',
  'closed',
  'disabled',
  'port_not_allowed',
  'shutdown',
  'rejected',
  'handshake_failed',
  'crypto_unavailable',
  'node_offline',
  'connecting',
  'upstream_refused',
  'upstream_timeout',
  'upstream_error',
  'too_large',
]

describe('copy', () => {
  it('has the same keys in English and Chinese', () => {
    expect(textKeys('zh')).toEqual(textKeys('en'))
  })
})

describe('error pages for navigations', () => {
  it.each(KINDS)('%s renders a complete page in both languages', (kind) => {
    for (const lang of ['en', 'zh'] as const) {
      const html = errorPageHtml(kind, lang)
      expect(html.startsWith('<!doctype html>')).toBe(true)
      expect(html).toContain(`<html lang="${lang}">`)
      expect(html).toMatch(/<h1>[^<]+<\/h1>/)
      expect(html).not.toMatch(/<script/i)
    }
  })

  it('terminal reasons tell the user to reopen from Anycon; transient ones retry on their own', () => {
    expect(errorPageHtml('expired', 'en')).toContain('open the preview again')
    expect(errorPageHtml('closed', 'zh')).toContain('回到 Anycon 重新打开预览')
    const offline = errorPageHtml('node_offline', 'en')
    expect(offline).toContain('<meta http-equiv="refresh" content="5">')
    expect(offline).not.toContain('open the preview again')
    expect(errorPageHtml('expired', 'en')).not.toContain('http-equiv="refresh"')
    expect(errorPageHtml('upstream_refused', 'en')).not.toContain('http-equiv="refresh"')
  })

  it('escapes the detail text', () => {
    const html = errorPageHtml('rejected', 'en', '<img src=x onerror=alert(1)>&"')
    expect(html).toContain('&lt;img src=x onerror=alert(1)&gt;&amp;&quot;')
    expect(html).not.toContain('<img')
  })

  it('is a no-store response with a locked-down CSP that only the hub and itself may frame', async () => {
    const res = errorResponse({ kind: 'port_not_allowed', lang: 'en', navigate: true, hubOrigin: 'https://hub.example' })
    expect(res.status).toBe(502)
    expect(res.headers.get('content-type')).toBe('text/html; charset=utf-8')
    expect(res.headers.get('cache-control')).toBe('no-store')
    expect(res.headers.get('content-security-policy')).toBe("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'self' https://hub.example")
    expect(res.headers.has('retry-after')).toBe(false)
    expect(await res.text()).toContain('port can no longer be previewed')
  })

  it('uses 503 with retry-after for transient reasons, 401 when there is no launch, 504 for upstream timeouts', () => {
    const r = (kind: ErrorKind) => errorResponse({ kind, lang: 'en', navigate: true })
    expect(r('node_offline').status).toBe(503)
    expect(r('node_offline').headers.get('retry-after')).toBe('5')
    expect(r('connecting').status).toBe(503)
    expect(r('no_launch').status).toBe(401)
    expect(r('upstream_timeout').status).toBe(504)
    expect(r('too_large').status).toBe(413)
  })
})

describe('error responses for sub-resources', () => {
  it('is a 502 plain text', async () => {
    const res = errorResponse({ kind: 'node_offline', lang: 'en', navigate: false })
    expect(res.status).toBe(502)
    expect(res.headers.get('content-type')).toBe('text/plain; charset=utf-8')
    expect(await res.text()).toContain('The machine is offline')
  })

  it('keeps 413 for oversized requests', () => {
    expect(errorResponse({ kind: 'too_large', lang: 'zh', navigate: false }).status).toBe(413)
  })
})
