// 搜索命中的定位（spec/web.md § 9）：消息流滚到 seq 所在的那条消息并短暂高亮；seq 可以落在一个块的中间。
import { act, render, screen } from '@testing-library/react'
import type * as ReactVirtual from '@tanstack/react-virtual'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { evs, resetSeq } from '@/test/factories'
import { buildView } from '../reducer'
import { focusRowIndex, MessageList, VIRTUALIZE_THRESHOLD, type MessageListProps } from './MessageList'

const scrollToIndex = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-virtual', async (orig) => {
  const actual = await orig<typeof ReactVirtual>()
  return {
    ...actual,
    useVirtualizer: (...args: Parameters<typeof actual.useVirtualizer>) => {
      const v = actual.useVirtualizer(...args)
      v.scrollToIndex = scrollToIndex
      return v
    },
  }
})

function props(over: Partial<MessageListProps> = {}): MessageListProps {
  return {
    blocks: [],
    pending: [],
    hasMoreOlder: false,
    loadingOlder: false,
    onLoadOlder: () => {},
    deviceLabel: (id) => id,
    respondingIds: new Set(),
    onRespond: () => {},
    onRetryPending: () => {},
    onEditPending: () => {},
    onDeletePending: () => {},
    modelLabel: (id) => id,
    modeLabel: (id) => id,
    ...over,
  }
}

/** seq：1 用户消息、2 turn_start、3 4 5 一段 assistant 文字（块起点 3）、6 turn_done、7 用户消息、8 turn_start、9 assistant 文字。 */
function conversation() {
  resetSeq()
  return buildView(
    evs([
      { type: 'user_message', message_id: 'm1', content: 'first question', by_device: 'me' },
      { type: 'turn_start', turn_id: 1, message_id: 'm1' },
      { type: 'text_delta', text: 'The answer ' },
      { type: 'text_delta', text: 'is in ' },
      { type: 'text_delta', text: 'the parser.' },
      { type: 'turn_done', turn_id: 1, reason: 'completed' },
      { type: 'user_message', message_id: 'm2', content: 'second question', by_device: 'me' },
      { type: 'turn_start', turn_id: 2, message_id: 'm2' },
      { type: 'text_delta', text: 'Second answer.' },
    ]),
  ).blocks
}

const rowOf = (text: string): HTMLElement => screen.getByText(text).closest('[data-index]') as HTMLElement

describe('focusRowIndex', () => {
  const rows = [{ seq: 1 }, { seq: 2 }, { seq: 3 }, {}, { seq: 7 }]
  it('picks the last row that starts at or before the seq, so a seq inside a block lands on that block', () => {
    expect(focusRowIndex(rows, 1)).toBe(0)
    expect(focusRowIndex(rows, 3)).toBe(2)
    expect(focusRowIndex(rows, 5)).toBe(2)
    expect(focusRowIndex(rows, 7)).toBe(4)
    expect(focusRowIndex(rows, 999)).toBe(4)
  })
  it('finds nothing when the seq is older than everything loaded (or no row has a seq)', () => {
    expect(focusRowIndex([{ seq: 5 }, { seq: 6 }], 4)).toBe(-1)
    expect(focusRowIndex([{}, {}], 4)).toBe(-1)
    expect(focusRowIndex([], 4)).toBe(-1)
  })
  it('does not depend on the order of the rows', () => {
    expect(focusRowIndex([{ seq: 9 }, { seq: 4 }, { seq: 6 }], 7)).toBe(2)
  })
})

describe('MessageList — focus on a search hit', () => {
  const scrolled = vi.fn()
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    scrollToIndex.mockReset()
    scrolled.mockReset()
    vi.spyOn(Element.prototype, 'scrollIntoView').mockImplementation(function (this: Element) {
      scrolled(this)
    })
  })
  afterEach(() => vi.useRealTimers())

  it('scrolls the message into the middle of the view, highlights it for a moment, and reports that the request was handled', () => {
    const onFocused = vi.fn()
    render(<MessageList {...props({ blocks: conversation(), focusSeq: 7, onFocused })} />)
    // seq 7 是第二条用户消息
    expect(scrolled).toHaveBeenCalledTimes(1)
    expect(scrolled.mock.calls[0]![0]).toBe(rowOf('second question'))
    expect(Element.prototype.scrollIntoView).toHaveBeenCalledWith({ block: 'center' })
    expect(rowOf('second question')).toHaveClass('search-flash')
    expect(rowOf('first question')).not.toHaveClass('search-flash')
    expect(onFocused).toHaveBeenCalledTimes(1)
    // 过一会儿高亮自己撤掉
    act(() => void vi.advanceTimersByTime(2300))
    expect(rowOf('second question')).not.toHaveClass('search-flash')
  })

  it('a seq in the middle of an assistant block (long text is indexed in pieces) lands on that block', () => {
    render(<MessageList {...props({ blocks: conversation(), focusSeq: 5 })} />)
    expect(scrolled.mock.calls[0]![0]).toBe(rowOf('The answer is in the parser.'))
    expect(rowOf('The answer is in the parser.')).toHaveClass('search-flash')
  })

  it('does not scroll or highlight anything when the seq is not among the loaded messages, but still reports it', () => {
    const blocks = conversation().slice(2) // 去掉前两个块：最早的块从 seq 3 开始
    const onFocused = vi.fn()
    render(<MessageList {...props({ blocks, focusSeq: 1, onFocused })} />)
    expect(scrolled).not.toHaveBeenCalled()
    expect(document.querySelector('.search-flash')).toBeNull()
    expect(onFocused).toHaveBeenCalledTimes(1)
  })

  it('handles a request once: re-rendering with the same seq does not scroll again', () => {
    const blocks = conversation()
    const { rerender } = render(<MessageList {...props({ blocks, focusSeq: 7 })} />)
    rerender(<MessageList {...props({ blocks, focusSeq: 7, hasMoreOlder: true })} />)
    expect(scrolled).toHaveBeenCalledTimes(1)
    // 清掉之后再给同一个 seq 就是新的一次请求
    rerender(<MessageList {...props({ blocks, focusSeq: null })} />)
    rerender(<MessageList {...props({ blocks, focusSeq: 7 })} />)
    expect(scrolled).toHaveBeenCalledTimes(2)
  })

  it('without a focus request it does not scroll to anything', () => {
    render(<MessageList {...props({ blocks: conversation() })} />)
    expect(scrolled).not.toHaveBeenCalled()
    expect(document.querySelector('.search-flash')).toBeNull()
  })

  it('shows a busy overlay while the history is being loaded for the target', () => {
    const { rerender } = render(<MessageList {...props({ blocks: conversation(), finding: true })} />)
    expect(screen.getByTestId('message-list-finding')).toHaveTextContent('Loading the message…')
    rerender(<MessageList {...props({ blocks: conversation(), finding: false })} />)
    expect(screen.queryByTestId('message-list-finding')).toBeNull()
  })

  it('in a long (virtualized) conversation it asks the virtualizer to scroll to the row, since that row may not be rendered yet', () => {
    resetSeq()
    const blocks = buildView(evs(Array.from({ length: VIRTUALIZE_THRESHOLD + 40 }, (_, i) => ({ type: 'user_message' as const, message_id: `m${i}`, content: `msg ${i}`, by_device: 'me' })))).blocks
    render(<MessageList {...props({ blocks, focusSeq: 100 })} />)
    expect(scrollToIndex).toHaveBeenCalledWith(99, { align: 'center' })
    expect(scrolled).not.toHaveBeenCalled()
  })
})
