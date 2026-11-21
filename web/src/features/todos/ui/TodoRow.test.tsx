// 清单行的触控行为（spec/web.md § 4.12）：长按弹菜单、抬手后的 click 不再「打开这一项」、多选时不滑动不长按、行不可选中文字。
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { todoItem } from '@/test/factories'
import { LONG_PRESS_MS } from '../swipe'
import type { ViewTodo } from '../types'
import { TodoRow } from './TodoRow'

const item = (): ViewTodo => ({ ...todoItem({ id: 'a', title: 'row' }), node_id: 'n1', unsynced: false })
const handlers = () => ({ onOpen: vi.fn(), onToggleCheck: vi.fn(), onDone: vi.fn(), onReopen: vi.fn(), onDrop: vi.fn(), onDelete: vi.fn() })
const touch = (el: Element, type: 'Down' | 'Move' | 'Up', x: number, y = 0) => fireEvent[`pointer${type}` as 'pointerDown'](el, { pointerId: 1, pointerType: 'touch', clientX: x, clientY: y })

beforeEach(() => vi.useFakeTimers())
afterEach(() => vi.useRealTimers())

describe('TodoRow touch behaviour', () => {
  it('a long press opens the action menu, and the click that follows the lift does not also open the item', () => {
    const h = handlers()
    render(<TodoRow item={item()} selected={false} multi={false} checked={false} {...h} />)
    const strip = screen.getByTestId('todo-main').parentElement!
    touch(strip, 'Down', 100)
    act(() => void vi.advanceTimersByTime(LONG_PRESS_MS + 20))
    touch(strip, 'Up', 100)
    fireEvent.click(screen.getByTestId('todo-main'))
    expect(h.onOpen).not.toHaveBeenCalled()
    expect(screen.getByRole('menu')).toBeInTheDocument()
  })

  it('an ordinary tap still opens the item', () => {
    const h = handlers()
    render(<TodoRow item={item()} selected={false} multi={false} checked={false} {...h} />)
    fireEvent.click(screen.getByTestId('todo-main'))
    expect(h.onOpen).toHaveBeenCalledTimes(1)
  })

  it('in multi-select mode a swipe or a long press does nothing (the row is a checkbox); a tap toggles it', () => {
    const h = handlers()
    render(<TodoRow item={item()} selected={false} multi checked={false} {...h} />)
    const strip = screen.getByTestId('todo-main').parentElement!
    touch(strip, 'Down', 300)
    touch(strip, 'Move', 100)
    touch(strip, 'Up', 100)
    expect(h.onDone).not.toHaveBeenCalled()
    touch(strip, 'Down', 100)
    act(() => void vi.advanceTimersByTime(LONG_PRESS_MS + 20))
    expect(screen.queryByRole('menu')).toBeNull()
    fireEvent.click(screen.getByTestId('todo-main'))
    expect(h.onToggleCheck).toHaveBeenCalledTimes(1)
    expect(h.onOpen).not.toHaveBeenCalled()
  })

  it('the row does not let the text be selected (select-none)', () => {
    render(<TodoRow item={item()} selected={false} multi={false} checked={false} {...handlers()} />)
    expect(screen.getByTestId('todo-row').className).toMatch(/select-none/)
  })

  it('a row with a half-written edit carries a visible chip', () => {
    render(<TodoRow item={item()} selected={false} hasDraft multi={false} checked={false} {...handlers()} />)
    expect(screen.getByTestId('todo-draft')).toHaveTextContent('Unsaved edit')
  })
})

describe('the source chip', () => {
  const withSource = (kind: string): ViewTodo => ({ ...item(), source: { kind } as never })

  it('an agent item has the agent chip; a source this version does not know is shown as added automatically; a person’s item has neither', () => {
    vi.useRealTimers()
    const h = handlers()
    const { rerender } = render(<TodoRow item={withSource('agent')} selected={false} multi={false} checked={false} {...h} />)
    expect(screen.getByTestId('todo-agent')).toBeInTheDocument()
    expect(screen.queryByTestId('todo-auto')).toBeNull()
    rerender(<TodoRow item={withSource('twin')} selected={false} multi={false} checked={false} {...h} />)
    expect(screen.queryByTestId('todo-agent')).toBeNull()
    expect(screen.getByTestId('todo-auto')).toHaveTextContent('Added automatically')
    rerender(<TodoRow item={withSource('device')} selected={false} multi={false} checked={false} {...h} />)
    expect(screen.queryByTestId('todo-agent')).toBeNull()
    expect(screen.queryByTestId('todo-auto')).toBeNull()
  })
})
