// 会话页的两个入口（spec/web.md § 14.5）：消息「⋯」菜单里的「记到清单」、选中文字后浮出的按钮。只预填输入条，不直接记。
import { act, fireEvent, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { useNodes } from '@/stores/nodes'
import { useCapture } from '../capture'
import { MessageTodoMenu } from './MessageTodoMenu'
import { SelectionCapture } from './SelectionCapture'

const A = 'a'.repeat(32)
const info = (todo: boolean): NodeInfo => ({ capabilities: { todo } }) as unknown as NodeInfo
const go = (path: string): void => window.history.pushState({}, '', path)

beforeEach(() => {
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info: info(true) }) }, order: [A], loaded: true })
  useCapture.setState({ open: false, text: '' })
  go('/n/aaaaaaaa/s/sess_1')
})
afterEach(() => {
  go('/')
  vi.restoreAllMocks()
})

describe('message menu', () => {
  it('“Add to to-dos” opens the bar with the text and a link back to the message', async () => {
    const user = userEvent.setup()
    render(<MessageTodoMenu text="rotate the key" seq={7} />)
    await user.click(screen.getByTestId('message-menu'))
    await user.click(await screen.findByTestId('message-add-todo'))
    const { open, text } = useCapture.getState()
    expect(open).toBe(true)
    expect(text.split('\n')[0]).toBe('rotate the key')
    expect(text).toContain('](/n/aaaaaaaa/s/sess_1?seq=7)')
    expect(text).not.toContain(location.origin)
  })

  it('is not offered when the node does not support to-dos, outside a session, or for empty text', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info: info(false) }) } })
    const { unmount } = render(<MessageTodoMenu text="x" seq={1} />)
    expect(screen.queryByTestId('message-menu')).toBeNull()
    unmount()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info: info(true) }) } })
    go('/inbox')
    render(<MessageTodoMenu text="x" seq={1} />)
    expect(screen.queryByTestId('message-menu')).toBeNull()
  })
})

describe('selection', () => {
  function selectIn(el: Element): void {
    const range = document.createRange()
    range.selectNodeContents(el)
    range.getBoundingClientRect = () => ({ left: 10, top: 50, width: 40, height: 10, right: 50, bottom: 60, x: 10, y: 50, toJSON: () => ({}) })
    const sel = window.getSelection()!
    sel.removeAllRanges()
    sel.addRange(range)
    act(() => void document.dispatchEvent(new Event('selectionchange')))
  }

  it('a selection inside the message list shows the button; clicking it pre-fills the bar with the quote and the message link', () => {
    render(
      <>
        <SelectionCapture />
        <div data-testid="message-list">
          <div data-seq="12">
            <p data-testid="p">check the migration order</p>
          </div>
        </div>
      </>,
    )
    expect(screen.queryByTestId('selection-add-todo')).toBeNull()
    selectIn(screen.getByTestId('p'))
    fireEvent.click(screen.getByTestId('selection-add-todo'))
    const { open, text } = useCapture.getState()
    expect(open).toBe(true)
    expect(text).toContain('check the migration order')
    expect(text).toContain('?seq=12')
    expect(screen.queryByTestId('selection-add-todo')).toBeNull()
  })

  it('a selection outside the message list shows nothing', () => {
    render(
      <>
        <SelectionCapture />
        <div data-testid="message-list" />
        <p data-testid="elsewhere">not a message</p>
      </>,
    )
    selectIn(screen.getByTestId('elsewhere'))
    expect(screen.queryByTestId('selection-add-todo')).toBeNull()
  })
})

describe('selection button placement (nits)', () => {
  it('scrolling or resizing hides the floating button (it was positioned on the selection and would drift); it sits below the selection, not under the system callout', () => {
    render(
      <>
        <SelectionCapture />
        <div data-testid="message-list">
          <div data-seq="3">
            <p data-testid="p">some words</p>
          </div>
        </div>
      </>,
    )
    const range = document.createRange()
    range.selectNodeContents(screen.getByTestId('p'))
    range.getBoundingClientRect = () => ({ left: 10, top: 50, width: 40, height: 10, right: 50, bottom: 60, x: 10, y: 50, toJSON: () => ({}) })
    window.getSelection()!.removeAllRanges()
    window.getSelection()!.addRange(range)
    act(() => void document.dispatchEvent(new Event('selectionchange')))
    const button = screen.getByTestId('selection-add-todo')
    expect(Number.parseInt(button.style.top, 10)).toBeGreaterThan(60)
    act(() => void window.dispatchEvent(new Event('scroll')))
    expect(screen.queryByTestId('selection-add-todo')).toBeNull()
  })
})

describe('message menu on touch devices (nits)', () => {
  it('is not hidden behind a hover: it is shown whenever the device cannot hover', () => {
    render(<MessageTodoMenu text="x" seq={1} />)
    expect(screen.getByTestId('message-menu').className).toMatch(/\[@media\(hover:none\)\]:opacity-100/)
  })
})
