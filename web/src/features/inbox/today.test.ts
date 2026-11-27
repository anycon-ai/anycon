import { afterEach, describe, expect, it } from 'vitest'
import { summary } from '@/test/factories'
import type { SessionItem } from '@/stores/sessions'
import { answeredToday, recordAnswered, sessionsActiveToday, startOfLocalDay } from './today'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const NOON = new Date(2026, 10, 27, 12, 0, 0).getTime()

const sess = (id: string, over: Partial<SessionItem> = {}): SessionItem => ({ ...summary({ id, updated_at_ms: NOON - 1000 }), node_id: A, cached_seq: 0, ...over })

afterEach(() => localStorage.removeItem('anycon.today'))

describe('answers counted in this browser', () => {
  it('counts per node and adds up only the nodes asked for', () => {
    recordAnswered(A, NOON)
    recordAnswered(A, NOON)
    recordAnswered(B, NOON)
    expect(answeredToday([A], NOON)).toBe(2)
    expect(answeredToday([A, B], NOON)).toBe(3)
    expect(answeredToday([], NOON)).toBe(0)
  })

  it('starts from zero on a new local day', () => {
    recordAnswered(A, NOON)
    const tomorrow = NOON + 24 * 60 * 60 * 1000
    expect(answeredToday([A], tomorrow)).toBe(0)
    recordAnswered(A, tomorrow)
    expect(answeredToday([A], tomorrow)).toBe(1)
  })

  it('treats a damaged record as no answers instead of throwing', () => {
    localStorage.setItem('anycon.today', '{not json')
    expect(answeredToday([A], NOON)).toBe(0)
    localStorage.setItem('anycon.today', JSON.stringify({ day: '2026-11-27', answered: { [A]: 'many', [B]: -3 } }))
    expect(answeredToday([A, B], NOON)).toBe(0)
  })
})

describe('sessions active today', () => {
  it('counts sessions on the picked nodes whose last event is today, leaving out archived ones', () => {
    const list = {
      s1: sess('s1'),
      s2: sess('s2', { updated_at_ms: startOfLocalDay(NOON) }),
      old: sess('old', { updated_at_ms: startOfLocalDay(NOON) - 1 }),
      arch: sess('arch', { archived: true }),
      other: sess('other', { node_id: B }),
    }
    expect(sessionsActiveToday(list, [A], NOON)).toBe(2)
    expect(sessionsActiveToday(list, [A, B], NOON)).toBe(3)
  })
})
