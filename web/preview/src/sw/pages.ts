// 隧道不可用 / 上游出错时返回给页面的东西（spec/web.md § 13.2「错误」）：
// 导航请求回一页 HTML 说明原因，终态时提示「回到 Anycon 重新打开」；子资源请求回 502 纯文本。
import { text, type Lang, type TextKey } from '../shared/i18n'
import type { UnavailableKind } from './tunnel/link'
import { frameAncestorsPolicy } from './http/response'

/** 隧道之外的错误：dev server 那一侧。 */
export type UpstreamKind = 'upstream_refused' | 'upstream_timeout' | 'upstream_error' | 'too_large'
export type ErrorKind = UnavailableKind | UpstreamKind

interface Entry {
  title: TextKey
  body: TextKey
  /** 是终态：提示回到 Anycon 重新打开。 */
  terminal: boolean
  /** 会自己好起来：页面自动刷新重试。 */
  retry: boolean
  status: number
}

const ENTRIES: Record<ErrorKind, Entry> = {
  no_launch: { title: 'needsLaunch.title', body: 'needsLaunch.body', terminal: false, retry: false, status: 401 },
  expired: { title: 'expired.title', body: 'expired.body', terminal: true, retry: false, status: 502 },
  unauthorized: { title: 'unauthorized.title', body: 'unauthorized.body', terminal: true, retry: false, status: 502 },
  revoked: { title: 'revoked.title', body: 'revoked.body', terminal: true, retry: false, status: 502 },
  version: { title: 'version.title', body: 'version.body', terminal: true, retry: false, status: 502 },
  closed: { title: 'closed.title', body: 'closed.body', terminal: true, retry: false, status: 502 },
  disabled: { title: 'disabled.title', body: 'disabled.body', terminal: true, retry: false, status: 502 },
  port_not_allowed: { title: 'portNotAllowed.title', body: 'portNotAllowed.body', terminal: true, retry: false, status: 502 },
  shutdown: { title: 'shutdown.title', body: 'shutdown.body', terminal: true, retry: false, status: 502 },
  rejected: { title: 'rejected.title', body: 'rejected.body', terminal: true, retry: false, status: 502 },
  handshake_failed: { title: 'handshakeFailed.title', body: 'handshakeFailed.body', terminal: true, retry: false, status: 502 },
  crypto_unavailable: { title: 'cryptoUnavailable.title', body: 'cryptoUnavailable.body', terminal: true, retry: false, status: 502 },
  node_offline: { title: 'nodeOffline.title', body: 'nodeOffline.body', terminal: false, retry: true, status: 503 },
  connecting: { title: 'connecting.title', body: 'connecting.body', terminal: false, retry: true, status: 503 },
  upstream_refused: { title: 'upstreamRefused.title', body: 'upstreamRefused.body', terminal: false, retry: false, status: 502 },
  upstream_timeout: { title: 'upstreamTimeout.title', body: 'upstreamTimeout.body', terminal: false, retry: false, status: 504 },
  upstream_error: { title: 'upstreamError.title', body: 'upstreamError.body', terminal: false, retry: false, status: 502 },
  too_large: { title: 'tooLarge.title', body: 'tooLarge.body', terminal: false, retry: false, status: 413 },
}

/** 自动重试的间隔（秒）。 */
const RETRY_SECONDS = 5

const ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }
const escapeHtml = (s: string): string => s.replace(/[&<>"']/g, (c) => ESCAPES[c]!)

const STYLE =
  ':root{color-scheme:light dark}body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;font:16px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif;background:#fff;color:#17171c}' +
  'main{max-width:30rem;padding:2rem}h1{font-size:1.25rem;margin:0 0 .5rem}p{margin:.5rem 0;color:#565662}pre{margin:1rem 0 0;padding:.5rem .75rem;border-radius:6px;background:#f1f1f5;color:#565662;font-size:.8rem;white-space:pre-wrap;word-break:break-word}' +
  '@media(prefers-color-scheme:dark){body{background:#121314;color:#bfbfbf}p{color:#8c8c8c}pre{background:#242526;color:#8c8c8c}}'

export function errorPageHtml(kind: ErrorKind, lang: Lang, detail?: string): string {
  const e = ENTRIES[kind]
  const title = text(lang, e.title)
  const parts = [
    '<!doctype html>',
    `<html lang="${lang}"><head><meta charset="utf-8">`,
    '<meta name="viewport" content="width=device-width,initial-scale=1">',
    '<meta name="color-scheme" content="light dark">',
    e.retry ? `<meta http-equiv="refresh" content="${RETRY_SECONDS}">` : '',
    `<title>${escapeHtml(title)}</title><style>${STYLE}</style></head><body><main>`,
    `<h1>${escapeHtml(title)}</h1><p>${escapeHtml(text(lang, e.body))}</p>`,
    e.terminal ? `<p>${escapeHtml(text(lang, 'reopen'))}</p>` : '',
    detail ? `<pre>${escapeHtml(detail)}</pre>` : '',
    '</main></body></html>',
  ]
  return parts.join('')
}

export interface ErrorOptions {
  kind: ErrorKind
  lang: Lang
  /** 导航请求：回 HTML 页；其他回 502 纯文本。 */
  navigate: boolean
  hubOrigin?: string
  detail?: string
}

export function errorResponse(opts: ErrorOptions): Response {
  const e = ENTRIES[opts.kind]
  const common = { 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' }
  if (opts.navigate) {
    return new Response(errorPageHtml(opts.kind, opts.lang, opts.detail), {
      status: e.status,
      headers: {
        ...common,
        'content-type': 'text/html; charset=utf-8',
        'content-security-policy': `default-src 'none'; style-src 'unsafe-inline'; ${frameAncestorsPolicy(opts.hubOrigin)}`,
        ...(e.retry ? { 'retry-after': String(RETRY_SECONDS) } : {}),
      },
    })
  }
  const body = `Anycon preview: ${text(opts.lang, e.title)}. ${text(opts.lang, e.body)}`
  return new Response(body, {
    status: e.status === 413 ? 413 : 502,
    headers: { ...common, 'content-type': 'text/plain; charset=utf-8' },
  })
}
