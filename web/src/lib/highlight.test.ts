import { describe, expect, it } from 'vitest'
import { LANG_BY_EXT, LANG_BY_NAME } from '@/features/files/paths'
import { highlightCode, resolveLang } from './highlight'

describe('highlight languages', () => {
  it('every language the file browser picks by extension or file name can be loaded', () => {
    for (const lang of new Set([...Object.values(LANG_BY_EXT), ...Object.values(LANG_BY_NAME)])) expect(resolveLang(lang), lang).toBe(lang)
  })

  it('knows the common aliases used in Markdown fences', () => {
    expect(resolveLang('ts')).toBe('typescript')
    expect(resolveLang('C++')).toBe('cpp')
    expect(resolveLang('kt')).toBe('kotlin')
    expect(resolveLang('docker')).toBe('dockerfile')
    expect(resolveLang('nope')).toBeNull()
    expect(resolveLang(undefined)).toBeNull()
  })

  it('highlights the languages added for the file browser (each one is a separate lazily loaded module)', async () => {
    for (const [lang, code] of [
      ['c', 'int main(void) { return 0; }'],
      ['cpp', 'std::vector<int> v;'],
      ['java', 'class A { void f() {} }'],
      ['kotlin', 'fun main() {}'],
      ['swift', 'let a = 1'],
      ['ruby', 'def f; end'],
      ['sql', 'SELECT 1 FROM t'],
      ['xml', '<a b="c"/>'],
      ['scss', 'a { b { c: d } }'],
      ['lua', 'local a = 1'],
      ['dockerfile', 'FROM scratch'],
      ['makefile', 'all:\n\techo hi'],
      ['ini', '[a]\nb=c'],
    ] as const) {
      const html = await highlightCode(code, lang)
      expect(html, lang).toContain('class="shiki')
    }
  }, 30_000)
})
