import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetDb } from '@/db'
import { FakeRpc } from '@/test/fakeRpc'
import { gitProject, project, summary } from '@/test/factories'
import { setRpc } from '@/transport/instance'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { handleNotification, refreshProjects } from './notifications'

const NODE = 'n'.repeat(32)
const SELF = 's'.repeat(32)
const OTHER = 'o'.repeat(32)
let rpc: FakeRpc

const resolved = (by: string) => ({ session_id: 'sess_1', request_id: 'r1', option_id: 'allow', by_device: by })
const flush = () => new Promise((r) => setTimeout(r, 0))

beforeEach(async () => {
  await resetDb()
  rpc = new FakeRpc()
  rpc.handlers['node.devices'] = () => ({
    devices: [{ device_id: OTHER, fingerprint: 'ab:cd', kind: 'web', name: 'Second browser', online: true, paired_at_ms: 1 }],
  })
  setRpc(rpc)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useNodes.getState().hydrate([{ node_id: NODE, name: 'node', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 }])
  useHub.setState((s) => ({ snapshot: { ...s.snapshot, deviceId: SELF } }))
})
afterEach(() => setRpc(null))

const NODE_B = 'b'.repeat(32)
const perm = (id: string, session: string) => ({ session_id: session, request_id: id, kind: 'tool' as const, title: 't', detail: {}, options: [], created_at_ms: 1 })

describe('notifications are isolated per node', () => {
  beforeEach(() => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    useNodes.getState().hydrate([
      { node_id: NODE, name: 'a', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 },
      { node_id: NODE_B, name: 'b', public_key: new Uint8Array(32), paired_at: 2, last_seen: 1 },
    ])
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_a', title: 'mine', status: 'idle' })])
    useSessions.getState().upsertSummaries(NODE_B, [summary({ id: 'sess_b', title: 'theirs' })])
  })

  it('session.changed from another node cannot rewrite, re-home or delete a session it does not own', () => {
    handleNotification(NODE_B, 'session.changed', { session_id: 'sess_a', title: 'pwned', status: 'running', last_seq: 99, pending_permissions: 0, updated_at_ms: 9 })
    expect(useSessions.getState().list['sess_a']).toMatchObject({ node_id: NODE, title: 'mine', status: 'idle' })
    handleNotification(NODE_B, 'session.changed', { session_id: 'sess_a', title: 'x', status: 'idle', last_seq: 1, pending_permissions: 0, updated_at_ms: 9, deleted: true })
    expect(useSessions.getState().list['sess_a']).toBeDefined()
    // 自己的会话照常更新
    handleNotification(NODE, 'session.changed', { session_id: 'sess_a', title: 'renamed', status: 'idle', last_seq: 1, pending_permissions: 0, updated_at_ms: 9 })
    expect(useSessions.getState().list['sess_a']!.title).toBe('renamed')
  })

  it('session.event is only accepted from the node that owns the open session', async () => {
    useSessions.getState().openStart(NODE, 'sess_a', [])
    const e = (seq: number) => ({ session_id: 'sess_a', seq, ts_ms: 1, event: { type: 'text_delta' as const, text: 'x' } })
    handleNotification(NODE_B, 'session.event', e(1))
    handleNotification(NODE_B, 'session.event', { ...e(1), session_id: 'sess_b' }) // sess_b 没打开
    await flush()
    await new Promise((r) => setTimeout(r, 30))
    expect(useSessions.getState().open['sess_a']!.events).toHaveLength(0)
    handleNotification(NODE, 'session.event', e(1))
    await new Promise((r) => setTimeout(r, 30))
    expect(useSessions.getState().open['sess_a']!.events).toHaveLength(1)
  })

  it('permission requests/resolutions cannot touch another node’s session or request ids', () => {
    handleNotification(NODE, 'permission.request', perm('r1', 'sess_a'))
    expect(Object.keys(useSessions.getState().pending)).toEqual(['r1'])
    // 另一个节点：对别人的会话发请求、用别人的 request id 覆盖、替别人答复，都无效
    handleNotification(NODE_B, 'permission.request', perm('r2', 'sess_a'))
    handleNotification(NODE_B, 'permission.request', { ...perm('r1', 'sess_b'), title: 'overwritten' })
    expect(useSessions.getState().pending['r1']).toMatchObject({ node_id: NODE, title: 't' })
    expect(useSessions.getState().pending['r2']).toBeUndefined()
    handleNotification(NODE_B, 'permission.resolved', { session_id: 'sess_a', request_id: 'r1', option_id: 'allow', by_device: OTHER })
    expect(useSessions.getState().pending['r1']).toBeDefined()
    handleNotification(NODE, 'permission.resolved', { session_id: 'sess_a', request_id: 'r1', option_id: 'allow', by_device: OTHER })
    expect(useSessions.getState().pending['r1']).toBeUndefined()
  })

  it('a node listing another node’s session id cannot take it over', () => {
    useSessions.getState().upsertSummaries(NODE_B, [summary({ id: 'sess_a', title: 'stolen' })])
    expect(useSessions.getState().list['sess_a']).toMatchObject({ node_id: NODE, title: 'mine' })
    useSessions.getState().setPending(NODE_B, [perm('r9', 'sess_a')])
    expect(useSessions.getState().pending['r9']).toBeUndefined()
  })
})

