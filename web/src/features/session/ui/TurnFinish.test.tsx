// turn 结束的收尾行（spec/web.md § 4.6）：用时、改了几个文件只写推得出来的；对勾只给用户看着时实时结束的那个 turn，
// 描边的 class 只在那一刻挂上。
import { render, screen } from '@testing-library/react'
import { beforeEach, describe, expect, it } from 'vitest'
import type { Event } from '@/protocol'
import { ev, evs, resetSeq } from '@/test/factories'
import { buildView, type TurnDoneBlock } from '../reducer'
import { MessageList, type MessageListProps } from './MessageList'

beforeEach(() => resetSeq())

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

const edit = (path: string, kind: 'created' | 'modified' | 'deleted' = 'modified'): Event => ({ type: 'file_changed', path, kind })
const done = (reason: 'completed' | 'cancelled' | 'error' = 'completed', duration_ms = 192_000): Event => ({ type: 'turn_done', turn_id: 1, reason, usage: { duration_ms, model: 'mock-large' } })
const doneBlock = (blocks: ReturnType<typeof buildView>['blocks']): TurnDoneBlock => blocks.find((b): b is TurnDoneBlock => b.type === 'turn_done')!

describe('files changed in a turn', () => {
  it('counts distinct paths from file_changed within the turn', () => {
    const view = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, edit('a.ts'), edit('b.ts', 'created'), edit('a.ts'), done()]))
    expect(doneBlock(view.blocks).filesChanged).toBe(2)
  })

  it('does not count when the turn started before the loaded window (earlier edits are not visible)', () => {
    const view = buildView([ev(edit('a.ts'), { turn_id: 1 }), ev(done(), { turn_id: 1 })])
    expect(doneBlock(view.blocks).filesChanged).toBeUndefined()
  })

  it('keeps turns apart', () => {
    const view = buildView(
      evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, edit('a.ts'), done(), { type: 'turn_start', turn_id: 2, message_id: 'm2' }, { type: 'turn_done', turn_id: 2, reason: 'completed', usage: { duration_ms: 1000 } }]),
    )
    const blocks = view.blocks.filter((b): b is TurnDoneBlock => b.type === 'turn_done')
    expect(blocks.map((b) => b.filesChanged)).toEqual([1, 0])
  })
})

describe('closing line', () => {
  it('states how long the turn took and how many files it changed', () => {
    const view = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, edit('a.ts'), edit('b.ts'), done()]))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    expect(screen.getByTestId('turn-facts')).toHaveTextContent('Took 3m 12s · 2 files changed')
    expect(screen.getByTestId('turn-usage')).toHaveTextContent('mock-large')
  })

  it('leaves out what it cannot tell: no duration reported, no file count outside the window, none for zero files', () => {
    const outside = buildView([ev(edit('a.ts'), { turn_id: 1 }), ev(done('completed', 0), { turn_id: 1 })])
    const { unmount } = render(<MessageList {...props({ blocks: outside.blocks })} />)
    expect(screen.queryByTestId('turn-facts')).toBeNull()
    unmount()
    const noEdits = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, done('completed', 45_000)]))
    render(<MessageList {...props({ blocks: noEdits.blocks })} />)
    expect(screen.getByTestId('turn-facts')).toHaveTextContent(/^Took 45s$/)
  })

  it('draws the check only for the turn that ended live, and the stroke animation only while `animate` is on', () => {
    const view = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, done()]))
    const key = doneBlock(view.blocks).key
    const { rerender } = render(<MessageList {...props({ blocks: view.blocks })} />)
    // 历史里的 turn：没有对勾
    expect(screen.queryByTestId('turn-check')).toBeNull()
    rerender(<MessageList {...props({ blocks: view.blocks, liveFinish: { key, animate: true } })} />)
    expect(screen.getByTestId('turn-check')).toHaveClass('moment-check')
    rerender(<MessageList {...props({ blocks: view.blocks, liveFinish: { key, animate: false } })} />)
    expect(screen.getByTestId('turn-check')).not.toHaveClass('moment-check')
  })

  it('shows no check on a stopped or failed turn, only the neutral line with its reason', () => {
    for (const reason of ['cancelled', 'error'] as const) {
      resetSeq()
      const view = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, done(reason)]))
      const { unmount } = render(<MessageList {...props({ blocks: view.blocks, liveFinish: { key: doneBlock(view.blocks).key, animate: true } })} />)
      expect(screen.queryByTestId('turn-check')).toBeNull()
      expect(screen.getByTestId('turn-usage')).toHaveTextContent(reason === 'cancelled' ? 'Stopped' : 'Ended with an error')
      expect(screen.getByTestId('turn-facts')).toHaveTextContent('Took 3m 12s')
      unmount()
    }
  })
})
