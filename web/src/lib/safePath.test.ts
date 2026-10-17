// 站内路径检查：浏览器会先删掉路径里的 Tab / 换行再解析，光看 `//` 前缀拦不住 `"/\t/evil.example"`。
import { describe, expect, it } from 'vitest'
import { isSafePath } from './safePath'

const ORIGIN = 'https://hub.example'

describe('isSafePath', () => {
  it.each(['/', '/n/abcd1234/s/sess_1', '/n/abcd1234/settings?tab=enroll&review=1', '/settings/notifications', '/a/b?x=%2F%2Fevil.example#frag', '/%09/evil', '/%5Cevil', '/n/ab.cd/s/x.y'])('lets the in-site path %j through', (path) => {
    expect(isSafePath(path, ORIGIN)).toBe(true)
    // 真的解析一遍也还在本站
    expect(new URL(path, ORIGIN).origin).toBe(ORIGIN)
  })

  it.each([
    ['tab after the slash', '/\t/evil.example'],
    ['newline after the slash', '/\n/evil.example'],
    ['carriage return then backslash', '/\r\\evil.example'],
    ['tabs between the slashes', '/\t\t/evil.example/x'],
    ['vertical tab', '/\u000b/evil.example'],
    ['form feed', '/\u000c/evil.example'],
    ['NUL', '/\u0000/evil.example'],
    ['DEL', '/\u007f/evil.example'],
    ['C1 control (NEL)', '/\u0085/evil.example'],
    ['plain space', '/ /evil.example'],
    ['no-break space', '/ /evil.example'],
    ['line separator', '/ /evil.example'],
    ['ideographic space', '/　/evil.example'],
    ['byte order mark', '/﻿/evil.example'],
    ['trailing newline', '/n/abc\n'],
    ['protocol-relative', '//evil.example'],
    ['protocol-relative with path', '//evil.example/x'],
    ['slash backslash', '/\\evil.example'],
    ['double backslash', '/\\\\evil'],
    ['backslash inside', '/a\\b'],
    ['absolute URL', 'https://evil.example/'],
    ['javascript URL', 'javascript:alert(1)'],
    ['data URL', 'data:text/html,x'],
    ['relative path', 'n/relative'],
    ['dot relative', './x'],
    ['empty', ''],
    ['query only', '?x=1'],
  ])('refuses %s', (_name, path) => {
    expect(isSafePath(path, ORIGIN)).toBe(false)
  })

  it('refuses anything that is not a string', () => {
    for (const v of [undefined, null, 3, {}, [], ['/x']]) expect(isSafePath(v, ORIGIN)).toBe(false)
  })

  it('the final check is the browser’s own: whatever resolves to another origin is refused', () => {
    // 如果有哪种写法躲过了前两层，这一层按 WHATWG 的规则解析，origin 变了就拒绝
    for (const path of ['/\t/evil.example', '/\n/evil.example', '/\r\\evil.example']) {
      expect(new URL(path, ORIGIN).origin).toBe('https://evil.example') // 这就是原来的漏洞
      expect(isSafePath(path, ORIGIN)).toBe(false)
    }
  })

  it('the last word is the browser’s own resolution: the path must resolve to exactly the origin it is checked against', () => {
    expect(isSafePath('/x', 'not an origin')).toBe(false) // 解析不了：不放行
    expect(isSafePath('/x', 'https://hub.example/')).toBe(false) // 不是 `location.origin` 的写法（带了路径）：不等于解析出来的 origin
    expect(isSafePath('/x', ORIGIN)).toBe(true)
  })

  it('defaults to the current page’s origin', () => {
    expect(isSafePath('/n/abcd1234')).toBe(true)
    expect(isSafePath('/\t/evil.example')).toBe(false)
  })
})
