// 隧道的响应 → 浏览器的 `Response`（spec/web.md § 13.2「响应」「注入 shim」）。
import type { PreviewResponseHead } from '@anycon/protocol'
import { createInjector, shimTag } from './inject'

/** 没有正文的状态码：`new Response(stream, { status })` 对它们会抛错。 */
const NULL_BODY_STATUS = new Set([101, 103, 204, 205, 304])

/** 不能作为 `Response` 状态码的（`new Response` 只接受 200–599）：当作上游出错。 */
export function isUsableStatus(status: number): boolean {
  return Number.isInteger(status) && status >= 200 && status <= 599
}

export function hasNullBody(status: number): boolean {
  return NULL_BODY_STATUS.has(status)
}

/** `frame-ancestors`：预览页只允许被 hub 的公网 origin 嵌入（还有它自己：dev app 自己的 iframe）。 */
export function frameAncestorsPolicy(hubOrigin: string | undefined): string {
  return hubOrigin ? `frame-ancestors 'self' ${hubOrigin}` : "frame-ancestors 'self'"
}

export interface FilteredHeaders {
  headers: Headers
  /** 响应里的 `Set-Cookie`（交给 cookie jar，不转给页面）。 */
  setCookies: string[]
}

/** 加 `content-security-policy: frame-ancestors …`（dev server 自己的 CSP 头保留，浏览器取交集）。 */
export function withFrameAncestors(headers: Headers, hubOrigin: string | undefined): Headers {
  const out = new Headers(headers)
  out.append('content-security-policy', frameAncestorsPolicy(hubOrigin))
  return out
}

/**
 * 响应头：去掉 `set-cookie`（存进 cookie jar）和 `x-frame-options`。`dropLength`：正文会被改写（注入 shim），
 * `content-length` 作废。`content-security-policy` 由 `withFrameAncestors` 另外加，这样存进缓存的是不带它的原始头。
 */
export function filterResponseHeaders(head: PreviewResponseHead, dropLength: boolean): FilteredHeaders {
  const headers = new Headers()
  const setCookies: string[] = []
  for (const [rawName, value] of head.headers) {
    const name = rawName.toLowerCase()
    if (name === 'set-cookie') setCookies.push(value)
    else if (name === 'x-frame-options') continue
    else if (name === 'content-length' && (dropLength || head.encoding === 'gzip')) continue
    else if (name === 'transfer-encoding' || name === 'connection' || name === 'keep-alive') continue
    else {
      try {
        headers.append(name, value)
      } catch {
        // 页面能看到的头不合法（上游给了怪值）：丢掉这一个
      }
    }
  }
  return { headers, setCookies }
}

/** 这个编码的 HTML 能不能在字节层面插入 ASCII 的 `<script>`（UTF-16 / UTF-32 不行）。 */
export function isAsciiCompatibleCharset(contentType: string | null): boolean {
  const m = /charset\s*=\s*"?([^;"\s]+)/i.exec(contentType ?? '')
  if (!m) return true
  return !/^(utf-?(16|32)|ucs-?[24]|unicode)/i.test(m[1]!)
}

export function isHtml(contentType: string | null): boolean {
  return /^\s*(text\/html|application\/xhtml\+xml)\b/i.test(contentType ?? '')
}

/** 响应体经过的变换：gzip 解压，然后（导航 HTML）注入 shim。 */
export function transformBody(
  body: ReadableStream<Uint8Array>,
  opts: { gzip: boolean; inject?: { cookies: string } },
): ReadableStream<Uint8Array> {
  let out = body
  if (opts.gzip) out = out.pipeThrough(new DecompressionStream('gzip') as unknown as TransformStream<Uint8Array, Uint8Array>)
  if (opts.inject) out = out.pipeThrough(createInjector(shimTag(opts.inject.cookies)))
  return out
}

/** 读一遍流，把每块交给 `tap`；流结束 / 出错 / 被取消时分别通知。 */
export function tapStream(
  source: ReadableStream<Uint8Array>,
  tap: { chunk(c: Uint8Array): void; end(): void; abort(): void },
): ReadableStream<Uint8Array> {
  const reader = source.getReader()
  return new ReadableStream<Uint8Array>(
    {
      async pull(controller) {
        let r: ReadableStreamReadResult<Uint8Array>
        try {
          r = await reader.read()
        } catch (err) {
          tap.abort()
          controller.error(err)
          return
        }
        if (r.done) {
          tap.end()
          controller.close()
        } else {
          tap.chunk(r.value)
          controller.enqueue(r.value)
        }
      },
      cancel(reason) {
        tap.abort()
        return reader.cancel(reason)
      },
    },
    { highWaterMark: 0 },
  )
}
