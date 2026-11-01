// 搜索片段只认字面的 <mark> / </mark>，其余全是文本（docs/protocol.md § 5.4 的 `session.search`）。
import { describe, expect, it } from 'vitest'
import { parseSnippet, SNIPPET_MAX_CHARS } from './snippet'

const plain = (s: string) => parseSnippet(s).map((p) => p.text).join('')

describe('parseSnippet', () => {
  it('splits the text around the highlighted words', () => {
    expect(parseSnippet('please <mark>refactor</mark> the <mark>parser</mark> module')).toEqual([
      { text: 'please ', mark: false },
      { text: 'refactor', mark: true },
      { text: ' the ', mark: false },
      { text: 'parser', mark: true },
      { text: ' module', mark: false },
    ])
    expect(parseSnippet('no highlight')).toEqual([{ text: 'no highlight', mark: false }])
    expect(parseSnippet('')).toEqual([])
  })

  it('turns the three entities the daemon escapes back into characters, once', () => {
    expect(plain('a &lt;b&gt; &amp; c')).toBe('a <b> & c')
    // 内容里本来就写着 `&lt;`：daemon 转义成 `&amp;lt;`，还原一遍得到的是原文 `&lt;`，不是 `<`
    expect(plain('x &amp;lt;b&amp;gt; y')).toBe('x &lt;b&gt; y')
    // 别的实体不认识，原样留着
    expect(plain('&quot; &#60; &nbsp; &amp')).toBe('&quot; &#60; &nbsp; &amp')
  })

  it('shows markup that was escaped by the daemon as plain text, never as tags', () => {
    const parts = parseSnippet('run &lt;script&gt;alert(1)&lt;/script&gt; &lt;img src=x onerror=alert(2)&gt; <mark>now</mark>')
    expect(parts.map((p) => p.text).join('')).toBe('run <script>alert(1)</script> <img src=x onerror=alert(2)> now')
    expect(parts.filter((p) => p.mark).map((p) => p.text)).toEqual(['now'])
  })

  it('treats anything but the exact mark tags as text, even when the daemon did not escape it', () => {
    const raw = '<script>alert(1)</script> <img src=x onerror=alert(2)> <MARK>no</MARK> <mark class="x">no <mark>yes</mark>'
    const parts = parseSnippet(raw)
    expect(parts.filter((p) => p.mark).map((p) => p.text)).toEqual(['yes'])
    expect(plain(raw)).toBe('<script>alert(1)</script> <img src=x onerror=alert(2)> <MARK>no</MARK> <mark class="x">no yes')
  })

  it('does not let a literal <mark> in the conversation forge a highlight (the daemon escapes it)', () => {
    const parts = parseSnippet('the &lt;mark&gt;<mark>danger</mark>&lt;/mark&gt; tag')
    expect(parts).toEqual([
      { text: 'the <mark>', mark: false },
      { text: 'danger', mark: true },
      { text: '</mark> tag', mark: false },
    ])
  })

  it('tolerates tags that do not pair up', () => {
    expect(parseSnippet('a </mark>b')).toEqual([{ text: 'a b', mark: false }])
    expect(parseSnippet('a <mark>b <mark>c')).toEqual([
      { text: 'a ', mark: false },
      { text: 'b c', mark: true },
    ])
    expect(parseSnippet('<mark></mark>')).toEqual([])
  })

  it('replaces control and bidi characters with spaces so they cannot rewrite the surrounding text', () => {
    expect(plain('a\nb\tc‮d‏e\u0000f')).toBe('a b c d e f')
  })

  it('cuts very long text and marks the cut', () => {
    const long = `${'x'.repeat(500)} <mark>tail</mark>`
    const parts = parseSnippet(long)
    expect(parts).toHaveLength(1)
    expect([...parts[0]!.text]).toHaveLength(SNIPPET_MAX_CHARS + 1)
    expect(parts[0]!.text.endsWith('…')).toBe(true)
    // 按字符（码点）数，不会把一个字劈成两半
    const emoji = parseSnippet('😀'.repeat(10), 4)
    expect(emoji).toEqual([{ text: '😀😀😀😀…', mark: false }])
  })
})