describe('permission.resolved', () => {
  it('re-reads node.devices when another, not yet known device answered (so the card can show its name)', async () => {
    handleNotification(NODE, 'permission.resolved', resolved(OTHER))
    await flush()
    expect(rpc.methods()).toEqual(['node.devices'])
    expect(useNodes.getState().byId[NODE]!.devices.map((d) => d.name)).toEqual(['Second browser'])
  })

  it('does not ask again once the device is known, and never for this device or the daemon itself', async () => {
    handleNotification(NODE, 'permission.resolved', resolved(OTHER))
    await flush()
    rpc.calls = []
    handleNotification(NODE, 'permission.resolved', resolved(OTHER))
    handleNotification(NODE, 'permission.resolved', resolved(SELF))
    handleNotification(NODE, 'permission.resolved', resolved('daemon'))
    await flush()
    expect(rpc.methods()).toEqual([])
  })

  it('a request the daemon itself resolved (folder changed, timeout, cancel) leaves the pending list without looking up a device called “daemon”', async () => {
    handleNotification(NODE, 'permission.request', perm('r7', 'sess_a'))
    expect(useSessions.getState().pending['r7']).toBeDefined()
    handleNotification(NODE, 'permission.resolved', { ...resolved('daemon'), request_id: 'r7' })
    await flush()
    expect(useSessions.getState().pending['r7']).toBeUndefined()
    expect(rpc.methods()).toEqual([])
  })
})

describe('workspace.changed { updated }', () => {
  it('re-reads workspace.list, so a project whose git info the daemon finished computing gets it', async () => {
    useNodes.getState().setProjects(NODE, '/ws', [project({ name: 'app' })]) // 冷启动时先不带 git
    expect(useNodes.getState().byId[NODE]!.projects[0]!.git).toBeUndefined()
    rpc.handlers['workspace.list'] = () => ({ root: '/ws', projects: [gitProject('app', { branch: 'dev' })] })
    handleNotification(NODE, 'workspace.changed', { updated: ['app'] })
    await flush()
    expect(rpc.methods()).toEqual(['workspace.list'])
    expect(useNodes.getState().byId[NODE]!.projects[0]!.git).toMatchObject({ branch: 'dev', worktree_ready: true })
  })

  it('an older answer that arrives after a newer one does not take the newer one’s git info away', async () => {
    const pending: Array<(projects: ReturnType<typeof project>[]) => void> = []
    rpc.handlers['workspace.list'] = () => new Promise((res) => pending.push((projects) => res({ root: '/ws', projects })))
    const older = refreshProjects(NODE)
    const newer = refreshProjects(NODE)
    pending[1]!([gitProject('app')])
    await newer
    pending[0]!([project({ name: 'app' })])
    await older
    expect(useNodes.getState().byId[NODE]!.projects[0]!.git).toBeDefined()
  })

  it('but an older answer still counts when the newer request failed', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const pending: Array<(projects: ReturnType<typeof project>[]) => void> = []
    let n = 0
    rpc.handlers['workspace.list'] = () => {
      n += 1
      if (n === 2) throw new Error('boom')
      return new Promise((res) => pending.push((projects) => res({ root: '/ws', projects })))
    }
    const older = refreshProjects(NODE)
    await refreshProjects(NODE) // 失败
    pending[0]!([gitProject('app')])
    await older
    expect(useNodes.getState().byId[NODE]!.projects[0]!.git).toBeDefined()
  })
})
