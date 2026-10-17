import { describe, expect, it } from 'vitest'
import { summary } from '@/test/factories'
import { planSessionSync } from './plan'

describe('planSessionSync', () => {
  it('upserts every remote session (new ones are inserted, known ones refreshed)', () => {
    const p = planSessionSync({ a: { cachedSeq: 5 } }, [summary({ id: 'a', last_seq: 8 }), summary({ id: 'b', last_seq: 1 })])
    expect(p.upsert.map((s) => s.id)).toEqual(['a', 'b'])
    expect(p.reset).toEqual([])
    expect(p.deleted).toEqual([])
  })

  it('marks local sessions missing remotely as deleted', () => {
    const p = planSessionSync({ a: { cachedSeq: 1 }, gone: { cachedSeq: 2 } }, [summary({ id: 'a', last_seq: 1 })])
    expect(p.deleted).toEqual(['gone'])
  })

  it('resets the cache when the remote seq went backwards', () => {
    const p = planSessionSync({ a: { cachedSeq: 50 } }, [summary({ id: 'a', last_seq: 3 })])
    expect(p.reset).toEqual(['a'])
  })

  it('does not reset sessions that were never cached or that only moved forward', () => {
    const p = planSessionSync({ a: { cachedSeq: 0 }, b: { cachedSeq: 4 } }, [summary({ id: 'a', last_seq: 8 }), summary({ id: 'b', last_seq: 9 })])
    expect(p.reset).toEqual([])
  })
})
