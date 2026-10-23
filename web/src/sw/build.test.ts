// 构建产物（`bun run --cwd web build` 之后才有 dist/）：service worker 在、预缓存清单已注入、push 相关的事件都登记了。
import { existsSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const dist = resolve(dirname(fileURLToPath(import.meta.url)), '../../dist') + '/'

describe.skipIf(!existsSync(`${dist}sw.js`))('built service worker (dist/sw.js)', () => {
  const sw = existsSync(`${dist}sw.js`) ? readFileSync(`${dist}sw.js`, 'utf8') : ''
  // 压缩器可能把字符串写成双引号、单引号或反引号，断言时都认
  const hasString = (text: string): boolean => ['"', "'", '`'].some((q) => sw.includes(`${q}${text}${q}`))

  it('has the precache manifest injected, with the app shell in it', () => {
    expect(sw).not.toContain('self.__WB_MANIFEST')
    expect(sw).toContain('"url":"index.html"')
    expect(sw).toMatch(/"url":"assets\/index-[^"]+\.js"/)
  })

  it('is a classic script a browser can register (no module syntax, nothing left to import)', () => {
    // 压缩后只有一两行，不能只看行首
    expect(sw).not.toMatch(/\bexport\s*(\{|\*|default\b)/)
    expect(sw).not.toMatch(/\bimport\s*\(|import\.meta|(^|[;}])\s*import\s*[{*"'\w]/)
  })

  it('handles push, notificationclick, pushsubscriptionchange and the page’s SKIP_WAITING', () => {
    for (const e of ['push', 'notificationclick', 'pushsubscriptionchange', 'SKIP_WAITING']) expect(hasString(e), e).toBe(true)
  })

  it('does not take over on its own: the only skipWaiting is the answer to the page’s message, and there is no clients.claim', () => {
    expect(sw).not.toMatch(/clients\.claim/)
    const calls = [...sw.matchAll(/\.skipWaiting\(\)/g)]
    expect(calls).toHaveLength(1)
    // 这一次调用就在「消息里 type === SKIP_WAITING」的处理里
    const at = calls[0]!.index
    expect(sw.slice(Math.max(0, at - 200), at)).toContain('.type===')
  })

  it('keeps the navigation fallback to index.html and its deny list', () => {
    expect(hasString('/index.html')).toBe(true)
    expect(sw).toContain('[/^\\/api\\//,/^\\/auth\\//,/^\\/v1\\//,/^\\/healthz/,/^\\/readyz/]')
    expect(sw).toMatch(/denylist:/)
  })
})
