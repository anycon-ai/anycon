// 同组期望状态的持久化、合并、重放、拒绝分类、对齐比较与用户版本选择。
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import * as db from '@/db'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { ruleItem } from '@/test/fakeRulesDaemon'
import { useNodes } from '@/stores/nodes'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { MAX_DROPS, MAX_LINK_FAILURES, alignRule, classifyRuleSyncError, compareGroupRules, discardRuleRecord, enqueueRuleSync, groupPeers, readGroupRules, reloadRuleSync, replayRuleSync, resetRuleSync, resumeRuleSync, retryRuleRecord, useRuleSync } from './groupSync'
import { addRule, cancelRuleRefreshes, deleteRule, resetRulesMemory, updateRule } from './service'
import { useRules } from './store'
import type { DesiredRule } from './syncTypes'

const A = 'a'.repeat(32), B = 'b'.repeat(32), C = 'c'.repeat(32), D = 'd'.repeat(32)
const info = (group: string): NodeInfo => ({ group, capabilities: { rules: true } }) as unknown as NodeInfo
const desired = { text: 'ask first', status: 'active', sync: 'group' } satisfies DesiredRule
let rpc: FakeRpc
beforeEach(async () => {
  resetRulesMemory()
  await db.resetDb()
  rpc = new FakeRpc()
  setRpc(rpc)
  useNodes.setState({ byId: {
    [A]: nodeEntry({ id: A, conn: 'ready', info: info('work') }),
    [B]: nodeEntry({ id: B, conn: 'peer_offline', info: info('work') }),
    [C]: nodeEntry({ id: C, conn: 'peer_offline', info: info('home') }),
    [D]: nodeEntry({ id: D, conn: 'peer_offline', info: info('') }),
  }, order: [A, B, C, D] })
})
afterEach(async () => {
  for (const node of [A, B, C, D]) await replayRuleSync(node)
  cancelRuleRefreshes()
  resetRuleSync()
  setRpc(null)
})
const online = (node = B): void => useNodes.getState().patch(node, { conn: 'ready' })

