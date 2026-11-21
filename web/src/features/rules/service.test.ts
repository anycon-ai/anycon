// 规则的网络和状态（docs/spec/daemon.md § 23，spec/web.md § 4.14）：连上读一份、`rule.changed` 并入、写操作直接发给节点
// （带 `if_updated_ms`）、每种拒绝的原因、节点不在线不发请求；支持与否由 `capabilities.rules` 决定。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon, ruleError, ruleItem } from '@/test/fakeRulesDaemon'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { addRule, cancelRuleRefreshes, deleteRule, handleRuleChanged, REFRESH_DELAY_MS, resetRulesMemory, setStatusMany, syncRules, updateRule } from './service'
import { isActiveRule, nodeSupportsRules, rulesOf, sourceOf, statusOf, useRules, writtenByPerson } from './store'

const N = 'n'.repeat(32)
const M = 'm'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeRulesDaemon

beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  resetRulesMemory()
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'alpha', info: info(true), conn: 'ready' }), [M]: nodeEntry({ id: M, name: 'beta', info: info(false), conn: 'ready' }) }, order: [N, M], loaded: true })
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
  vi.useRealTimers()
})

describe('capability gating', () => {
  it('only nodes that announce capabilities.rules support rules; an old node or one without node.info does not', () => {
    expect(nodeSupportsRules(N)).toBe(true)
    expect(nodeSupportsRules(M)).toBe(false)
    expect(nodeSupportsRules('x'.repeat(32))).toBe(false)
  })

  it('rule.changed from a node that does not support rules is ignored', () => {
    handleRuleChanged(M, { id: 'r', rule: ruleItem({ id: 'r' }), deleted: false })
    expect(useRules.getState().nodes[M]).toBeUndefined()
  })
})

describe('reading', () => {
  it('rule.list fills the node with its rules and the budget', async () => {
    daemon.seed(ruleItem({ id: 'a' }), ruleItem({ id: 'b', status: 'pending' }))
    daemon.budget = { cap: 4096, node_bytes: 120, projects: { web: 30 } }
    await syncRules(N)
    const n = useRules.getState().nodes[N]!
    expect(n.loaded).toBe(true)
    expect(Object.keys(n.items).sort()).toEqual(['a', 'b'])
    expect(n.budget).toEqual({ cap: 4096, node_bytes: 120, projects: { web: 30 } })
  })

  it('a failed read marks the node failed but keeps what it had', async () => {
    daemon.seed(ruleItem({ id: 'a' }))
    await syncRules(N)
    daemon.failNext('rule.list', new RpcCallError('timeout', 'slow'))
    await syncRules(N)
    expect(useRules.getState().nodes[N]).toMatchObject({ failed: true, loaded: true })
    expect(Object.keys(useRules.getState().nodes[N]!.items)).toEqual(['a'])
  })

  it('rule.changed adds, replaces and deletes; an older version never overwrites a newer one', () => {
    handleRuleChanged(N, { id: 'a', rule: ruleItem({ id: 'a', updated_ms: 5, text: 'new' }), deleted: false })
    handleRuleChanged(N, { id: 'a', rule: ruleItem({ id: 'a', updated_ms: 3, text: 'stale' }), deleted: false })
    expect(useRules.getState().nodes[N]!.items.a!.text).toBe('new')
    handleRuleChanged(N, { id: 'a', deleted: true })
    expect(useRules.getState().nodes[N]!.items.a).toBeUndefined()
    // malformed ones are dropped
    handleRuleChanged(N, { id: 'z', deleted: false })
    handleRuleChanged(N, 'nonsense')
    expect(useRules.getState().nodes[N]!.items.z).toBeUndefined()
  })

  it('re-reads the whole list shortly after a change (budget and last-used are not in rule.changed), once for a burst', async () => {
    vi.useFakeTimers()
    daemon.seed(ruleItem({ id: 'a' }))
    handleRuleChanged(N, { id: 'a', rule: ruleItem({ id: 'a', updated_ms: 2 }), deleted: false })
    handleRuleChanged(N, { id: 'a', rule: ruleItem({ id: 'a', updated_ms: 3 }), deleted: false })
    expect(daemon.calls('rule.list')).toHaveLength(0)
    await vi.advanceTimersByTimeAsync(REFRESH_DELAY_MS + 10)
    expect(daemon.calls('rule.list')).toHaveLength(1)
  })

  it('rules come out oldest first, with the node on each', () => {
    useRules.getState().replaceAll(N, [ruleItem({ id: 'b', created_ms: 2 }), ruleItem({ id: 'a', created_ms: 1 })], { cap: 4096, node_bytes: 0, projects: {} })
    expect(rulesOf(useRules.getState().nodes, [N]).map((r) => [r.id, r.node_id])).toEqual([['a', N], ['b', N]])
  })
})

