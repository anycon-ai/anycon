// 离线改动的入队合并和叠到界面上（纯函数）。每个场景：给定现有的待发改动 + 新的一条 → 期望留下什么。
import { describe, expect, it } from 'vitest'
import { todoItem } from '@/test/factories'
import { mergeUpdate, overlayOutbox, planEnqueue } from './outbox'
import type { OutboxOp, OutboxRecord } from './types'

const N = 'n'.repeat(32)
const ID = '11111111-1111-4111-8111-111111111111'
const rec = (seq: number, op: OutboxOp, over: Partial<OutboxRecord> = {}): OutboxRecord => ({ seq, node_id: N, id: ID, op, created_at: seq * 10, state: 'pending', ...over })
const add = (title = 'a'): OutboxOp => ({ kind: 'add', params: { title, body: '' } })
const upd = (fields: Extract<OutboxOp, { kind: 'update' }>['fields']): OutboxOp => ({ kind: 'update', fields })

describe('mergeUpdate', () => {
  it('later fields overwrite earlier ones; clear and set cancel each other', () => {
    expect(mergeUpdate({ title: 'a', state: 'working' }, { title: 'b' })).toEqual({ title: 'b', state: 'working' })
    expect(mergeUpdate({ project: 'p' }, { clear: ['project'] })).toEqual({ clear: ['project'] })
    expect(mergeUpdate({ clear: ['due_ms'] }, { due_ms: 5 })).toEqual({ due_ms: 5 })
    expect(mergeUpdate({ title: 'a' }, { state: 'done', reviewed: true })).toEqual({ title: 'a', state: 'done', reviewed: true })
  })
})

describe('planEnqueue (scenario → plan)', () => {
  it('two edits of the same field offline: the second merges into the first update, one request on replay', () => {
    const p = planEnqueue(N, ID, [rec(1, upd({ title: 'one' }))], upd({ title: 'two', body: 'x' }), 99)
    expect(p.remove).toEqual([])
    expect(p.put).toEqual([rec(1, upd({ title: 'two', body: 'x' }))])
  })

  it('an update after a claimed (in-flight) update is appended, not merged into it', () => {
    const p = planEnqueue(N, ID, [rec(1, upd({ title: 'one' }), { claimed_at: 5 })], upd({ title: 'two' }), 99)
    expect(p.put).toHaveLength(1)
    expect(p.put[0]!.seq).toBeUndefined()
    expect(p.put[0]!.op).toEqual(upd({ title: 'two' }))
  })

  it('edits of the text fields while the add is still queued go into the add; a state change stays a separate update after it', () => {
    const p = planEnqueue(N, ID, [rec(1, add('old'))], upd({ title: 'new', state: 'working' }), 99)
    expect(p.put).toHaveLength(2)
    expect(p.put[0]).toMatchObject({ seq: 1, op: { kind: 'add', params: { title: 'new' } } })
    expect(p.put[1]!.op).toEqual(upd({ state: 'working' }))
  })

  it('editing an add that was rejected re-queues it (the user is fixing it)', () => {
    const failed = rec(1, add('bad'), { state: 'failed', error: { code: 'invalid', field: 'title' } })
    const p = planEnqueue(N, ID, [failed], upd({ title: 'fixed' }), 99)
    expect(p.put).toHaveLength(1)
    expect(p.put[0]).toMatchObject({ seq: 1, state: 'pending', op: { params: { title: 'fixed' } } })
    expect(p.put[0]!.error).toBeUndefined()
  })

  it('add then delete offline: the daemon never saw it, so everything for the item is cancelled and nothing is sent', () => {
    const p = planEnqueue(N, ID, [rec(1, add()), rec(2, upd({ state: 'done' }))], { kind: 'delete' }, 99)
    expect(p.remove).toEqual([1, 2])
    expect(p.put).toEqual([])
  })

  it('delete when the add is already in flight: pending updates are dropped and a delete is queued (not_found counts as done)', () => {
    const p = planEnqueue(N, ID, [rec(1, add(), { claimed_at: 5 }), rec(2, upd({ title: 'x' }))], { kind: 'delete' }, 99)
    expect(p.remove).toEqual([2])
    expect(p.put).toHaveLength(1)
    expect(p.put[0]!.op).toEqual({ kind: 'delete' })
  })

  it('delete of an item the daemon already has queues just the delete', () => {
    const p = planEnqueue(N, ID, [], { kind: 'delete' }, 99)
    expect(p).toMatchObject({ remove: [], put: [{ op: { kind: 'delete' }, state: 'pending' }] })
  })
})

describe('overlayOutbox', () => {
  const base = new Map([[ID, todoItem({ id: ID, title: 'from daemon', updated_ms: 100 })]])

  it('shows local adds, applies updates in order, hides locally deleted items, and marks them unsynced', () => {
    const view = overlayOutbox(N, base, [rec(1, upd({ title: 'edited', state: 'done' })), rec(2, { kind: 'add', params: { title: 'new one', body: 'b', project: 'p' } }, { id: 'other' })])
    expect(view.get(ID)).toMatchObject({ title: 'edited', state: 'done', unsynced: true })
    expect(view.get(ID)!.closed_ms).toBeDefined()
    expect(view.get('other')).toMatchObject({ title: 'new one', project: 'p', state: 'open', unsynced: true, source: { kind: 'device' } })
    expect(overlayOutbox(N, base, [rec(1, { kind: 'delete' })]).has(ID)).toBe(false)
  })

  it('reopening clears closed_ms; a human edit clears unreviewed', () => {
    const closed = new Map([[ID, todoItem({ id: ID, state: 'done', closed_ms: 5, unreviewed: true, source: { kind: 'agent' } })]])
    const v = overlayOutbox(N, closed, [rec(1, upd({ state: 'open' }))]).get(ID)!
    expect(v.state).toBe('open')
    expect(v.closed_ms).toBeUndefined()
    expect(v.unreviewed).toBe(false)
  })

  it('a rejected change is visible as failed and keeps the item on screen', () => {
    const v = overlayOutbox(N, new Map(), [rec(1, add('kept'), { state: 'failed', error: { code: 'invalid', reason: 'too_many' } })]).get(ID)!
    expect(v).toMatchObject({ title: 'kept', failed: { reason: 'too_many' }, unsynced: true })
  })

  it('an add whose item the daemon already has (replay cut off after the reply) keeps the daemon version, marked unsynced', () => {
    expect(overlayOutbox(N, base, [rec(1, add('local title'))]).get(ID)).toMatchObject({ title: 'from daemon', unsynced: true })
  })

  it('updates for an item that is not in the cache (and has no add) show nothing', () => {
    expect(overlayOutbox(N, new Map(), [rec(1, upd({ title: 'x' }))]).size).toBe(0)
  })

  it('only applies the ops of its own node', () => {
    expect(overlayOutbox(N, base, [rec(1, upd({ title: 'x' }), { node_id: 'other-node' })]).get(ID)).toMatchObject({ title: 'from daemon', unsynced: false })
  })
})
