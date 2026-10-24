// 构建产物（`bun run --cwd web build` 之后才有 dist-preview/）：引导页、`sw.js`、`shim.js`、wasm 都在，
// `sw.js` / `shim.js` 是不带模块语法的单文件；预览运行时和 Web UI 的构建产物互不包含对方的文件，
// Web UI 的 service worker 预缓存清单里没有预览运行时的任何文件（spec/web.md § 13.2）。
import { createHash } from 'node:crypto'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const web = resolve(dirname(fileURLToPath(import.meta.url)), '../..') + '/'
const preview = `${web}dist-preview/`
const main = `${web}dist/`
const wasmBuilt = existsSync(resolve(web, '../packages/protocol/wasm/anycon_crypto_bg.wasm'))

function list(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? list(join(dir, e.name)) : [join(dir, e.name)]))
}
const rel = (root: string, files: string[]): string[] => files.map((f) => relative(root, f)).sort()
const sha = (file: string): string => createHash('sha256').update(readFileSync(file)).digest('hex')

describe.skipIf(!existsSync(`${preview}index.html`))('built preview runtime (dist-preview/)', () => {
  const read = (name: string): string => readFileSync(`${preview}${name}`, 'utf8')

  it('has the fixed-name scripts, the boot page, its assets and the licences', () => {
    const files = rel(preview, list(preview))
    for (const f of ['index.html', 'sw.js', 'shim.js', 'THIRD-PARTY-LICENSES.txt']) expect(files, f).toContain(f)
    expect(files.some((f) => /^assets\/index-[^/]+\.js$/.test(f))).toBe(true)
    expect(files.some((f) => /^assets\/index-[^/]+\.css$/.test(f))).toBe(true)
    // 引导页、sw、shim 之外没有别的 html / 脚本
    expect(files.filter((f) => f.endsWith('.html'))).toEqual(['index.html'])
  })

  it.each(['sw.js', 'shim.js'])('%s is a classic script a browser can register (no module syntax, no import(), no import.meta)', (name) => {
    const code = read(name)
    // 压缩后只有一两行，不能只看行首
    expect(code).not.toMatch(/\bexport\s*(\{|\*|default\b)/)
    expect(code).not.toMatch(/\bimport\s*\(|import\.meta|(^|[;}\n])\s*import\s*[{*"'\w]/)
    expect(code.startsWith('(function(){')).toBe(true)
    expect(code).not.toContain('sourceMappingURL')
  })

  it('sw.js handles install (skipWaiting), activate (clients.claim), fetch and message, and nothing else of the Web UI', () => {
    const sw = read('sw.js')
    for (const e of ['install', 'activate', 'fetch', 'message']) expect(['"', "'", '`'].some((q) => sw.includes(`${q}${e}${q}`)), e).toBe(true)
    expect(sw).toMatch(/\.skipWaiting\(\)/)
    expect(sw).toMatch(/clients\.claim\(\)/)
    expect(sw).toContain('anycon-preview-http') // Cache Storage
    expect(sw).toContain('/__anycon/config.json')
    expect(sw).not.toContain('__WB_MANIFEST') // 不是 Web UI 的 workbox service worker
    expect(sw).not.toMatch(/react|workbox/i)
  })

  it('sw.js loads the Noise wasm from the file that is really in assets/', () => {
    const sw = read('sw.js')
    const files = rel(preview, list(preview)).filter((f) => f.endsWith('.wasm'))
    if (!wasmBuilt) return // wasm 还没构建：运行时才会报「加密模块没能加载」
    expect(files).toHaveLength(1)
    expect(files[0]).toMatch(/^assets\/anycon_crypto_bg-[^/]+\.wasm$/)
    expect(sw).toContain(`/__anycon/${files[0]}`)
  })

  it('shim.js stays small and has no tunnel code in it', () => {
    const shim = read('shim.js')
    expect(shim.length).toBeLessThan(20 * 1024)
    expect(shim).not.toMatch(/msgpack|anycon-preview-http|previewInitiator/)
    expect(shim).toContain('keepalive')
    expect(shim).toContain('cookies.sync')
  })

  it('the boot page script carries the boot flow, not the tunnel', () => {
    const assets = list(`${preview}assets`).filter((f) => /index-[^/]+\.js$/.test(f))
    const code = readFileSync(assets[0]!, 'utf8')
    expect(code).toContain('/__anycon/sw.js')
    expect(code).not.toContain('anycon-preview-http')
  })
})

describe.skipIf(!existsSync(`${preview}index.html`) || !existsSync(`${main}index.html`))('preview runtime vs. Web UI build outputs', () => {
  // Noise wasm 两边各放一份（内容相同、文件名也可能相同），许可证文件各有各的；其余不能有交集
  const shared = (f: string): boolean => f.endsWith('.wasm') || f.endsWith('/THIRD-PARTY-LICENSES.txt')
  // 跳过的 describe 在收集阶段也会执行回调，所以目录要等到测试里再读
  const previewFiles = (): string[] => list(preview).filter((f) => !shared(f))
  const mainFiles = (): string[] => list(main).filter((f) => !shared(f))

  it('never share a file: not by content, and not by name except index.html and sw.js, which are different files in different origins', () => {
    const mainHashes = new Map(mainFiles().map((f) => [sha(f), relative(main, f)]))
    for (const f of previewFiles()) {
      expect(mainHashes.get(sha(f)), `${relative(preview, f)} is also in dist/`).toBeUndefined()
    }
    const mainNames = new Set(rel(main, mainFiles()))
    for (const name of rel(preview, previewFiles())) {
      if (name === 'index.html' || name === 'sw.js') continue
      expect(mainNames.has(name), `${name} exists in both builds`).toBe(false)
    }
  })

  it('the Web UI does not contain the preview runtime', () => {
    expect(existsSync(`${main}shim.js`)).toBe(false)
    const mainSw = readFileSync(`${main}sw.js`, 'utf8')
    const previewSw = readFileSync(`${preview}sw.js`, 'utf8')
    expect(mainSw).not.toBe(previewSw)
    for (const marker of ['anycon-preview-http', '/__anycon/shim.js', '/__anycon/config.json', 'cookies.sync']) {
      for (const f of list(main).filter((p) => /\.(js|html)$/.test(p))) {
        expect(readFileSync(f, 'utf8'), `${marker} in dist/${relative(main, f)}`).not.toContain(marker)
      }
    }
  })

  it('the Web UI service worker precache manifest lists none of the preview runtime files', () => {
    const mainSw = readFileSync(`${main}sw.js`, 'utf8')
    const urls = [...mainSw.matchAll(/"url":"([^"]+)"/g)].map((m) => m[1]!)
    expect(urls.length).toBeGreaterThan(3)
    const previewNames = new Set(rel(preview, previewFiles()))
    for (const url of urls) {
      if (url === 'index.html') continue // Web UI 自己的 index.html
      expect(previewNames.has(url), `precache lists ${url}`).toBe(false)
      expect(url, url).not.toMatch(/(^|\/)(shim\.js|__anycon)/)
    }
    // 预缓存的每个文件，内容都不是预览运行时的
    const previewHashes = new Set(previewFiles().map(sha))
    for (const url of urls) {
      const file = `${main}${url}`
      if (existsSync(file)) expect(previewHashes.has(sha(file)), `precache lists ${url}`).toBe(false)
    }
  })
})
