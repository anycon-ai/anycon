// 输入控件的宽度：缺省占满一行，调用方给了 `w-*` 就只用调用方的（`cn` 不合并冲突的类，两个都在时 `w-full` 总是赢）。
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { Input, Select, Textarea } from './Field'

const classes = (id: string): string[] => screen.getByTestId(id).className.split(/\s+/)

describe('field width', () => {
  it('fills the row when the caller gives no width', () => {
    render(
      <>
        <Input data-testid="i" />
        <Select data-testid="s" className="max-w-40" />
        <Textarea data-testid="t" className="min-w-0" />
      </>,
    )
    expect(classes('i')).toContain('w-full')
    expect(classes('s')).toContain('w-full')
    expect(classes('t')).toContain('w-full')
  })

  it('uses only the caller’s width when one is given', () => {
    render(
      <>
        <Input data-testid="i" className="w-44" />
        <Select data-testid="s" className="!h-9 w-auto max-w-48" />
      </>,
    )
    expect(classes('i')).toContain('w-44')
    expect(classes('i')).not.toContain('w-full')
    expect(classes('s')).toContain('w-auto')
    expect(classes('s')).not.toContain('w-full')
  })
})
