import { describe, expect, it } from 'vitest'
import { todoItem } from '@/test/factories'
import { flatOrder, groupTodos, inList, openCount, projectsOf } from './list'
import { LONG_PRESS_MS, SWIPE_DISTANCE, swipeOffset, swipeVerdict } from './swipe'
import type { ViewTodo } from './types'

const v = (over: Parameters<typeof todoItem>[0]): ViewTodo => ({ ...todoItem(over), node_id: 'n1', unsynced: false })

describe('grouping', () => {
  it('working (plus waiting / review written by other clients), not started, closed; unreviewed agent items are not on the list', () => {
    const g = groupTodos([
      v({ id: 'a', state: 'working' }),
      v({ id: 'b', state: 'waiting' }),
      v({ id: 'c', state: 'review' }),
      v({ id: 'd', state: 'open' }),
      v({ id: 'e', state: 'done' }),
      v({ id: 'f', state: 'dropped' }),
      v({ id: 'agent', state: 'open', source: { kind: 'agent' }, unreviewed: true }),
    ])
    expect(g.working.map((t) => t.id).sort()).toEqual(['a', 'b', 'c'])
    expect(g.open.map((t) => t.id)).toEqual(['d'])
    expect(g.closed.map((t) => t.id).sort()).toEqual(['e', 'f'])
    expect(inList(v({ id: 'x', unreviewed: true }))).toBe(false)
  })

  it('an accepted agent item (reviewed) is on the list like any other', () => {
    expect(groupTodos([v({ id: 'agent', source: { kind: 'agent' }, unreviewed: false })]).open).toHaveLength(1)
  })

  it('orders by due time (earliest first, items without a due time after), then by most recent update', () => {
    const g = groupTodos([v({ id: 'old', updated_ms: 1 }), v({ id: 'new', updated_ms: 9 }), v({ id: 'late', due_ms: 500, updated_ms: 1 }), v({ id: 'soon', due_ms: 100, updated_ms: 1 })])
    expect(g.open.map((t) => t.id)).toEqual(['soon', 'late', 'new', 'old'])
  })
})

describe('filters', () => {
  const items = [v({ id: 'a', project: 'api' }), v({ id: 'b', project: 'web' }), v({ id: 'c' }), v({ id: 'd', project: 'api', source: { kind: 'agent' } })]

  it('by project (exact) and by source (mine = not agent)', () => {
    expect(groupTodos(items, { project: 'api', source: 'all' }).open.map((t) => t.id).sort()).toEqual(['a', 'd'])
    expect(groupTodos(items, { project: null, source: 'agent' }).open.map((t) => t.id)).toEqual(['d'])
    expect(groupTodos(items, { project: null, source: 'mine' }).open.map((t) => t.id).sort()).toEqual(['a', 'b', 'c'])
    expect(groupTodos(items, { project: 'api', source: 'mine' }).open.map((t) => t.id)).toEqual(['a'])
  })

  it('the project choices are the ones in use, sorted', () => {
    expect(projectsOf(items)).toEqual(['api', 'web'])
  })
})

describe('counts and keyboard order', () => {
  it('the nav count is the open (not closed) items on the list: not closed ones, not unreviewed ones', () => {
    expect(openCount([v({ id: 'a' }), v({ id: 'b', state: 'working' }), v({ id: 'c', state: 'done' }), v({ id: 'd', unreviewed: true, source: { kind: 'agent' } })])).toBe(2)
  })

  it('the flat order for J / K is working, not started, then closed only when expanded', () => {
    const g = groupTodos([v({ id: 'w', state: 'working' }), v({ id: 'o' }), v({ id: 'c', state: 'done' })])
    expect(flatOrder(g, false).map((t) => t.id)).toEqual(['w', 'o'])
    expect(flatOrder(g, true).map((t) => t.id)).toEqual(['w', 'o', 'c'])
  })
})

describe('swipe', () => {
  it('only a far, mostly horizontal, leftward touch drag is “done”; a mouse never swipes', () => {
    expect(swipeVerdict(-SWIPE_DISTANCE - 1, 3, 'touch')).toBe('done')
    expect(swipeVerdict(-SWIPE_DISTANCE + 5, 0, 'touch')).toBe('none')
    expect(swipeVerdict(SWIPE_DISTANCE + 40, 0, 'touch')).toBe('none')
    expect(swipeVerdict(-120, 100, 'touch')).toBe('none') // 竖着滚动
    expect(swipeVerdict(-200, 0, 'mouse')).toBe('none')
    expect(swipeVerdict(-200, 0, 'pen')).toBe('none')
  })

  it('the row follows the finger only to the left, only when the drag is horizontal', () => {
    expect(swipeOffset(-40, 2)).toBe(-40)
    expect(swipeOffset(-400, 0)).toBeLessThan(-SWIPE_DISTANCE)
    expect(swipeOffset(30, 0)).toBe(0)
    expect(swipeOffset(-20, 80)).toBe(0)
    expect(LONG_PRESS_MS).toBeGreaterThan(300)
  })
})

describe('an item with a source kind this version does not know', () => {
  const unknown = v({ id: 'u', source: { kind: 'twin' } as never })
  const items = [v({ id: 'a' }), unknown]

  it('is not “mine”, is not an agent’s, and still shows under “all”', () => {
    expect(groupTodos(items, { project: null, source: 'mine' }).open.map((t) => t.id)).toEqual(['a'])
    expect(groupTodos(items, { project: null, source: 'agent' }).open.map((t) => t.id)).toEqual([])
    expect(groupTodos(items, { project: null, source: 'all' }).open.map((t) => t.id).sort()).toEqual(['a', 'u'])
  })
})
