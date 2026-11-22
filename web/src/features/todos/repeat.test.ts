// 周期项的纯函数（spec/web.md § 14.6）：规则 ⇄ 草稿里的字符串、校验、清单页的分组、导航上的数字、待发队列的叠加。
import { describe, expect, it } from 'vitest'
import { todoItem } from '@/test/factories'
import { flatOrder, groupTodos, openCount } from './list'
import { overlayItem } from './outbox'
import { formOf, inputOf, repeatError, repeatFromInput, repeatInputError, repeatTemplateOf, repeatToInput } from './repeat'
import { isOtherSource, isRepeatSource } from './source'
import type { OutboxRecord, ViewTodo } from './types'
import { draftError } from './validate'

const view = (over: Parameters<typeof todoItem>[0]): ViewTodo => ({ ...todoItem(over), node_id: 'n', unsynced: false })

describe('the rule as a draft string', () => {
  it('round-trips the three kinds and reads an empty string as "does not repeat"', () => {
    expect(repeatToInput(undefined)).toBe('')
    expect(repeatToInput({ kind: 'daily', at: '09:00' })).toBe('daily@09:00')
    expect(repeatToInput({ kind: 'weekdays', at: '07:30' })).toBe('weekdays@07:30')
    expect(repeatToInput({ kind: 'weekly', at: '18:00', weekday: 5 })).toBe('weekly:5@18:00')
    for (const r of [{ kind: 'daily', at: '09:00' }, { kind: 'weekdays', at: '07:30' }, { kind: 'weekly', at: '18:00', weekday: 5 }] as const) {
      expect(repeatFromInput(repeatToInput(r))).toEqual(r)
    }
    expect(repeatFromInput('')).toBeNull()
    expect(formOf('')).toMatchObject({ choice: 'none' })
    expect(formOf('weekly:3@10:15')).toEqual({ choice: 'weekly', weekday: 3, at: '10:15' })
    expect(inputOf({ choice: 'weekly', weekday: 2, at: '08:00' })).toBe('weekly:2@08:00')
    expect(inputOf({ choice: 'none', weekday: 2, at: '08:00' })).toBe('')
  })

  it('a kind this version does not know is left alone: it reads as no rule, so saving sends nothing for it', () => {
    expect(repeatToInput({ kind: 'unknown', at: '09:00' })).toBe('')
  })

  it('rejects a time that is not HH:MM and a weekly rule without a weekday, like the node does', () => {
    expect(repeatInputError('daily@09:00')).toBe(false)
    expect(repeatInputError('daily@')).toBe(true)
    expect(repeatInputError('daily@24:00')).toBe(true)
    expect(repeatInputError('')).toBe(false)
    expect(repeatError({ kind: 'weekly', at: '09:00' })).toBe(true)
    expect(repeatError({ kind: 'weekly', at: '09:00', weekday: 8 })).toBe(true)
    expect(repeatError({ kind: 'daily', at: '09:00', weekday: 1 })).toBe(true)
    expect(repeatError({ kind: 'weekly', at: '09:00', weekday: 7 })).toBe(false)
    expect(draftError({ title: 't', body: '', repeat: { kind: 'daily', at: '9:00' } })).toEqual({ field: 'repeat', reason: 'invalid' })
    expect(draftError({ title: 't', body: '', repeat: { kind: 'daily', at: '09:00' } })).toBeNull()
  })

  it('a copy points back at its template', () => {
    expect(repeatTemplateOf({ source: { kind: 'repeat', todo_id: 'tpl' } })).toBe('tpl')
    expect(repeatTemplateOf({ source: { kind: 'local' } })).toBeNull()
    expect(isRepeatSource({ kind: 'repeat' })).toBe(true)
    // 「来自周期项」有自己的标记，不算「自动记的」
    expect(isOtherSource({ kind: 'repeat' })).toBe(false)
    expect(isOtherSource({ kind: 'twin' })).toBe(true)
  })
})

describe('the list', () => {
  const tpl = (id: string, next: number | null, over: Partial<Parameters<typeof todoItem>[0]> = {}): ViewTodo =>
    view({ id, title: id, repeat: { kind: 'daily', at: '09:00', ...(next ? { next_ms: next } : {}) }, ...over })

  it('keeps the open templates in a group of their own, soonest next time first, out of "not started"', () => {
    const g = groupTodos([view({ id: 'plain' }), tpl('late', 900), tpl('soon', 100), tpl('unknown-next', null)])
    expect(g.repeat.map((t) => t.id)).toEqual(['soon', 'late', 'unknown-next'])
    expect(g.open.map((t) => t.id)).toEqual(['plain'])
  })

  it('a closed template goes to the closed group like any closed item', () => {
    const g = groupTodos([tpl('done-tpl', 100, { state: 'done' })])
    expect(g.repeat).toEqual([])
    expect(g.closed.map((t) => t.id)).toEqual(['done-tpl'])
  })

  it('the repeat group can be left out of the keyboard order when it is collapsed, and templates are not counted in the badge', () => {
    const items = [view({ id: 'plain' }), tpl('t', 100)]
    const g = groupTodos(items)
    expect(flatOrder(g, false).map((t) => t.id)).toEqual(['plain', 't'])
    expect(flatOrder(g, false, false).map((t) => t.id)).toEqual(['plain'])
    expect(openCount(items)).toBe(1)
  })
})

describe('the pending queue', () => {
  const rec = (seq: number, op: OutboxRecord['op']): OutboxRecord => ({ seq, node_id: 'n', id: 't', op, created_at: 100 + seq, state: 'pending' })

  it('shows a rule set offline and takes it away again with clear', () => {
    const base = todoItem({ id: 't' })
    const set = overlayItem('n', 't', base, [rec(1, { kind: 'update', fields: { repeat: { kind: 'weekly', at: '09:00', weekday: 1 } } })])
    expect(set?.repeat).toEqual({ kind: 'weekly', at: '09:00', weekday: 1 })
    expect(set?.unsynced).toBe(true)
    const cleared = overlayItem('n', 't', base, [rec(1, { kind: 'update', fields: { repeat: { kind: 'daily', at: '09:00' } } }), rec(2, { kind: 'update', fields: { clear: ['repeat'] } })])
    expect(cleared?.repeat).toBeUndefined()
  })

  it('a new item added offline with a rule shows it', () => {
    const v = overlayItem('n', 'new', undefined, [rec(1, { kind: 'add', params: { title: 'x', body: '', repeat: { kind: 'daily', at: '06:00' } } })])
    expect(v?.repeat).toEqual({ kind: 'daily', at: '06:00' })
  })
})
