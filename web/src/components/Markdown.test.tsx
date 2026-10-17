import { fireEvent, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { classifyImage, Markdown, STATIC_NAME_MAX } from './Markdown'

describe('classifyImage', () => {
  it('lets inline bitmaps and same-origin paths load directly, and flags every other host as remote', () => {
    const o = 'https://hub.example.com'
    expect(classifyImage('data:image/png;base64,AAAA', o)).toEqual({ kind: 'inline' })
    expect(classifyImage('blob:https://hub.example.com/1234', o)).toEqual({ kind: 'inline' })
    expect(classifyImage('/assets/logo.png', o)).toEqual({ kind: 'static' })
    expect(classifyImage('https://hub.example.com/icons/a.svg', o)).toEqual({ kind: 'static' })
    expect(classifyImage('/favicon.svg', o)).toEqual({ kind: 'static' })
    expect(classifyImage('/icon-maskable-512.png', o)).toEqual({ kind: 'static' })
    expect(classifyImage('https://evil.example/x.png?d=secret', o)).toEqual({ kind: 'remote', host: 'evil.example' })
    expect(classifyImage('http://evil.example:8080/x.png', o)).toEqual({ kind: 'remote', host: 'evil.example:8080' })
    expect(classifyImage('//evil.example/x.png', o)).toEqual({ kind: 'remote', host: 'evil.example' })
  })
  it('treats same-origin paths that are not known static assets, or that carry a query, like remote images', () => {
    const o = 'https://hub.example.com'
    for (const p of ['/x?d=SECRET', '/assets/logo.png?d=SECRET', 'https://hub.example.com/x.png', '/api/me', '/auth/logout', '/assets/', '/assets', '/assets/../api/me', '/assets/%2e%2e/api/me', '/icons/%2e%2e/x', '/assets/x.png?', '//hub.example.com/api/me?x=1', 'https://user:pw@hub.example.com/assets/x.png']) {
      const got = classifyImage(p, o)
      expect(got.kind, p).toBe('remote')
    }
    expect(classifyImage('/x?d=SECRET', o)).toEqual({ kind: 'remote', host: 'hub.example.com' })
  })
  it('only lets one path segment of [A-Za-z0-9._-] with a length cap load directly from /assets and /icons (no data in the path)', () => {
    const o = 'https://hub.example.com'
    // 构建产物的样子：一层、字母数字点横杠下划线
    for (const ok of ['/assets/index-BQsy88HA.js', '/assets/Logo-o_HEntJ4.png', '/icons/a.svg', '/assets/a', `/assets/${'a'.repeat(STATIC_NAME_MAX)}`, '/assets/x.png#frag']) {
      expect(classifyImage(ok, o).kind, ok).toBe('static')
    }
    const bad = [
      '/assets/a/b.png', // 第二层目录
      '/assets/a/b/c/d.png',
      '/assets/a//b.png',
      `/assets/${'a'.repeat(STATIC_NAME_MAX + 1)}`, // 超长：路径里能塞数据
      `/assets/${'a'.repeat(1000)}.png`,
      '/assets/a%20b.png', // 百分号编码
      '/assets/%41.png',
      '/assets/a%2Fb.png',
      '/assets/a b.png',
      '/assets/a;b=c.png',
      '/assets/a,b.png',
      '/assets/a@b.png',
      '/assets/a~b.png',
      '/assets/a+b.png',
      '/assets/a=b.png',
      '/assets/a:b.png',
      '/assets/é.png',
      '/icons/a/b.svg',
      '/icons/s%65cret.svg',
      '/assets/x.png?', // 空 query 也不行
      '/assets/x.png?d=1',
    ]
    for (const p of bad) expect(classifyImage(p, o).kind, p).toBe('remote')
  })
  it('blocks scripts, svg data urls and empty sources', () => {
    const o = 'https://hub.example.com'
    for (const bad of ['', 'javascript:alert(1)', 'data:text/html,<script>1</script>', 'data:image/svg+xml;base64,AAAA', 'file:///etc/passwd']) expect(classifyImage(bad, o).kind).toBe('blocked')
  })
})

describe('Markdown images', () => {
  it('never renders a remote <img> up front: it shows a click-to-load placeholder with the host', () => {
    const { container } = render(<Markdown text={'![chart](https://evil.example/x?d=SECRET)'} />)
    expect(container.querySelector('img')).toBeNull()
    const ph = screen.getByTestId('md-image-placeholder')
    expect(ph).toHaveTextContent('evil.example')
    expect(ph).not.toHaveTextContent('SECRET')
  })

  it('loads the remote image only after the user clicks, without a referrer', async () => {
    const user = userEvent.setup()
    const { container } = render(<Markdown text={'![chart](https://evil.example/x.png)'} />)
    await user.click(screen.getByTestId('md-image-placeholder'))
    const img = container.querySelector('img')
    expect(img).not.toBeNull()
    expect(img).toHaveAttribute('src', 'https://evil.example/x.png')
    expect(img).toHaveAttribute('referrerpolicy', 'no-referrer')
    expect(screen.queryByTestId('md-image-placeholder')).toBeNull()
  })

  it('falls back to a plain link when the browser refuses to load it (the hub CSP only allows self/data/blob images)', async () => {
    const user = userEvent.setup()
    const { container } = render(<Markdown text={'![chart](https://evil.example/x.png)'} />)
    await user.click(screen.getByTestId('md-image-placeholder'))
    fireEvent.error(container.querySelector('img')!)
    expect(container.querySelector('img')).toBeNull()
    const link = screen.getByTestId('md-image-link')
    expect(link).toHaveAttribute('href', 'https://evil.example/x.png')
    expect(link).toHaveAttribute('rel', expect.stringContaining('noopener'))
  })

  it('puts a same-origin image with a query behind the click-to-load placeholder too (it would leak the query to the hub)', () => {
    const { container } = render(<Markdown text={'![](/x?d=SECRET)\n\n![](/assets/x.png?d=SECRET)'} />)
    expect(container.querySelector('img')).toBeNull()
    expect(screen.getAllByTestId('md-image-placeholder')).toHaveLength(2)
  })

  it('puts a same-origin image that carries data in its path behind the placeholder', () => {
    const { container } = render(<Markdown text={`![](/assets/a/SECRET.png)\n\n![](/assets/${'s'.repeat(300)}.png)\n\n![](/assets/%53ECRET.png)`} />)
    expect(container.querySelector('img')).toBeNull()
    expect(screen.getAllByTestId('md-image-placeholder')).toHaveLength(3)
  })

  it('renders data: bitmaps and hub static assets directly', () => {
    const { container } = render(<Markdown text={'![a](data:image/png;base64,iVBORw0KGgo=)\n\n![b](/assets/x.png)'} />)
    const srcs = [...container.querySelectorAll('img')].map((i) => i.getAttribute('src'))
    expect(srcs).toEqual(['data:image/png;base64,iVBORw0KGgo=', '/assets/x.png'])
    expect(screen.queryByTestId('md-image-placeholder')).toBeNull()
  })

  it('drops javascript: image sources instead of rendering them', () => {
    const { container } = render(<Markdown text={'![x](javascript:alert(1))'} />)
    expect(container.querySelector('img')).toBeNull()
    expect(screen.queryByTestId('md-image-placeholder')).toBeNull()
  })

  it('does not interpret raw HTML: <img>, <script> and event handlers come out as text', () => {
    const { container } = render(<Markdown text={'<img src="https://evil.example/p.png" onerror="alert(1)">\n\n<script>alert(1)</script>\n\n<div onclick="alert(1)">hi</div>'} />)
    expect(container.querySelector('img')).toBeNull()
    expect(container.querySelector('script')).toBeNull()
    expect(container.querySelector('[onclick]')).toBeNull()
    expect(container.querySelector('[onerror]')).toBeNull()
    expect(container).toHaveTextContent('<script>alert(1)</script>')
  })

  it('keeps angle brackets in prose (Vec<String>) instead of swallowing them as tags', () => {
    const { container } = render(<Markdown text={'Use Vec<String> here'} />)
    expect(container).toHaveTextContent('Use Vec<String> here')
  })

  it('links open in a new tab without opener', () => {
    render(<Markdown text={'[docs](https://example.com)'} />)
    const a = screen.getByRole('link', { name: 'docs' })
    expect(a).toHaveAttribute('target', '_blank')
    expect(a.getAttribute('rel')).toContain('noopener')
  })
})
