// 规则的网络（docs/spec/daemon.md § 23，spec/web.md § 4.14）：连上节点时读一份（`rule.list`，带字数预算），之后靠 `rule.changed`；
// 用户写操作先发给来源节点，成功后把同组目标的期望状态持久化并重放。来源节点离线时禁用写入。
// `rule.changed` 不带字数预算和 `last_injected_ms` 的变化，所以每次变化之后隔一会儿重读一份（合并连续的变化）。
import type { Rule, RuleAddParams, RuleChanged, RuleListResult, RuleUpdateParams } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { nodeSupportsRules, useRules } from './store'
import { enqueueRuleSync, groupPeers, forgetRuleSyncNode, propagateRule, resetRuleSync } from './groupSync'
import { toast } from '@/stores/ui'
import i18n from '@/i18n'
import { newRuleId } from './validate'

/** `rule.changed` 之后隔多久重读一份（预算和 `last_injected_ms`）。 */
export const REFRESH_DELAY_MS = 400

const refreshTimers = new Map<string, ReturnType<typeof setTimeout>>()

/** 读一份整表。失败只记 `failed`，界面给「重试」。 */
export async function syncRules(nodeId: string): Promise<void> {
  try {
    const since = useRules.getState().tick
    const res = await rpc().request<RuleListResult>(nodeId, 'rule.list', {})
    useRules.getState().replaceAll(nodeId, res.rules, res.budget, since)
  } catch (e) {
    useRules.getState().patchNode(nodeId, (n) => ({ ...n, failed: true }))
    // 链路断了是常态（连上之后会再同步一次）
    if (!(e instanceof RpcCallError && e.kind !== 'rpc')) console.warn('anycon: could not read the rules', e)
  }
}

function scheduleRefresh(nodeId: string): void {
  if (refreshTimers.has(nodeId)) return
  refreshTimers.set(
    nodeId,
    setTimeout(() => {
      refreshTimers.delete(nodeId)
      if (nodeReady(nodeId)) void syncRules(nodeId)
    }, REFRESH_DELAY_MS),
  )
}

/** 测试收尾用：取消还没到点的重读。 */
export function cancelRuleRefreshes(): void {
  for (const t of refreshTimers.values()) clearTimeout(t)
  refreshTimers.clear()
}

/** `rule.changed`：并入内存（同一个 id 只留一份；`deleted` 删掉），再安排一次重读。 */
export function handleRuleChanged(nodeId: string, raw: unknown): void {
  if (typeof raw !== 'object' || raw === null) return
  const ch = raw as Partial<RuleChanged>
  if (typeof ch.id !== 'string' || ch.id === '' || !nodeSupportsRules(nodeId)) return
  const valid: RuleChanged = { id: ch.id, ...(ch.deleted ? { deleted: true } : {}), ...(ch.rule ? { rule: ch.rule } : {}) }
  if (!valid.deleted && (!valid.rule || valid.rule.id !== valid.id)) return
  useRules.getState().applyChanged(nodeId, valid)
  scheduleRefresh(nodeId)
}

export function forgetNodeRules(nodeId: string): void {
  const t = refreshTimers.get(nodeId)
  if (t) clearTimeout(t)
  refreshTimers.delete(nodeId)
  forgetRuleSyncNode(nodeId)
  useRules.getState().removeNode(nodeId)
}

export function resetRulesMemory(): void {
  cancelRuleRefreshes()
  resetRuleSync()
  useRules.getState().reset()
}

export const nodeReady = (nodeId: string): boolean => useNodes.getState().byId[nodeId]?.conn === 'ready'

/** 一次写失败的原因（界面把它翻成一句话，见 `ui/errors.ts`）。 */
export type RuleFail =
  | { kind: 'offline' }
  /** 这条规则在你读到之后被改过（别的设备）：`current` 是现在的样子（读不到就是 null，规则已经被删了）。 */
  | { kind: 'changed'; current: Rule | null }
  | { kind: 'over_budget'; bytes?: number; cap?: number }
  | { kind: 'too_many' }
  | { kind: 'too_many_pending' }
  | { kind: 'invisible' }
  | { kind: 'not_syncable' }
  | { kind: 'invalid'; field?: string }
  | { kind: 'not_found' }
  | { kind: 'forbidden' }
  | { kind: 'unsupported' }
  | { kind: 'link' }
  | { kind: 'failed' }

export type RuleResult = { ok: true; rule: Rule } | { ok: false; fail: RuleFail }

async function classify(nodeId: string, id: string | null, e: unknown): Promise<RuleFail> {
  if (!(e instanceof RpcCallError)) return { kind: 'failed' }
  if (e.kind !== 'rpc') return { kind: e.kind === 'offline' ? 'offline' : 'link' }
  const reason = e.reason
  switch (e.code) {
    case 'invalid':
      if (reason === 'changed' && id) {
        // 现在的样子：重读一份整表，给界面显示新版本
        await syncRules(nodeId)
        return { kind: 'changed', current: useRules.getState().nodes[nodeId]?.items[id] ?? null }
      }
      if (reason === 'over_budget') return { kind: 'over_budget', bytes: e.bytes, cap: e.cap }
      if (reason === 'too_many') return { kind: 'too_many' }
      if (reason === 'too_many_pending') return { kind: 'too_many_pending' }
      if (reason === 'not_syncable') return { kind: 'not_syncable' }
      if (reason === 'invisible') return { kind: 'invisible' }
      return { kind: 'invalid', field: e.field }
    case 'not_found':
      return { kind: 'not_found' }
    case 'forbidden':
      return { kind: 'forbidden' }
    case 'unsupported':
      return { kind: 'unsupported' }
    default:
      return { kind: 'failed' }
  }
}

