// 没固定的宽屏左栏：悬停窄条滑出，指针离开窄条和浮出层后收回；菜单 / 对话框开着、拖宽度时不收；点外面、Esc、固定后收回。
import { useRef } from 'react'
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useSidebarPeek } from './useSidebarPeek'

function Harness({ enabled = true }: { enabled?: boolean }) {
  const flyout = useRef<HTMLElement>(null)
  const { peek, railProps } = useSidebarPeek(enabled, flyout)
  return (
    <>
      <div data-testid="rail" {...railProps}>
        <button type="button">search</button>
      </div>
      <aside ref={flyout} data-testid="flyout" data-state={peek ? 'peek' : 'hidden'} />
    </>
  )
}

const state = (): string | null => screen.getByTestId('flyout').getAttribute('data-state')

function mount(enabled = true) {
  const r = render(<Harness enabled={enabled} />)
  // jsdom 不排版：浮出层右缘放在 300px
  vi.spyOn(screen.getByTestId('flyout'), 'getBoundingClientRect').mockReturnValue({ left: 48, right: 300, top: 0, bottom: 800, width: 252, height: 800, x: 48, y: 0, toJSON: () => ({}) })
  return r
}

function hoverRail(): void {
  fireEvent.pointerEnter(screen.getByTestId('rail'))
  act(() => vi.advanceTimersByTime(200))
}

beforeEach(() => {
  vi.useFakeTimers()
})
afterEach(() => {
  vi.useRealTimers()
  delete document.documentElement.dataset.resizing
})

describe('useSidebarPeek', () => {
  it('slides out after resting on the rail, not when the pointer just passes by', () => {
    mount()
    fireEvent.pointerEnter(screen.getByTestId('rail'))
    fireEvent.pointerLeave(screen.getByTestId('rail'))
    act(() => vi.advanceTimersByTime(500))
    expect(state()).toBe('hidden')
    hoverRail()
    expect(state()).toBe('peek')
  })

  it('stays while the pointer is over the rail or the flyout, and hides shortly after it leaves both', () => {
    mount()
    hoverRail()
    fireEvent.pointerMove(document, { clientX: 200 })
    act(() => vi.advanceTimersByTime(500))
    expect(state()).toBe('peek')
    // 移出去又马上回来：不收
    fireEvent.pointerMove(document, { clientX: 600 })
    act(() => vi.advanceTimersByTime(100))
    fireEvent.pointerMove(document, { clientX: 250 })
    act(() => vi.advanceTimersByTime(500))
    expect(state()).toBe('peek')
    fireEvent.pointerMove(document, { clientX: 600 })
    act(() => vi.advanceTimersByTime(300))
    expect(state()).toBe('hidden')
  })

  it('does not hide while a menu or dialog opened from it is open, or while the width is being dragged', () => {
    mount()
    hoverRail()
    const menu = document.createElement('div')
    menu.setAttribute('role', 'menu')
    document.body.appendChild(menu)
    fireEvent.pointerMove(document, { clientX: 600 })
    fireEvent.pointerDown(document.body, { clientX: 600 })
    fireEvent.keyDown(document, { key: 'Escape' })
    act(() => vi.advanceTimersByTime(500))
    expect(state()).toBe('peek')
    menu.remove()

    document.documentElement.dataset.resizing = '1'
    fireEvent.pointerMove(document, { clientX: 600 })
    act(() => vi.advanceTimersByTime(500))
    expect(state()).toBe('peek')
    delete document.documentElement.dataset.resizing

    fireEvent.pointerMove(document, { clientX: 600 })
    act(() => vi.advanceTimersByTime(300))
    expect(state()).toBe('hidden')
  })

  it('hides on a click outside or Esc', () => {
    mount()
    hoverRail()
    fireEvent.pointerDown(document.body, { clientX: 200 })
    expect(state()).toBe('peek')
    fireEvent.pointerDown(document.body, { clientX: 600 })
    expect(state()).toBe('hidden')
    hoverRail()
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(state()).toBe('hidden')
  })

  it('a tap on the empty part of the rail slides it out; its buttons do not', () => {
    mount()
    fireEvent.click(screen.getByRole('button', { name: 'search' }))
    expect(state()).toBe('hidden')
    fireEvent.click(screen.getByTestId('rail'))
    expect(state()).toBe('peek')
  })

  it('forgets the flyout once pinned, so unpinning later does not pop it out', () => {
    const { rerender } = mount()
    hoverRail()
    rerender(<Harness enabled={false} />)
    expect(state()).toBe('hidden')
    rerender(<Harness enabled />)
    expect(state()).toBe('hidden')
  })
})
