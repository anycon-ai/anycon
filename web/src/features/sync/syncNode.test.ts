import { beforeEach, afterEach, describe, expect, it } from 'vitest'
import { resetDb } from '@/db'
import { FakeRpc } from '@/test/fakeRpc'
import { mockAgent, summary } from '@/test/factories'
import { setRpc } from '@/transport/instance'
import { useDrafts } from '@/stores/drafts'
import { pendingApprovals, useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import type { NodeInfo } from '@/protocol'
import { applySessionList, protocolCompatible, syncNode } from './syncNode'

const NODE = 'n'.repeat(32)
const info = (over: Partial<NodeInfo> = {}): NodeInfo => ({
  node_id: NODE,
  name: 'mac-mini',
  platform: 'macos-aarch64',
  version: '0.1.0',
  protocol_version: 1,
  started_at_ms: 1,
  workspace_root: '/w',
  capabilities: { audit: true, blobs: false, fs: false, search: true, workspace: true },
  ...over,
})
let rpc: FakeRpc

beforeEach(async () => {
  await resetDb()
  rpc = new FakeRpc()
  setRpc(rpc)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  useNodes.getState().hydrate([{ node_id: NODE, name: 'node', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 }])
  useNodes.getState().setConn(NODE, 'ready')
})
afterEach(() => setRpc(null))

describe('protocolCompatible', () => {
  it('accepts the same or adjacent protocol version', () => {
    expect(protocolCompatible(1, 1)).toBe(true)
    expect(protocolCompatible(2, 1)).toBe(true)
    expect(protocolCompatible(3, 1)).toBe(false)
  })
})

describe('syncNode', () => {
  it('runs node.info → agent.list → workspace.list → session.list → permission.list → node.devices → node.enroll_list and fills the stores', async () => {
    const agents = [mockAgent()]
    rpc.handlers['node.info'] = () => info()
    rpc.handlers['agent.list'] = () => ({ agents, etag: 'e1' })
    rpc.handlers['workspace.list'] = () => ({ root: '/w', projects: [{ name: 'demo', path: '/w/demo', modified_at_ms: 1, active_sessions: 0 }] })
    rpc.handlers['session.list'] = () => ({ sessions: [summary({ id: 's1', last_seq: 4, pending_permissions: 1 })] })
    rpc.handlers['permission.list'] = () => ({ requests: [{ session_id: 's1', request_id: 'r1', kind: 'tool', title: 'Run', detail: {}, options: [], created_at_ms: 1 }] })
    rpc.handlers['node.devices'] = () => ({ devices: [{ device_id: 'd1', fingerprint: 'ab:cd', kind: 'web', name: 'Chrome', online: true, paired_at_ms: 1 }] })
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [] })
    await syncNode(NODE)
    expect(rpc.methods()).toEqual(['node.info', 'agent.list', 'workspace.list', 'session.list', 'permission.list', 'node.devices', 'node.enroll_list'])
    const n = useNodes.getState().byId[NODE]!
    expect(n).toMatchObject({ name: 'mac-mini', agentsEtag: 'e1', agentsLoaded: true, synced: true, syncing: false, versionMismatch: false })
    expect(n.agents).toHaveLength(1)
    expect(n.projects.map((p) => p.name)).toEqual(['demo'])
    expect(n.devices.map((d) => d.device_id)).toEqual(['d1'])
    expect(useSessions.getState().list.s1).toMatchObject({ node_id: NODE, last_seq: 4 })
    expect(Object.keys(useSessions.getState().pending)).toEqual(['r1'])
    expect(rpc.calls.find((c) => c.method === 'session.list')!.params).toMatchObject({ include_archived: true })
  })

  it('asks agent.list with the cached etag and keeps the cache on not_modified', async () => {
    useNodes.getState().setAgents(NODE, [mockAgent()], 'e1')
    rpc.handlers['node.info'] = () => info()
    rpc.handlers['agent.list'] = () => ({ agents: [], etag: 'e1', not_modified: true })
    rpc.handlers['workspace.list'] = () => ({ root: '/w', projects: [] })
    rpc.handlers['session.list'] = () => ({ sessions: [] })
    rpc.handlers['permission.list'] = () => ({ requests: [] })
    rpc.handlers['node.devices'] = () => ({ devices: [] })
    await syncNode(NODE)
    expect(rpc.calls[1]!.params).toEqual({ if_none_match: 'e1' })
    expect(useNodes.getState().byId[NODE]!.agents).toHaveLength(1)
  })

  it('fills in the enrollment requests that are still waiting (reconnect / page load), and drops the ones that ended while offline', async () => {
    useEnroll.getState().upsert(NODE, { enroll_id: 'stale', method: 'code', approval: true, status: 'wait_approval', created_at_ms: 1, expires_at_ms: Date.now() + 100_000 })
    rpc.handlers['node.info'] = () => info()
    rpc.handlers['agent.list'] = () => ({ agents: [], etag: 'e' })
    rpc.handlers['session.list'] = () => ({ sessions: [] })
    rpc.handlers['node.enroll_list'] = () => ({
      enrolls: [
        { enroll_id: 'live', method: 'code', approval: true, status: 'wait_approval', hint: { name: 'iPad', kind: 'web', platform: 'browser' }, created_at_ms: 1, expires_at_ms: Date.now() + 100_000 },
        { bogus: true },
      ],
    })
    await syncNode(NODE)
    expect(Object.keys(useEnroll.getState().items)).toEqual([`${NODE}:live`]) // 已经结束的 stale 不在了，坏数据被丢掉
    expect(pendingApprovals(useEnroll.getState(), Date.now()).map((i) => i.info.enroll_id)).toEqual(['live'])
  })

  it('stops after node.info when the daemon speaks an incompatible protocol', async () => {
    rpc.handlers['node.info'] = () => info({ protocol_version: 9 })
    await syncNode(NODE)
    expect(rpc.methods()).toEqual(['node.info'])
    expect(useNodes.getState().byId[NODE]).toMatchObject({ versionMismatch: true, synced: false })
  })

  it('keeps going when an optional step fails with a daemon error', async () => {
    rpc.handlers['node.info'] = () => info()
    rpc.handlers['agent.list'] = () => ({ agents: [], etag: 'e' })
    rpc.handlers['session.list'] = () => ({ sessions: [summary({ id: 's1' })] })
    // workspace.list / permission.list / node.devices 没有 handler → 返回 unsupported，同步照常完成
    await syncNode(NODE)
    expect(useNodes.getState().byId[NODE]!.synced).toBe(true)
    expect(useSessions.getState().list.s1).toBeDefined()
  })

  it('gives up quietly when the link drops mid-sync', async () => {
    rpc.handlers['node.info'] = () => info()
    rpc.handlers['agent.list'] = () => {
      rpc.ready = false
      return { agents: [], etag: 'e' }
    }
    await syncNode(NODE)
    expect(useNodes.getState().byId[NODE]).toMatchObject({ synced: false, syncing: false })
  })
})

describe('applySessionList', () => {
  it('removes sessions the daemon no longer lists and keeps ones it still has', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'keep', last_seq: 1 }), summary({ id: 'gone', last_seq: 1 })])
    await applySessionList(NODE, [summary({ id: 'keep', last_seq: 1, title: 'renamed' })])
    const list = useSessions.getState().list
    expect(Object.keys(list)).toEqual(['keep'])
    expect(list.keep!.title).toBe('renamed')
  })

  it('resets the cached seq when the daemon history went backwards', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 's', last_seq: 50 })])
    useSessions.setState((s) => ({ list: { ...s.list, s: { ...s.list.s!, cached_seq: 50 } } }))
    await applySessionList(NODE, [summary({ id: 's', last_seq: 3 })])
    expect(useSessions.getState().list.s!.cached_seq).toBe(0)
  })

  it('verifies a vanished open session with session.get before dropping it', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'open', last_seq: 1 })])
    useSessions.getState().openStart(NODE, 'open', [])
    rpc.handlers['session.get'] = () => summary({ id: 'open', last_seq: 1, title: 'still here' })
    await applySessionList(NODE, [])
    expect(useSessions.getState().list.open!.title).toBe('still here')
  })
})
