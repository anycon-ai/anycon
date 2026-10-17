import { describe, expect, it } from 'vitest'
import { summary } from '@/test/factories'
import type { SessionItem } from '@/stores/sessions'
import { groupSessions } from './group'

const item = (o: Partial<SessionItem>): SessionItem => ({ ...summary(), node_id: 'n', cached_seq: 0, ...o })

describe('groupSessions', () => {
  it('groups by project, most recent activity first, scratch separate', () => {
    const g = groupSessions(
      [item({ id: '1', project: 'a', updated_at_ms: 10 }), item({ id: '2', project: 'b', updated_at_ms: 30 }), item({ id: '3', updated_at_ms: 20 }), item({ id: '4', project: 'a', updated_at_ms: 40 })],
      [],
    )
    expect(g.projects.map((p) => p.name)).toEqual(['a', 'b'])
    expect(g.projects[0]!.sessions.map((s) => s.id)).toEqual(['4', '1'])
    expect(g.scratch.map((s) => s.id)).toEqual(['3'])
  })
  it('hides archived sessions unless asked and keeps empty projects', () => {
    const projects = [{ name: 'empty', path: '/x', modified_at_ms: 5, active_sessions: 0 }]
    const g = groupSessions([item({ id: '1', project: 'a', archived: true })], projects)
    expect(g.projects.map((p) => p.name)).toEqual(['empty'])
    expect(groupSessions([item({ id: '1', project: 'a', archived: true })], projects, { includeArchived: true }).projects.map((p) => p.name)).toContain('a')
  })
  it('sums pending permissions per project', () => {
    const g = groupSessions([item({ id: '1', project: 'a', pending_permissions: 2 }), item({ id: '2', project: 'a', pending_permissions: 1 })], [])
    expect(g.projects[0]!.pending).toBe(3)
  })
})