describe('what an unknown value means', () => {
  it('a status this version does not know is not active; a source kind it does not know is not written by a person', () => {
    const r = ruleItem({ id: 'x', status: 'expired' as never, source: { kind: 'twin' as never } })
    expect(statusOf(r)).toBe('unknown')
    expect(isActiveRule(r)).toBe(false)
    expect(sourceOf(r)).toBe('other')
    expect(writtenByPerson(r)).toBe(false)
    expect(writtenByPerson(ruleItem({ id: 'm', source: { kind: 'manual' } }))).toBe(true)
    expect(writtenByPerson(ruleItem({ id: 'a', source: { kind: 'answer' } }))).toBe(true)
    expect(writtenByPerson(ruleItem({ id: 'g', source: { kind: 'agent' } }))).toBe(false)
    expect(writtenByPerson(ruleItem({ id: 'e', source: { kind: 'extracted' } }))).toBe(false)
  })
})

describe('writing', () => {
  it('add sends a client-made id (the same one on a retry), the text, the scope and the source; the rule is in the store at once', async () => {
    const r = await addRule(N, { id: '11111111-2222-4333-8444-555555555555', text: 'write the test first', project: 'web', status: 'active', source: { kind: 'answer', session_id: 's1', request_id: 'q1' } })
    expect(r.ok).toBe(true)
    expect(daemon.calls('rule.add')[0]).toEqual({ id: '11111111-2222-4333-8444-555555555555', text: 'write the test first', project: 'web', status: 'active', source: { kind: 'answer', session_id: 's1', request_id: 'q1' } })
    expect(useRules.getState().nodes[N]!.items['11111111-2222-4333-8444-555555555555']!.text).toBe('write the test first')
    // a blank project is not sent
    await addRule(N, { text: 'node wide', project: '  ' })
    expect(daemon.calls('rule.add')[1]).not.toHaveProperty('project')
  })

  it('update carries the version it was looked at (if_updated_ms); clearing the scope sends clear', async () => {
    daemon.seed(ruleItem({ id: 'a', project: 'web', updated_ms: 7 }))
    await syncRules(N)
    const r = await updateRule(N, 'a', { text: 'changed', project: null, status: 'active' }, 7)
    expect(r.ok).toBe(true)
    expect(daemon.calls('rule.update')[0]).toEqual({ id: 'a', text: 'changed', clear: ['project'], status: 'active', if_updated_ms: 7 })
  })

  it('a rule changed elsewhere is not overwritten: the answer says so and carries the new version', async () => {
    daemon.seed(ruleItem({ id: 'a', text: 'mine', updated_ms: 7 }))
    await syncRules(N)
    daemon.changeElsewhere('a', { text: 'theirs' })
    const r = await updateRule(N, 'a', { status: 'active' }, 7)
    expect(r).toMatchObject({ ok: false, fail: { kind: 'changed', current: { id: 'a', text: 'theirs' } } })
    expect(daemon.rules.get('a')!.text).toBe('theirs')
    expect(useRules.getState().nodes[N]!.items.a!.text).toBe('theirs')
  })

  it('delete carries the version too; the same conflict applies; a deleted-elsewhere rule says so', async () => {
    daemon.seed(ruleItem({ id: 'a', updated_ms: 7 }))
    await syncRules(N)
    daemon.changeElsewhere('a', { text: 'edited' })
    expect(await deleteRule(N, 'a', 7)).toMatchObject({ ok: false, fail: { kind: 'changed' } })
    expect(daemon.rules.has('a')).toBe(true)
    expect(await deleteRule(N, 'a', daemon.rules.get('a')!.updated_ms)).toEqual({ ok: true })
    expect(useRules.getState().nodes[N]!.items.a).toBeUndefined()
    daemon.seed(ruleItem({ id: 'b', updated_ms: 1 }))
    await syncRules(N)
    daemon.rules.delete('b')
    expect(await deleteRule(N, 'b', 1)).toMatchObject({ ok: false, fail: { kind: 'not_found' } })
  })

  it('a change whose rule vanished meanwhile reports changed with no current version', async () => {
    daemon.seed(ruleItem({ id: 'a', updated_ms: 7 }))
    await syncRules(N)
    daemon.failNext('rule.update', ruleError('invalid', { field: 'if_updated_ms', reason: 'changed', updated_ms: 9 }))
    daemon.rules.delete('a')
    expect(await updateRule(N, 'a', { status: 'active' }, 7)).toMatchObject({ ok: false, fail: { kind: 'changed', current: null } })
  })

  it.each([
    ['over_budget', ruleError('invalid', { field: 'text', reason: 'over_budget', bytes: 4300, cap: 4096 }), { kind: 'over_budget', bytes: 4300, cap: 4096 }],
    ['too_many', ruleError('invalid', { field: 'rules', reason: 'too_many' }), { kind: 'too_many' }],
    ['too_many_pending', ruleError('invalid', { field: 'rules', reason: 'too_many_pending' }), { kind: 'too_many_pending' }],
    ['invisible', ruleError('invalid', { field: 'text', reason: 'invisible' }), { kind: 'invisible' }],
    ['invalid project', ruleError('invalid', { field: 'project' }), { kind: 'invalid', field: 'project' }],
    ['forbidden', ruleError('forbidden'), { kind: 'forbidden' }],
    ['unsupported', ruleError('unsupported'), { kind: 'unsupported' }],
    ['link', new RpcCallError('timeout', 'slow'), { kind: 'link' }],
  ])('maps a refusal (%s) to its reason', async (_name, error, fail) => {
    daemon.failNext('rule.add', error)
    expect(await addRule(N, { text: 'x' })).toEqual({ ok: false, fail })
    // nothing entered the store
    expect(Object.keys(useRules.getState().nodes[N]?.items ?? {})).toEqual([])
  })

  it('does not send anything when the node is offline', async () => {
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'alpha', info: info(true), conn: 'peer_offline' }) }, order: [N], loaded: true })
    expect(await addRule(N, { text: 'x' })).toEqual({ ok: false, fail: { kind: 'offline' } })
    expect(await updateRule(N, 'a', { status: 'active' }, 1)).toEqual({ ok: false, fail: { kind: 'offline' } })
    expect(await deleteRule(N, 'a', 1)).toEqual({ ok: false, fail: { kind: 'offline' } })
    expect(rpc.calls).toEqual([])
  })

  it('a batch changes rules one by one, each with its own version, and reports what failed', async () => {
    daemon.seed(ruleItem({ id: 'a', status: 'pending', updated_ms: 1 }), ruleItem({ id: 'b', status: 'pending', updated_ms: 2 }), ruleItem({ id: 'c', status: 'pending', updated_ms: 3 }))
    await syncRules(N)
    daemon.changeElsewhere('b', { text: 'edited' })
    const res = await setStatusMany(Object.values(useRules.getState().nodes[N]!.items).map((r) => ({ ...r, node_id: N })), 'active')
    expect(res.done).toBe(2)
    expect(res.failed.map((f) => [f.id, f.fail.kind])).toEqual([['b', 'changed']])
    expect(daemon.rules.get('a')!.status).toBe('active')
    expect(daemon.rules.get('b')!.status).toBe('pending')
  })
})

