// 派发之后的分组、谁在处理、「经过」的合并（spec/web.md § 4.12）。`activity` 只读 daemon 给的，不认识的值当作没有。
import { beforeEach, describe, expect, it } from 'vitest'
import { todoItem } from '@/test/factories'
import { activityOf, handlerOf, isReview, listGroupOf } from './activity'
import { flatOrder, groupTodos, NO_FILTERS } from './list'
import { handleTodoEvent, historyKey, latestResult, mergeEvents, useTodoHistory } from './history'
import type { ViewTodo } from './types'

const v = (over: Parameters<typeof todoItem>[0] & { activity?: unknown }): ViewTodo => ({ ...todoItem(over as never), node_id: 'n', unsynced: false }) as ViewTodo

describe('activity and the list groups', () => {
  it('the five groups, in order: waiting, review, working, not started, closed', () => {
    const items = [
      v({ id: 'open' }),
      v({ id: 'w', activity: 'working' }),
      v({ id: 'r', activity: 'review' }),
      v({ id: 'd', activity: 'waiting' }),
      v({ id: 'c', state: 'done' }),
      v({ id: 'sw', state: 'working' }),
    ]
    const g = groupTodos(items)
    expect(g.decide.map((t) => t.id)).toEqual(['d'])
    expect(g.review.map((t) => t.id)).toEqual(['r'])
    expect(g.working.map((t) => t.id).sort()).toEqual(['sw', 'w'])
    expect(g.open.map((t) => t.id)).toEqual(['open'])
    expect(g.closed.map((t) => t.id)).toEqual(['c'])
    expect(flatOrder(g, true).map((t) => t.id)).toEqual(['d', 'r', ...g.working.map((t) => t.id), 'open', 'c'])
  })

  it('an unknown activity is shown as no activity: an open item stays not started, a person’s working item stays in progress', () => {
    expect(activityOf({ activity: 'brand-new' as never, state: 'open' })).toBeNull()
    expect(activityOf({ activity: 'unknown', state: 'open' })).toBeNull()
    expect(listGroupOf({ activity: 'brand-new' as never, state: 'open' })).toBe('open')
    expect(listGroupOf({ activity: 'brand-new' as never, state: 'working' })).toBe('working')
    expect(handlerOf({ activity: 'brand-new' as never, state: 'working' })).toBe('me')
  })

  it('a closed item has no activity even if a stale value is attached', () => {
    expect(activityOf({ activity: 'review', state: 'done' })).toBeNull()
    expect(listGroupOf({ activity: 'working', state: 'dropped' })).toBe('closed')
    expect(isReview({ activity: 'review', state: 'dropped' })).toBe(false)
  })

  it('states written by other clients (waiting / review) without an activity stay in “in progress”', () => {
    expect(listGroupOf({ state: 'waiting' })).toBe('working')
    expect(listGroupOf({ state: 'review' })).toBe('working')
  })

  it('who is handling: the agent only while it is working; everything else is in your hands', () => {
    expect(handlerOf({ activity: 'working', state: 'open' })).toBe('agent')
    expect(handlerOf({ activity: 'waiting', state: 'open' })).toBe('me')
    expect(handlerOf({ activity: 'review', state: 'open' })).toBe('me')
    expect(handlerOf({ state: 'open' })).toBe('me')
    const items = [v({ id: 'a', activity: 'working' }), v({ id: 'b', activity: 'review' }), v({ id: 'c' })]
    expect(groupTodos(items, { ...NO_FILTERS, handler: 'agent' }).working.map((t) => t.id)).toEqual(['a'])
    const me = groupTodos(items, { ...NO_FILTERS, handler: 'me' })
    expect([me.working.length, me.review.map((t) => t.id), me.open.map((t) => t.id)]).toEqual([0, ['b'], ['c']])
  })
})

describe('the history store', () => {
  beforeEach(() => useTodoHistory.getState().reset())
  const ev = (seq: number, text = ''): never => ({ seq, ts_ms: seq * 10, kind: 'result', session_id: 's', turn: seq, text, truncated: false }) as never

  it('merges pages and notifications by seq, newest first, without duplicates', () => {
    expect(mergeEvents([ev(3), ev(2)], [ev(4), ev(2, 'again'), ev(1)]).map((e) => [e.seq, (e as { text?: string }).text])).toEqual([[4, ''], [3, ''], [2, 'again'], [1, '']])
  })

  it('a notification is only appended to an item that is being watched; a malformed one is dropped', () => {
    const key = historyKey('n', 't')
    handleTodoEvent('n', { todo_id: 't', event: ev(1) })
    expect(useTodoHistory.getState().byKey[key]).toBeUndefined()
    useTodoHistory.getState().patch(key, () => ({ loaded: true, events: [ev(1)] }))
    handleTodoEvent('n', { todo_id: 't', event: ev(2, 'new') })
    handleTodoEvent('n', { todo_id: 't', event: { seq: 'x' } })
    handleTodoEvent('n', { event: ev(9) })
    handleTodoEvent('n', null)
    expect(useTodoHistory.getState().byKey[key]!.events.map((e) => e.seq)).toEqual([2, 1])
  })

  it('the latest result is the newest result event', () => {
    const state = { seq: 5, ts_ms: 1, kind: 'state', from: 'open', to: 'done', by: { kind: 'device' } } as never
    expect(latestResult([state, ev(3, 'last'), ev(2, 'older')])?.text).toBe('last')
    expect(latestResult([state])).toBeUndefined()
  })
})
