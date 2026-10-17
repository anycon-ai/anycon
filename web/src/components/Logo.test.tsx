import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import favicon from '../../public/favicon.svg?raw'
import logo from '../../public/logo.svg?raw'
import { Logo, MARK_D } from './Logo'

// 主标的设计稿原件在 public/（logo.svg 彩色、favicon.svg 单色）；Logo 组件内联的是同一条外形路径。
// 这个测试保证两边不会悄悄分叉——换了设计稿要同步 Logo.tsx，并重跑 scripts/icons.mjs 出 PNG。
describe('Logo', () => {
  it('inlines the same mark outline as public/logo.svg and public/favicon.svg', () => {
    const { container } = render(<Logo withWord={false} />)
    const clip = container.querySelector('clipPath path')
    expect(clip?.getAttribute('d')).toBe(MARK_D)
    expect(logo).toContain(`d="${MARK_D}"`)
    expect(favicon).toContain(`d="${MARK_D}"`)
  })

  it('gives each instance its own gradient ids', () => {
    const { container } = render(
      <>
        <Logo withWord={false} />
        <Logo withWord={false} />
      </>,
    )
    const ids = [...container.querySelectorAll('linearGradient, clipPath')].map((el) => el.id)
    expect(new Set(ids).size).toBe(ids.length)
    const g = container.querySelector('g')
    expect(ids).toContain(g?.getAttribute('clip-path')?.slice(5, -1))
  })

  it('shows the wordmark by default and hides it on request', () => {
    expect(render(<Logo />).container.textContent).toBe('Anycon')
    expect(render(<Logo withWord={false} />).container.textContent).toBe('')
  })
})