describe('a full list that was requested before a change does not undo it', () => {
  const NN = 'n'.repeat(32)
  const mk = (id: string, ms: number, text = id) => ({ id, text, status: 'active' as const, sync: 'node' as const, source: { kind: 'manual' as const }, created_ms: 1, updated_ms: ms })
  const budget = { cap: 4096, node_bytes: 0, projects: {} }

  it('keeps a rule changed by a notification while the list was on its way, and does not bring back one deleted meanwhile', () => {
    useRules.getState().reset()
    useRules.getState().replaceAll(NN, [mk('a', 1), mk('b', 1), mk('c', 1)], budget)
    const since = useRules.getState().tick
    useRules.getState().applyChanged(NN, { id: 'a', rule: mk('a', 5, 'edited'), deleted: false })
    useRules.getState().applyChanged(NN, { id: 'b', deleted: true })
    useRules.getState().applyChanged(NN, { id: 'd', rule: mk('d', 6), deleted: false })
    useRules.getState().replaceAll(NN, [mk('a', 1), mk('b', 1), mk('c', 1)], budget, since)
    const items = useRules.getState().nodes[NN]!.items
    expect(items.a!.text).toBe('edited')
    expect(items.b).toBeUndefined()
    expect(items.c).toBeDefined()
    expect(items.d).toBeDefined()
  })

  it('a list that is newer than the local copy wins, and without a since the list is taken as it is', () => {
    useRules.getState().reset()
    useRules.getState().replaceAll(NN, [mk('a', 1)], budget)
    const since = useRules.getState().tick
    useRules.getState().applyChanged(NN, { id: 'a', rule: mk('a', 5, 'edited'), deleted: false })
    useRules.getState().replaceAll(NN, [mk('a', 9, 'newer on the node')], budget, since)
    expect(useRules.getState().nodes[NN]!.items.a!.text).toBe('newer on the node')
    useRules.getState().replaceAll(NN, [], budget)
    expect(Object.keys(useRules.getState().nodes[NN]!.items)).toEqual([])
  })

  it('syncRules drops what a notification deleted while its request was pending', async () => {
    useRules.getState().reset()
    const rpc = new FakeRpc()
    let release: (v: unknown) => void = () => {}
    rpc.handlers['rule.list'] = () => new Promise((res) => (release = res))
    setRpc(rpc)
    const pending = syncRules(NN)
    await Promise.resolve()
    handleRuleChanged(NN, { id: 'x', deleted: true })
    release({ rules: [mk('x', 1)], budget })
    await pending
    expect(useRules.getState().nodes[NN]!.items.x).toBeUndefined()
    setRpc(null)
  })
})
