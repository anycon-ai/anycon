// 宽屏左栏的拖动条：拖动改宽度（只在松手时保存）、范围限制、双击恢复默认、键盘调整。
import { useState } from 'react'
import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { clampSidebarWidth, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN } from '@/stores/prefs'
import { SidebarResizeHandle } from './SidebarResizeHandle'

function Harness({ initial = 260, onCommit }: { initial?: number; onCommit: (w: number) => void }) {
  const [saved, setSaved] = useState(initial)
  const [drag, setDrag] = useState<number | null>(null)
  return (
    <SidebarResizeHandle
      width={drag ?? saved}
      onResize={setDrag}
      onCommit={(w) => {
        setDrag(null)
        setSaved(w)
        onCommit(w)
      }}
    />
  )
}

const handle = (): HTMLElement => screen.getByTestId('sidebar-resize')

describe('SidebarResizeHandle', () => {
  it('follows the pointer while dragging and saves only on release', () => {
    const onCommit = vi.fn()
    render(<Harness onCommit={onCommit} />)
    fireEvent.pointerDown(handle(), { button: 0, clientX: 300, pointerId: 1 })
    expect(document.documentElement.dataset.resizing).toBe('1')
    fireEvent.pointerMove(handle(), { clientX: 340, pointerId: 1 })
    fireEvent.pointerMove(handle(), { clientX: 360, pointerId: 1 })
    expect(handle()).toHaveAttribute('aria-valuenow', '320')
    expect(onCommit).not.toHaveBeenCalled()
    fireEvent.pointerUp(handle(), { clientX: 360, pointerId: 1 })
    expect(onCommit).toHaveBeenCalledExactlyOnceWith(320)
    expect(document.documentElement.dataset.resizing).toBeUndefined()
  })

  it('stays within the allowed range', () => {
    const onCommit = vi.fn()
    render(<Harness onCommit={onCommit} />)
    fireEvent.pointerDown(handle(), { button: 0, clientX: 300, pointerId: 1 })
    fireEvent.pointerMove(handle(), { clientX: 2000, pointerId: 1 })
    expect(handle()).toHaveAttribute('aria-valuenow', String(SIDEBAR_MAX))
    fireEvent.pointerMove(handle(), { clientX: -2000, pointerId: 1 })
    fireEvent.pointerUp(handle(), { pointerId: 1 })
    expect(onCommit).toHaveBeenLastCalledWith(SIDEBAR_MIN)
  })

  it('double-click resets to the default width', () => {
    const onCommit = vi.fn()
    render(<Harness initial={400} onCommit={onCommit} />)
    fireEvent.doubleClick(handle())
    expect(onCommit).toHaveBeenLastCalledWith(SIDEBAR_DEFAULT)
  })

  it('arrow keys resize by 16px, 48px with Shift', () => {
    const onCommit = vi.fn()
    render(<Harness onCommit={onCommit} />)
    fireEvent.keyDown(handle(), { key: 'ArrowRight' })
    expect(onCommit).toHaveBeenLastCalledWith(276)
    fireEvent.keyDown(handle(), { key: 'ArrowLeft', shiftKey: true })
    expect(onCommit).toHaveBeenLastCalledWith(228)
  })

  it('a bad stored width falls back to the default', () => {
    expect(clampSidebarWidth(Number.NaN)).toBe(SIDEBAR_DEFAULT)
    expect(clampSidebarWidth(123.4)).toBe(SIDEBAR_MIN)
  })
})