async function send<R>(nodeId: string, id: string | null, run: () => Promise<R>): Promise<{ ok: true; value: R } | { ok: false; fail: RuleFail }> {
  if (!nodeReady(nodeId)) return { ok: false, fail: { kind: 'offline' } }
  try {
    return { ok: true, value: await run() }
  } catch (e) {
    return { ok: false, fail: await classify(nodeId, id, e) }
  }
}

export interface NewRule {
  /** 重试用同一个 id（`rule.add` 按它幂等）；缺省生成一个。 */
  id?: string
  text: string
  project?: string | null
  sync?: 'node' | 'group'
  status?: 'active' | 'pending'
  source?: RuleAddParams['source']
}

export async function addRule(nodeId: string, r: NewRule): Promise<RuleResult> {
  const params: RuleAddParams = {
    id: r.id ?? newRuleId(),
    text: r.text,
    ...(r.project && r.project.trim() ? { project: r.project } : {}),
    ...(r.status ? { status: r.status } : {}),
    ...(r.sync ? { sync: r.sync } : {}),
    ...(r.source ? { source: r.source } : {}),
  }
  const res = await send(nodeId, null, () => rpc().request<Rule>(nodeId, 'rule.add', params))
  if (!res.ok) return res
  useRules.getState().applyChanged(nodeId, { id: res.value.id, rule: res.value, deleted: false })
  scheduleRefresh(nodeId)
  await propagateRule(nodeId, res.value).catch(syncStorageError)
  return { ok: true, rule: res.value }
}

/** 要改的字段（只带改了的）。`project: null` 清掉范围（变成全节点）。 */
export interface RuleEdit {
  text?: string
  project?: string | null
  status?: 'pending' | 'active' | 'disabled' | 'rejected'
  sync?: 'node' | 'group'
  /** 只能给 `false`：恢复过期的规则（和变成生效一样过字数检查）。 */
  stale?: false
}

/** 修改。`ifUpdatedMs` 是用户看到这条规则时的版本：别的设备改过它就不动，返回 `changed` 和现在的样子。 */
export async function updateRule(nodeId: string, id: string, edit: RuleEdit, ifUpdatedMs: number): Promise<RuleResult> {
  const params: RuleUpdateParams = {
    id,
    ...(edit.text !== undefined ? { text: edit.text } : {}),
    ...(typeof edit.project === 'string' ? { project: edit.project } : {}),
    ...(edit.project === null ? { clear: ['project'] } : {}),
    ...(edit.status ? { status: edit.status } : {}),
    ...(edit.sync ? { sync: edit.sync } : {}),
    ...(edit.stale === false ? { stale: false } : {}),
    if_updated_ms: ifUpdatedMs,
  }
  const res = await send(nodeId, id, () => rpc().request<Rule>(nodeId, 'rule.update', params))
  if (!res.ok) return res
  useRules.getState().applyChanged(nodeId, { id, rule: res.value, deleted: false })
  scheduleRefresh(nodeId)
  await propagateRule(nodeId, res.value).catch(syncStorageError)
  return { ok: true, rule: res.value }
}

export async function deleteRule(nodeId: string, id: string, ifUpdatedMs: number): Promise<{ ok: true } | { ok: false; fail: RuleFail }> {
  const previous = useRules.getState().nodes[nodeId]?.items[id]
  const res = await send(nodeId, id, () => rpc().request<Record<string, never>>(nodeId, 'rule.delete', { id, if_updated_ms: ifUpdatedMs }))
  if (!res.ok) return res
  useRules.getState().applyChanged(nodeId, { id, deleted: true })
  scheduleRefresh(nodeId)
  if (previous?.sync === 'group') await enqueueRuleSync(groupPeers(nodeId), id, 'deleted').catch(syncStorageError)
  return { ok: true }
}

/** 一批规则改成同一个状态（多选确认 / 否决）：一条一条发，各带它自己的版本；返回成功的条数和失败的（原因各不同）。 */
export async function setStatusMany(rules: readonly { node_id: string; id: string; updated_ms: number }[], status: 'active' | 'rejected', sync?: 'node' | 'group'): Promise<{ done: number; failed: Array<{ id: string; fail: RuleFail }> }> {
  let done = 0
  const failed: Array<{ id: string; fail: RuleFail }> = []
  for (const r of rules) {
    const res = await updateRule(r.node_id, r.id, { status, ...(sync ? { sync } : {}) }, r.updated_ms)
    if (res.ok) done++
    else failed.push({ id: r.id, fail: res.fail })
  }
  return { done, failed }
}

function syncStorageError(e: unknown): void {
  console.warn('anycon: could not save group rule synchronization', e)
  toast.error(i18n.t('rules.group.storageFailed'))
}
