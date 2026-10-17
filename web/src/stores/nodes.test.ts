import { beforeEach, describe, expect, it } from 'vitest'
import { listNodes, resetDb, type NodeRecord } from '@/db'
import { useNodes } from './nodes'

const rec = (over: Partial<NodeRecord> = {}): NodeRecord => ({ node_id: 'n1', name: 'mac', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1, ...over })
const project = (name: string) => ({ name, path: `/w/${name}`, modified_at_ms: 1, active_sessions: 0 })

beforeEach(async () => {
  await resetDb()
  useNodes.setState({ byId: {}, order: [], loaded: false })
})

describe('project list cache', () => {
  it('persists the project list so an offline reload still shows it', async () => {
    useNodes.getState().hydrate([rec()])
    expect(useNodes.getState().byId['n1']!.projectsLoaded).toBe(false) // 还没拿到过
    useNodes.getState().setProjects('n1', '/w', [project('a'), project('b')])
    await new Promise((r) => setTimeout(r, 20))
    const saved = (await listNodes())[0]!
    expect(saved.projects?.map((p) => p.name)).toEqual(['a', 'b'])
    expect(saved.workspace_root).toBe('/w')

    // 「重载」：从 IndexedDB 读回来
    useNodes.setState({ byId: {}, order: [], loaded: false })
    useNodes.getState().hydrate(await listNodes())
    const e = useNodes.getState().byId['n1']!
    expect(e.projects.map((p) => p.name)).toEqual(['a', 'b'])
    expect(e.workspaceRoot).toBe('/w')
    expect(e.projectsLoaded).toBe(true)
  })

  it('keeps the cache in step with workspace.changed (rename / removal)', async () => {
    useNodes.getState().hydrate([rec()])
    useNodes.getState().setProjects('n1', '/w', [project('a'), project('b')])
    useNodes.getState().applyWorkspaceChanged('n1', { removed: ['b'], renamed: [{ from: 'a', to: 'c' }] } as never)
    await new Promise((r) => setTimeout(r, 20))
    expect((await listNodes())[0]!.projects?.map((p) => p.name)).toEqual(['c'])
  })

  it('a node that never reported its projects stays "not loaded" instead of looking empty', () => {
    useNodes.getState().hydrate([rec()])
    expect(useNodes.getState().byId['n1']).toMatchObject({ projects: [], projectsLoaded: false })
  })
})