describe('最后期望与重放', () => {
  it('只取配对过的非空同组；同一目标规则合并并按最后写下的顺序持久化', async () => {
    expect(groupPeers(A)).toEqual([B])
    expect(groupPeers(D)).toEqual([])
    await enqueueRuleSync([B], 'one', desired)
    await enqueueRuleSync([B], 'two', desired)
    await enqueueRuleSync([B], 'one', { ...desired, text: 'new' })
    resetRuleSync()
    await reloadRuleSync()
    expect(useRuleSync.getState().records.map((r) => [r.rule_id, r.desired])).toEqual([['two', desired], ['one', { ...desired, text: 'new' }]])
    await enqueueRuleSync([B], 'one', 'deleted')
    expect((await db.listRuleSync()).filter((r) => r.rule_id === 'one')).toMatchObject([{ desired: 'deleted' }])
  })

  it('重连重放 add；已存在且内容、状态、范围不同则 update，不比较节点时钟', async () => {
    await enqueueRuleSync([B], 'one', desired)
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', text: 'old', status: 'disabled', updated_ms: 99999999 })
    rpc.handlers['rule.update'] = () => ruleItem({ id: 'one', ...desired })
    online()
    await resumeRuleSync(B)
    expect(rpc.methods()).toEqual(['rule.add', 'rule.update'])
    expect(rpc.calls.map((r) => r.params)).toEqual([{ id: 'one', ...desired }, { id: 'one', ...desired }])
    expect(await db.listRuleSync()).toEqual([])
    expect(useRules.getState().nodes[B]?.items.one?.sync).toBe('group')
  })

  it('add 已符合期望不 update；删除 not_found 成功', async () => {
    await enqueueRuleSync([B], 'one', { ...desired, status: 'rejected' })
    await enqueueRuleSync([B], 'two', 'deleted')
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', ...desired, status: 'rejected' })
    rpc.handlers['rule.delete'] = () => { throw rpcError('not_found') }
    online()
    await replayRuleSync(B)
    expect(rpc.methods()).toEqual(['rule.add', 'rule.delete'])
    expect(await db.listRuleSync()).toEqual([])
  })

  it('旧节点忽略范围时保留失败，不能把没有应用的期望标成已同步', async () => {
    await enqueueRuleSync([B], 'one', desired)
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', text: desired.text })
    rpc.handlers['rule.update'] = () => ruleItem({ id: 'one', text: desired.text })
    online()
    await replayRuleSync(B)
    expect(await db.listRuleSync()).toMatchObject([{ node: B, state: 'failed', error: { code: 'unsupported' } }])
    await replayRuleSync(B)
    expect(rpc.methods()).toEqual(['rule.add', 'rule.update'])
  })

  it('失败保留原因且不自动重试，不挡其他规则；重试或放弃', async () => {
    await enqueueRuleSync([B], 'one', desired)
    await enqueueRuleSync([B], 'two', desired)
    rpc.handlers['rule.add'] = (p) => {
      if ((p as { id: string }).id === 'one') throw rpcError('invalid', 'sync', 'not_syncable')
      return ruleItem({ id: 'two', ...desired })
    }
    online()
    await replayRuleSync(B)
    const rec = (await db.listRuleSync())[0]!
    expect(rec).toMatchObject({ rule_id: 'one', state: 'failed', error: { code: 'invalid', field: 'sync', reason: 'not_syncable' } })
    await replayRuleSync(B)
    expect(rpc.calls).toHaveLength(2)
    useNodes.getState().patch(B, { conn: 'peer_offline' })
    await retryRuleRecord(rec)
    expect((await db.listRuleSync())[0]?.state).toBe('pending')
    await discardRuleRecord(rec)
    expect(await db.listRuleSync()).toEqual([])
  })

  it('超时留在队列，下次重连再发', async () => {
    await enqueueRuleSync([B], 'one', desired)
    rpc.handlers['rule.add'] = () => { throw new RpcCallError('timeout', 'timeout') }
    online()
    await replayRuleSync(B)
    expect((await db.listRuleSync())[0]?.state).toBe('pending')
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', ...desired })
    await resumeRuleSync(B)
    expect(await db.listRuleSync()).toEqual([])
  })

  it.each(['add', 'update', 'delete'] as const)('%s 在线超时达到上限标失败，不再重试且不挡后面的规则', async (operation) => {
    await enqueueRuleSync([B], 'one', operation === 'delete' ? 'deleted' : desired)
    await enqueueRuleSync([B], 'two', desired)
    rpc.handlers['rule.add'] = (p) => {
      const id = (p as { id: string }).id
      if (id === 'one' && operation === 'add') throw new RpcCallError('timeout', 'timeout')
      return ruleItem({ id, ...desired, ...(operation === 'update' && id === 'one' ? { text: 'old' } : {}) })
    }
    rpc.handlers['rule.update'] = rpc.handlers['rule.delete'] = () => { throw new RpcCallError('timeout', 'timeout') }
    online()
    for (let attempts = 1; attempts <= MAX_LINK_FAILURES; attempts++) {
      await replayRuleSync(B)
      expect((await db.listRuleSync())[0]).toMatchObject({ attempts, state: attempts === MAX_LINK_FAILURES ? 'failed' : 'pending' })
      // 模块重建后继续用落盘的计数；add 的幂等成功不清掉 update 的失败次数。
      resetRuleSync()
      await reloadRuleSync()
      expect(useRuleSync.getState().records[0]?.attempts).toBe(attempts)
    }
    expect(await db.listRuleSync()).toMatchObject([{ rule_id: 'one', error: { code: 'link' } }])
    expect(await db.listRuleSync()).toHaveLength(1)
    const sent = rpc.calls.length
    await replayRuleSync(B)
    expect(rpc.calls).toHaveLength(sent)
  })

  it.each(['peer_reconnected', 'noise_reset'] as const)('节点侧 %s 断线达到上限标失败，重试清零', async (reason) => {
    await enqueueRuleSync([B], 'one', desired)
    rpc.handlers['rule.add'] = () => {
      useNodes.getState().patch(B, { conn: 'peer_offline' })
      throw new RpcCallError('closed', 'closed', undefined, reason)
    }
    for (let drops = 1; drops <= MAX_DROPS; drops++) {
      online()
      await replayRuleSync(B)
      expect((await db.listRuleSync())[0]).toMatchObject({ drops, state: drops === MAX_DROPS ? 'failed' : 'pending' })
    }
    const rec = (await db.listRuleSync())[0]!
    expect(rec.error).toEqual({ code: 'link' })
    await retryRuleRecord(rec)
    const retried = (await db.listRuleSync())[0]!
    expect(retried.state).toBe('pending')
    expect(retried.attempts).toBeUndefined()
    expect(retried.drops).toBeUndefined()
    expect(retried.error).toBeUndefined()
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', ...desired })
    online()
    await replayRuleSync(B)
    expect(await db.listRuleSync()).toEqual([])
  })

  it.each([
    new RpcCallError('closed', 'hub lost', undefined, 'hub_lost'),
    new RpcCallError('closed', 'peer offline', undefined, 'peer_offline'),
    new RpcCallError('offline', 'offline'),
    new RpcCallError('undeliverable', 'undeliverable'),
    new RpcCallError('timeout', 'offline timeout'),
  ])('离线和链路中断不累计超时或节点侧断线次数：%s', async (error) => {
    await db.putRuleSync([{ node: B, rule_id: 'one', desired, state: 'pending', attempts: 1, drops: 1 }])
    rpc.handlers['rule.add'] = () => {
      if (error.kind === 'timeout') useNodes.getState().patch(B, { conn: 'peer_offline' })
      throw error
    }
    for (let i = 0; i <= MAX_DROPS; i++) {
      online()
      await replayRuleSync(B)
      expect((await db.listRuleSync())[0]).toMatchObject({ state: 'pending', attempts: 1, drops: 1 })
    }
  })

  it('手动重试和新期望清零两种计数；一次完整成功后没有残留', async () => {
    await db.putRuleSync([{ node: B, rule_id: 'one', desired, state: 'failed', error: { code: 'link' }, attempts: MAX_LINK_FAILURES, drops: MAX_DROPS }])
    await retryRuleRecord((await db.listRuleSync())[0]!)
    expect((await db.listRuleSync())[0]?.attempts).toBeUndefined()
    expect((await db.listRuleSync())[0]?.drops).toBeUndefined()
    rpc.handlers['rule.add'] = () => { throw new RpcCallError('timeout', 'timeout') }
    online()
    await replayRuleSync(B)
    expect((await db.listRuleSync())[0]?.attempts).toBe(1)
    rpc.handlers['rule.add'] = () => ruleItem({ id: 'one', ...desired })
    await replayRuleSync(B)
    expect(await db.listRuleSync()).toEqual([])
    useNodes.getState().patch(B, { conn: 'peer_offline' })
    await db.putRuleSync([{ node: B, rule_id: 'one', desired, state: 'pending', attempts: 2, drops: 4 }])
    await enqueueRuleSync([B], 'one', 'deleted')
    const replaced = (await db.listRuleSync())[0]!
    expect(replaced.desired).toBe('deleted')
    expect(replaced.attempts).toBeUndefined()
    expect(replaced.drops).toBeUndefined()
  })

  it('在途回执不删新期望，最终发送新状态', async () => {
    await enqueueRuleSync([B], 'one', desired)
    let release: (() => void) | undefined
    rpc.handlers['rule.add'] = async () => {
      await new Promise<void>((resolve) => { release = resolve })
      return ruleItem({ id: 'one', ...desired })
    }
    online()
    const running = replayRuleSync(B)
    while (!release) await new Promise((r) => setTimeout(r, 1))
    await enqueueRuleSync([B], 'one', 'deleted')
    rpc.handlers['rule.delete'] = () => ({})
    release()
    await running
    // 同一实例可以接着发送自己的新期望，旧回执不能删掉新记录。
    const next = (await db.listRuleSync())[0]
    if (next) {
      expect(next.desired).toBe('deleted')
      await db.finishRuleSync(next.seq!, undefined, true)
      await replayRuleSync(B)
    }
    expect(await db.listRuleSync()).toEqual([])
    expect(rpc.methods()).toEqual(['rule.add', 'rule.delete'])
  })

  it('跨实例认领按节点互斥，替换在途记录仍保留租约，旧实例不能删除被接手的记录', async () => {
    await db.putRuleSync([{ node: B, rule_id: 'one', desired, state: 'pending' }])
    const first = await db.claimRuleSync(B, 60_000, 'first')
    expect(first?.claimed_by).toBe('first')
    await db.putRuleSync([{ node: B, rule_id: 'one', desired: 'deleted', state: 'pending' }, { node: B, rule_id: 'two', desired, state: 'pending' }])
    expect(await db.claimRuleSync(B, 60_000, 'second')).toBeNull()
    await db.finishRuleSync(first!.seq!)
    expect(await db.listRuleSync()).toHaveLength(2)
    const taken = await db.claimRuleSync(B, 0, 'second')
    expect(taken?.desired).toBe('deleted')
    await db.finishRuleSync(taken!.seq!, undefined, false, 'first')
    expect(await db.listRuleSync()).toHaveLength(2)
    await db.finishRuleSync(taken!.seq!, undefined, false, 'second')
    expect(await db.listRuleSync()).toHaveLength(1)
  })

  it('忘记节点和清空身份同时清除规则队列', async () => {
    await enqueueRuleSync([B, C], 'one', desired)
    await db.deleteNode(B)
    expect((await db.listRuleSync()).map((r) => r.node)).toEqual([C])
    await db.wipeAll()
    expect(await db.listRuleSync()).toEqual([])
  })
})

