// @vitest-environment jsdom
import { describe, expect, it } from 'vitest'
import { text } from '../shared/i18n'
import type { BootView } from './flow'
import { renderView } from './view'

function render(view: BootView, lang: 'en' | 'zh' = 'en') {
  document.body.innerHTML = '<div id="root"></div>'
  const root = document.getElementById('root')!
  renderView(document, root, lang, view)
  return root
}

describe('renderView', () => {
  it('shows a title and an explanation for each state, in the page language', () => {
    const root = render({ kind: 'needs_launch' })
    expect(root.querySelector('h1')?.textContent).toBe(text('en', 'needsLaunch.title'))
    expect(root.querySelector('p')?.textContent).toBe(text('en', 'needsLaunch.body'))
    expect(document.documentElement.lang).toBe('en')
    expect(root.dataset.state).toBe('needs_launch')
    const zh = render({ kind: 'expired' }, 'zh')
    expect(zh.querySelector('h1')?.textContent).toBe('这个预览已过期')
    expect(document.documentElement.lang).toBe('zh')
    expect(document.title).toBe('Anycon 预览')
  })

  it('shows only a title while starting', () => {
    const root = render({ kind: 'starting' })
    expect(root.querySelector('p')).toBeNull()
    expect(root.querySelector('a')).toBeNull()
  })

  it('offers the new-tab link when there is one, safely', () => {
    const root = render({ kind: 'unsupported', newTabUrl: 'https://x.example/__anycon/enter#abc' })
    const a = root.querySelector('a')!
    expect(a.getAttribute('href')).toBe('https://x.example/__anycon/enter#abc')
    expect(a.target).toBe('_blank')
    expect(a.rel).toBe('noopener noreferrer')
    expect(a.textContent).toBe('Open in a new tab')
    expect(render({ kind: 'unsupported', newTabUrl: undefined }).querySelector('a')).toBeNull()
    expect(render({ kind: 'loop' }).querySelector('a')).toBeNull()
  })

  it('replaces the previous content', () => {
    const root = render({ kind: 'starting' })
    renderView(document, root, 'en', { kind: 'invalid' })
    expect(root.querySelectorAll('h1')).toHaveLength(1)
    expect(root.querySelector('h1')?.textContent).toBe(text('en', 'invalid.title'))
  })
})
