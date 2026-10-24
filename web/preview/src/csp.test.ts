// hub 在预览域名上给引导页的 CSP 是 `script-src 'self' 'wasm-unsafe-eval'`（spec/hub.md § 13.2），不允许内联脚本和内联事件处理器。
// 和 Web UI 的 `src/csp.test.ts` 一样的约束，守住 `index.html`（源码，以及构建产物 `dist-preview/index.html` 如果存在）。
import { existsSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const preview = resolve(dirname(fileURLToPath(import.meta.url)), '..') + '/'
const built = resolve(preview, '../dist-preview') + '/'

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
  expect(html, 'external resource').not.toMatch(/(?:src|href)\s*=\s*["']?(?:https?:)?\/\//i)
}

describe('index.html is CSP-clean (no inline scripts / handlers, nothing from other origins)', () => {
  it('source index.html', () => check(`${preview}index.html`))

  it.skipIf(!existsSync(`${built}index.html`))('built dist-preview/index.html', () => {
    check(`${built}index.html`)
    const html = readFileSync(`${built}index.html`, 'utf8')
    // base 是 /__anycon/：脚本和样式都从 hub 的预览文件路径取
    for (const m of html.matchAll(/(?:src|href)="([^"]+)"/g)) expect(m[1], m[1]).toMatch(/^\/__anycon\/assets\//)
  })

  it('the page script is a module loaded from a file, not inlined into the HTML', () => {
    expect(readFileSync(`${preview}index.html`, 'utf8')).toContain('<script type="module" src="./src/boot.ts"></script>')
  })
})