it.each(['offline', 'closed', 'undeliverable'] as const)('链路错误 %s 保留待发', (kind) => {
  expect(classifyRuleSyncError(desired, new RpcCallError(kind, kind))).toEqual({ kind: 'retain' })
})
it('超时只有在线才计次，只有节点侧的 closed 才记 drops', () => {
  const timeout = new RpcCallError('timeout', 'timeout')
  expect(classifyRuleSyncError(desired, timeout)).toEqual({ kind: 'retain', counter: 'attempts' })
  expect(classifyRuleSyncError(desired, timeout, false)).toEqual({ kind: 'retain' })
  expect(classifyRuleSyncError(desired, new RpcCallError('closed', 'closed', undefined, 'disposed'))).toEqual({ kind: 'retain' })
  expect(classifyRuleSyncError(desired, new RpcCallError('closed', 'closed', undefined, 'noise_reset'), false)).toEqual({ kind: 'retain', counter: 'drops' })
})
it.each(['over_budget', 'too_many', 'not_syncable', 'other'])('拒绝 %s 标失败并记原因', (reason) => {
  expect(classifyRuleSyncError(desired, rpcError('invalid', 'sync', reason))).toEqual({ kind: 'failed', error: { code: 'invalid', field: 'sync', reason } })
})
it('forbidden 标失败，只有删除的 not_found 才算成功', () => {
  expect(classifyRuleSyncError(desired, rpcError('forbidden')).kind).toBe('failed')
  expect(classifyRuleSyncError(desired, rpcError('not_found')).kind).toBe('failed')
  expect(classifyRuleSyncError('deleted', rpcError('not_found')).kind).toBe('done')
})

