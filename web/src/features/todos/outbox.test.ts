// 待发队列叠到界面上的纯函数（队列本身是只追加的日志，不合并，见 outbox.ts 头部）。
import { describe, expect, it } from 'vitest'
import type { Todo } from '@/protocol'
import { todoItem } from '@/test/factories'
import { groupOf, overlayItem, overlayOutbox } from './outbox'
import { useTodos, viewOf } from './store'
import type { OutboxRecord } from './types'

const N = 'n1'
const rec = (seq: number, id: string, op: OutboxRecord['op'], over: Partial<OutboxRecord> = {}): OutboxRecord => ({ seq, node_id: N, id, op, created_at: 100 + seq, state: 'pending', ...over })
const base = (...ts: Todo[]): Map<string, Todo> => new Map(ts.map((t) => [t.id, t]))

describe('overlay', () => {
  it('applies the log in order on top of the daemon’s copy and marks the item unsynced', () => {
    const out = overlayOutbox(N, base(todoItem({ id: 'a', title: 'orig', project: 'p' })), [
      rec(2, 'a', { kind: 'update', fields: { title: 'second' } }),
      rec(1, 'a', { kind: 'update', fields: { title: 'first', clear: ['project'] } }),
    ])
    expect(out.get('a')).toMatchObject({ title: 'second', unsynced: true })
    expect(out.get('a')!.project).toBeUndefined()
  })

  it('an add that has no cached copy shows the new item; an add the daemon already has keeps the daemon’s version', () => {
    expect(overlayOutbox(N, base(), [rec(1, 'x', { kind: 'add', params: { title: 'new', body: 'b' } })]).get('x')).toMatchObject({ title: 'new', body: 'b', state: 'open', unsynced: true })
    expect(overlayOutbox(N, base(todoItem({ id: 'x', title: 'daemon' })), [rec(1, 'x', { kind: 'add', params: { title: 'mine', body: '' } })]).get('x')).toMatchObject({ title: 'daemon', unsynced: true })
  })

  it('a delete hides the item; a failed delete keeps it visible as failed', () => {
    const b = base(todoItem({ id: 'a' }))
    expect(overlayOutbox(N, b, [rec(1, 'a', { kind: 'delete' })]).has('a')).toBe(false)
    expect(overlayOutbox(N, b, [rec(1, 'a', { kind: 'delete' }, { state: 'failed', error: { code: 'invalid' } })]).get('a')).toMatchObject({ failed: { code: 'invalid' } })
  })

  it('a failed record flags the item; an update for an item that is not there is ignored', () => {
    expect(overlayItem(N, 'a', todoItem({ id: 'a' }), [rec(1, 'a', { kind: 'update', fields: { title: 'x' } }, { state: 'failed', error: { code: 'invalid', field: 'title' } })])).toMatchObject({ failed: { field: 'title' } })
    expect(overlayItem(N, 'zz', undefined, [rec(1, 'zz', { kind: 'update', fields: { title: 'x' } })])).toBeNull()
  })

  it('a human edit makes an agent item reviewed', () => {
    const t = todoItem({ id: 'a', unreviewed: true, source: { kind: 'agent' } })
    expect(overlayItem(N, 'a', t, [rec(1, 'a', { kind: 'update', fields: { reviewed: true } })])!.unreviewed).toBe(false)
  })

  it('closing records closed_ms, reopening removes it', () => {
    const open = overlayItem(N, 'a', todoItem({ id: 'a' }), [rec(1, 'a', { kind: 'update', fields: { state: 'done' } })])!
    expect(open.closed_ms).toBeDefined()
    const back = overlayItem(N, 'a', open, [rec(2, 'a', { kind: 'update', fields: { state: 'open' } })])!
    expect(back.closed_ms).toBeUndefined()
  })
})

describe('groups', () => {
  it('open, closed, everything else is “working”', () => {
    expect([groupOf('open'), groupOf('done'), groupOf('dropped'), groupOf('working'), groupOf('waiting'), groupOf('review')]).toEqual(['open', 'closed', 'closed', 'working', 'working', 'working'])
  })
})

describe('viewOf is incremental', () => {
  it('items without queued changes keep the same object across rebuilds; only the item with a change is rebuilt', () => {
    const a = todoItem({ id: 'a' })
    const b = todoItem({ id: 'b' })
    const nodes = { [N]: { items: { a, b }, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } as unknown as ReturnType<typeof useTodos.getState>['nodes']
    const v1 = viewOf({ nodes, outbox: [] }, [N])
    const v2 = viewOf({ nodes, outbox: [rec(1, 'b', { kind: 'update', fields: { title: 'x' } })] }, [N])
    expect(v2.find((v) => v.id === 'a')).toBe(v1.find((v) => v.id === 'a'))
    expect(v2.find((v) => v.id === 'b')).toMatchObject({ title: 'x', unsynced: true })
  })
})
