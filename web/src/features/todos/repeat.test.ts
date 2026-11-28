// 周期项的纯函数（spec/web.md § 14.6）：规则 ⇄ 草稿里的字符串、校验、清单页的分组、导航上的数字、待发队列的叠加。
import { describe, expect, it } from 'vitest'
import { todoItem } from '@/test/factories'
import { flatOrder, groupTodos, openCount } from './list'
import { overlayItem } from './outbox'
import { formOf, inputOf, withStateFlags, repeatError, repeatFromInput, repeatInputError, repeatInputProblem, repeatTemplateOf, repeatToInput } from './repeat'
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
    const single = { every: 0, until: '18:00', autoClose: false, keepState: false, stateHintOff: false }
    expect(formOf('weekly:3@10:15')).toEqual({ choice: 'weekly', weekday: 3, at: '10:15', ...single })
    expect(inputOf({ choice: 'weekly', weekday: 2, at: '08:00', ...single })).toBe('weekly:2@08:00')
    expect(inputOf({ choice: 'none', weekday: 2, at: '08:00', ...single })).toBe('')
  })

  it('round-trips an interval rule and auto-close, and sends only the fields that apply', () => {
    const interval = { kind: 'interval', days: 'weekdays', at: '09:00', every_min: 120, until: '18:00' } as const
    expect(repeatToInput(interval)).toBe('weekdays@09:00-18:00/120')
    expect(repeatToInput({ ...interval, days: 'weekly', weekday: 2, auto_close: true })).toBe('weekly:2@09:00-18:00/120#close')
    expect(repeatToInput({ kind: 'daily', at: '09:00', auto_close: true })).toBe('daily@09:00#close')
    for (const r of [interval, { ...interval, days: 'weekly', weekday: 6, auto_close: true }, { kind: 'weekly', at: '18:00', weekday: 5, auto_close: true }, { kind: 'daily', at: '09:00' }] as const) {
      expect(repeatFromInput(repeatToInput(r))).toEqual(r)
    }
    expect(formOf('weekdays@09:00-17:30/90#close')).toEqual({ choice: 'weekdays', weekday: 1, at: '09:00', every: 90, until: '17:30', autoClose: true, keepState: false, stateHintOff: false })
    // 只选「每隔」、没动「到」：缺省 18:00
    expect(inputOf({ ...formOf('daily@09:00'), every: 60 })).toBe('daily@09:00-18:00/60')
    // 间隔改回「只一次」：不再带「到」和间隔
    expect(inputOf({ ...formOf('daily@09:00-18:00/60#close'), every: 0 })).toBe('daily@09:00#close')
    // 不认识的天（更新的节点给的）读成没有规则
    expect(repeatToInput({ kind: 'interval', days: 'unknown', at: '09:00', every_min: 60, until: '18:00' })).toBe('')
  })

  it('carries the cross-run state switches through the draft string, so editing only the time keeps them', () => {
    const r = { kind: 'interval', days: 'weekdays', at: '09:00', every_min: 60, until: '18:00', auto_close: true, keep_state: true, state_hint_off: true } as const
    expect(repeatToInput(r)).toBe('weekdays@09:00-18:00/60#close#state#nohint')
    expect(repeatFromInput(repeatToInput(r))).toEqual(r)
    expect(repeatFromInput(repeatToInput({ kind: 'daily', at: '09:00', keep_state: true }))).toEqual({ kind: 'daily', at: '09:00', keep_state: true })
    // 只改了时间：两个开关原样带着
    const edited = inputOf({ ...formOf(repeatToInput(r)), at: '10:00' })
    expect(repeatFromInput(edited)).toEqual({ ...r, at: '10:00' })
    // 「不附带用法说明」只跟在跨次状态后面：单独出现（不该有的草稿）不发
    expect(repeatFromInput('daily@09:00#nohint')).toEqual({ kind: 'daily', at: '09:00' })
    // 关掉跨次状态时一起关掉；不认识的开关忽略
    expect(withStateFlags('daily@09:00#state#nohint', { keepState: false })).toBe('daily@09:00')
    expect(withStateFlags('daily@09:00#close', { keepState: true })).toBe('daily@09:00#close#state')
    expect(withStateFlags('daily@09:00#state', { stateHintOff: true })).toBe('daily@09:00#state#nohint')
    expect(withStateFlags('', { keepState: true })).toBe('')
    expect(formOf('daily@09:00#later#state')).toMatchObject({ keepState: true, autoClose: false })
  })

  it('checks an interval like the node does: the end is later than the start, the interval is 15 to 720 minutes', () => {
    expect(repeatInputError('daily@09:00-18:00/120')).toBe(false)
    expect(repeatInputProblem('daily@09:00-09:00/120')).toBe('until')
    expect(repeatInputProblem('daily@18:00-09:00/120')).toBe('until')
    expect(repeatInputProblem('daily@09:00-/120')).toBe('until')
    expect(repeatInputProblem('daily@09:00-18:00/14')).toBe('every')
    expect(repeatInputProblem('daily@09:00-18:00/721')).toBe('every')
    expect(repeatInputProblem('daily@9:00-18:00/60')).toBe('at')
    expect(repeatError({ kind: 'interval', days: 'daily', at: '09:00', every_min: 60, until: '18:00' })).toBe(false)
    expect(repeatError({ kind: 'interval', at: '09:00', every_min: 60, until: '18:00' })).toBe(true)
    expect(repeatError({ kind: 'interval', days: 'weekly', at: '09:00', every_min: 60, until: '18:00' })).toBe(true)
    expect(repeatError({ kind: 'interval', days: 'daily', at: '09:00', every_min: 60, until: '08:00' })).toBe(true)
    expect(repeatError({ kind: 'daily', at: '09:00', every_min: 60 })).toBe(true)
    expect(draftError({ title: 't', body: '', repeat: { kind: 'interval', days: 'daily', at: '09:00', every_min: 5, until: '18:00' } })).toEqual({ field: 'repeat', reason: 'invalid' })
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