describe('对齐', () => {
  const rule = ruleItem({ id: 'one', ...desired })
  it('一致时不列，时间和来源不参与比较', () => {
    expect(compareGroupRules([{ node: A, rules: [rule] }, { node: B, rules: [{ ...rule, updated_ms: 4, source: { kind: 'answer' } }] }])).toEqual([])
  })
  it.each([{ text: 'different' }, { status: 'disabled' as const }, { sync: 'node' as const }])('文字、状态和范围变化列出差异 %o', (patch) => {
    expect(compareGroupRules([{ node: A, rules: [rule] }, { node: B, rules: [{ ...rule, ...patch }] }])).toHaveLength(1)
  })
  it('缺失与离线未知分开，只有 node 的 id 不参与', () => {
    const diff = compareGroupRules([{ node: A, rules: [rule, ruleItem({ id: 'local' })] }, { node: B, rules: [] }, { node: C, rules: null }])[0]!
    expect(diff.versions[1]).toMatchObject({ rule: null, unknown: false })
    expect(diff.versions[2]).toMatchObject({ rule: null, unknown: true })
    expect(diff.id).toBe('one')
  })
  it('对齐不越过实际空间边界', async () => {
    await alignRule([A, C], rule.id, rule, A)
    expect(await db.listRuleSync()).toEqual([])
  })
  it('在线才读，读取失败未知；选版本与都删掉均写入同一队列', async () => {
    rpc.handlers['rule.list'] = () => ({ rules: [rule] })
    const snapshots = await readGroupRules([A, B])
    expect(snapshots).toEqual([{ node: A, rules: [rule] }, { node: B, rules: null }])
    rpc.handlers['rule.list'] = () => { throw rpcError('forbidden') }
    expect((await readGroupRules([A]))[0]?.rules).toBeNull()
    await alignRule([A, B], rule.id, rule, A)
    expect(await db.listRuleSync()).toMatchObject([{ node: B, desired }])
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    await alignRule([A, B], rule.id, 'deleted')
    expect((await db.listRuleSync()).map((r) => r.desired)).toEqual(['deleted', 'deleted'])
  })
})

it('用户写成功才转发；包括状态变化和删除，改回 node 不动副本', async () => {
  rpc.handlers['rule.add'] = (p) => ruleItem({ id: 'one', ...desired, ...p as DesiredRule })
  rpc.handlers['rule.update'] = (p) => ruleItem({ id: 'one', ...desired, ...p as DesiredRule })
  rpc.handlers['rule.delete'] = () => ({})
  await addRule(A, { id: 'one', ...desired })
  expect(await db.listRuleSync()).toMatchObject([{ node: B, desired }])
  await updateRule(A, 'one', { status: 'disabled', sync: 'group' }, 1000)
  expect(await db.listRuleSync()).toMatchObject([{ node: B, desired: { status: 'disabled' } }])
  await updateRule(A, 'one', { sync: 'node', text: 'local only' }, 1000)
  expect(await db.listRuleSync()).toMatchObject([{ desired: { text: 'ask first', status: 'disabled' } }])
  await updateRule(A, 'one', { ...desired }, 1000)
  await deleteRule(A, 'one', 1000)
  expect(await db.listRuleSync()).toMatchObject([{ desired: 'deleted' }])
  rpc.handlers['rule.add'] = () => { throw rpcError('invalid', 'text', 'over_budget') }
  expect((await addRule(A, { id: 'two', ...desired })).ok).toBe(false)
  expect(await db.listRuleSync()).toHaveLength(1)
})
