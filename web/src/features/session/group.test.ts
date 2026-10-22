import { describe, expect, it } from 'vitest'
import { summary } from '@/test/factories'
import type { SessionItem } from '@/stores/sessions'
import { groupNodeSessions, groupSessions } from './group'

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

describe('groupNodeSessions', () => {
  const proj = (name: string, modified_at_ms = 0) => ({ name, path: `/${name}`, modified_at_ms, active_sessions: 0 })

  it('merges the projects of every given node, same-named projects stay separate groups', () => {
    const g = groupNodeSessions(
      [
        { id: 'a', projects: [proj('app'), proj('only-a')] },
        { id: 'b', projects: [proj('app')] },
      ],
      [item({ id: '1', node_id: 'a', project: 'app', updated_at_ms: 10 }), item({ id: '2', node_id: 'b', project: 'app', updated_at_ms: 30 })],
    )
    expect(g.projects.map((p) => [p.nodeId, p.name, p.sessions.map((s) => s.id)])).toEqual([
      ['b', 'app', ['2']],
      ['a', 'app', ['1']],
      ['a', 'only-a', []],
    ])
  })

  it('leaves out sessions of nodes that are not given, scratch merged newest first', () => {
    const g = groupNodeSessions(
      [
        { id: 'a', projects: [] },
        { id: 'b', projects: [] },
      ],
      [item({ id: '1', node_id: 'a', updated_at_ms: 10 }), item({ id: '2', node_id: 'b', updated_at_ms: 20 }), item({ id: '3', node_id: 'c', updated_at_ms: 30 }), item({ id: '4', node_id: 'c', project: 'x' })],
    )
    expect(g.scratch.map((s) => s.id)).toEqual(['2', '1'])
    expect(g.projects).toEqual([])
  })
})
