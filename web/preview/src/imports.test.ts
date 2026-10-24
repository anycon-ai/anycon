// 预览运行时只依赖 `@anycon/protocol`：不引 React、不引 Web UI（`web/src`）的任何代码（spec/web.md § 13.2）。
// 测试文件可以用 vitest、node 内置模块和 fake-indexeddb。
import { readFileSync, readdirSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const src = resolve(dirname(fileURLToPath(import.meta.url)))
const previewRoot = resolve(src, '..')

function files(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? files(join(dir, e.name)) : e.name.endsWith('.ts') ? [join(dir, e.name)] : []))
}

function specifiers(file: string): string[] {
  const code = readFileSync(file, 'utf8')
  return [...code.matchAll(/(?:^|\n)\s*(?:import|export)\b[^'"`;]*?\bfrom\s+['"]([^'"]+)['"]/g), ...code.matchAll(/(?:^|\n)\s*import\s+['"]([^'"]+)['"]/g), ...code.matchAll(/\bimport\(\s*['"]([^'"]+)['"]\s*\)/g)].map((m) => m[1]!)
}

const all = files(src)
const isTest = (f: string): boolean => /\.test\.ts$/.test(f) || f.includes('/src/test/')

describe('imports', () => {
  it('finds the source files', () => {
    expect(all.length).toBeGreaterThan(30)
  })

  it('runtime code imports only relative files and @anycon/protocol', () => {
    for (const file of all.filter((f) => !isTest(f))) {
      for (const spec of specifiers(file)) {
        expect(spec.startsWith('.') || spec === '@anycon/protocol', `${relative(previewRoot, file)} imports ${spec}`).toBe(true)
      }
    }
  })

  it('tests add only vitest, node built-ins and fake-indexeddb', () => {
    for (const file of all.filter(isTest)) {
      for (const spec of specifiers(file)) {
        const ok = spec.startsWith('.') || spec === '@anycon/protocol' || spec === 'vitest' || spec.startsWith('node:') || spec === 'fake-indexeddb/auto'
        expect(ok, `${relative(previewRoot, file)} imports ${spec}`).toBe(true)
      }
    }
  })

  it('no relative import leaves web/preview (web/src belongs to the Web UI)', () => {
    for (const file of all) {
      for (const spec of specifiers(file).filter((s) => s.startsWith('.'))) {
        const target = resolve(dirname(file), spec)
        expect(target.startsWith(previewRoot + '/'), `${relative(previewRoot, file)} imports ${spec}`).toBe(true)
      }
    }
  })
})
