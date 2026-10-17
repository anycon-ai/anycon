// hub 的 CSP 是 `script-src 'self' 'wasm-unsafe-eval'`，不允许内联脚本和内联事件处理器。
// 这里守住 index.html（源码，以及构建产物 dist/index.html 如果存在）。
import { existsSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..') + '/'

function check(path: string): void {
  const html = readFileSync(path, 'utf8')
  const scripts = [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)]
  expect(scripts.length).toBeGreaterThan(0)
  for (const [, attrs, body] of scripts) {
    expect(attrs, `inline <script> in ${path}`).toMatch(/\bsrc=/)
    expect(body!.trim(), `inline script body in ${path}`).toBe('')
  }
  expect(html, 'inline event handler').not.toMatch(/\son[a-z]+\s*=/i)
  expect(html, 'javascript: URL').not.toMatch(/javascript:/i)
}

describe('index.html is CSP-clean (no inline scripts / handlers)', () => {
  it('source index.html', () => check(`${root}index.html`))
  it('the theme bootstrap lives in a static file', () => {
    expect(existsSync(`${root}public/theme.js`)).toBe(true)
    expect(readFileSync(`${root}index.html`, 'utf8')).toContain('src="/theme.js"')
  })
  it.skipIf(!existsSync(`${root}dist/index.html`))('built dist/index.html', () => check(`${root}dist/index.html`))
})
