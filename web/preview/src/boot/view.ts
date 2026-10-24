// 把 `BootView` 画到页面上。只用 `textContent` / `createElement`，不拼 HTML。
import { text, type Lang, type TextKey } from '../shared/i18n'
import type { BootView } from './flow'

interface Copy {
  title: TextKey
  body?: TextKey
}

function copyOf(view: BootView): Copy {
  switch (view.kind) {
    case 'starting':
      return { title: 'starting' }
    case 'needs_launch':
      return { title: 'needsLaunch.title', body: 'needsLaunch.body' }
    case 'expired':
      return { title: 'expired.title', body: 'expired.body' }
    case 'invalid':
      return { title: 'invalid.title', body: 'invalid.body' }
    case 'unsupported':
      return { title: 'unsupported.title', body: 'unsupported.body' }
    case 'failed':
      return { title: 'failed.title', body: 'failed.body' }
    case 'loop':
      return { title: 'loop.title', body: 'loop.body' }
  }
}

export function renderView(doc: Document, root: HTMLElement, lang: Lang, view: BootView): void {
  const copy = copyOf(view)
  doc.documentElement.lang = lang
  doc.title = text(lang, 'title')
  const h1 = doc.createElement('h1')
  h1.textContent = text(lang, copy.title)
  const nodes: HTMLElement[] = [h1]
  if (copy.body) {
    const p = doc.createElement('p')
    p.textContent = text(lang, copy.body)
    nodes.push(p)
  }
  if ((view.kind === 'unsupported' || view.kind === 'failed') && view.newTabUrl) {
    const a = doc.createElement('a')
    a.className = 'button'
    a.href = view.newTabUrl
    a.target = '_blank'
    a.rel = 'noopener noreferrer'
    a.textContent = text(lang, 'openInTab')
    nodes.push(a)
  }
  root.dataset.state = view.kind
  root.replaceChildren(...nodes)
}
