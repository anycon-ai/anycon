// 同组规则由 Web 转发：先在来源节点写成功，再将各目标的最后期望持久化；重连时重放，拒绝留给用户处理。
import { create } from 'zustand'
import * as db from '@/db'
import type { Rule, RuleListResult } from '@/protocol'
import { RPC_TIMEOUT_MS } from '@/lib/constants'
import { navigateTo } from '@/lib/navigate'
import { groupOf } from '@/lib/spaces'
import { useNodes } from '@/stores/nodes'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useRules } from './store'
import { newRuleId } from './validate'
import { desiredRule, type DesiredRule, type RuleSyncRecord } from './syncTypes'

export const useRuleSync = create<{ records: RuleSyncRecord[]; review: string | null }>(() => ({ records: [], review: null }))
export const openRuleAlignment = (node: string): void => { useRuleSync.setState({ review: node }); navigateTo('/rules') }

/** 空分组自成一组；这里包含暂时离线、尚不支持规则的已配对目标，拒绝会在对齐中显示。 */
export function groupPeers(node: string): string[] {
  const { order, byId } = useNodes.getState()
  const group = groupOf(byId[node]?.info)
  return group ? order.filter((id) => id !== node && groupOf(byId[id]?.info) === group) : []
}

let epoch = 0
let channel: BroadcastChannel | null = null
function broadcast(): void {
  if (!channel && typeof BroadcastChannel !== 'undefined') {
    channel = new BroadcastChannel('anycon-rule-sync')
    channel.onmessage = () => { void reloadRuleSync().catch(reportError) }
  }
  channel?.postMessage(null)
}
const reportError = (e: unknown): void => console.warn('anycon: rule synchronization storage failed', e)

export async function reloadRuleSync(): Promise<void> {
  const since = epoch
  const records = await db.listRuleSync()
  if (since === epoch) useRuleSync.setState({ records })
}

/** 每个目标的最后状态在一个事务里写下，然后才尝试网络发送。 */
export async function enqueueRuleSync(nodes: readonly string[], id: string, desired: DesiredRule | 'deleted'): Promise<void> {
  if (nodes.length === 0) return
  await db.putRuleSync(nodes.map((node) => ({ node, rule_id: id, desired, state: 'pending' })))
  await reloadRuleSync()
  broadcast()
  for (const node of nodes) kickRuleReplay(node)
}

export async function propagateRule(node: string, rule: Rule): Promise<void> {
  const desired = desiredRule(rule)
  if (desired) await enqueueRuleSync(groupPeers(node), rule.id, desired)
}

// 与清单 outbox 的 MAX_LINK_FAILURES / MAX_DROPS 保持一致。
export const MAX_LINK_FAILURES = 3
export const MAX_DROPS = 5

export type SyncVerdict = { kind: 'retain'; counter?: 'attempts' | 'drops' } | { kind: 'done' } | { kind: 'failed'; error: NonNullable<RuleSyncRecord['error']> }
export function classifyRuleSyncError(desired: RuleSyncRecord['desired'], e: unknown, ready = true): SyncVerdict {
  if (e instanceof RpcCallError) {
    if (e.kind === 'closed' && (e.closeReason === 'peer_reconnected' || e.closeReason === 'noise_reset')) return { kind: 'retain', counter: 'drops' }
    if (e.kind === 'timeout' && ready) return { kind: 'retain', counter: 'attempts' }
    if (['offline', 'closed', 'undeliverable', 'timeout'].includes(e.kind)) return { kind: 'retain' }
    if (e.kind === 'rpc' && e.code === 'not_found' && desired === 'deleted') return { kind: 'done' }
    return { kind: 'failed', error: { code: e.code ?? e.kind, ...(e.field ? { field: e.field } : {}), ...(e.reason ? { reason: e.reason } : {}) } }
  }
  return { kind: 'failed', error: { code: 'unknown' } }
}

const OWNER = newRuleId()
const running = new Map<string, Promise<void>>()
const retries = new Map<string, ReturnType<typeof setTimeout>>()
function scheduleRetry(node: string): void {
  if (retries.has(node)) return
  retries.set(node, setTimeout(() => { retries.delete(node); kickRuleReplay(node) }, RPC_TIMEOUT_MS + 5_000))
}
export function kickRuleReplay(node: string): void {
  void replayRuleSync(node).catch(reportError)
}

/** 顺序重放；各节点时钟不可比较，因此 update 不带 if_updated_ms。 */
export function replayRuleSync(node: string): Promise<void> {
  const active = running.get(node)
  if (active) return active.then(() => replayRuleSync(node))
  const since = epoch
  const work = async (): Promise<void> => {
    const locks = typeof navigator !== 'undefined' ? navigator.locks : undefined
    const run = async (): Promise<void> => {
      while (since === epoch && useNodes.getState().byId[node]?.conn === 'ready') {
        const rec = await db.claimRuleSync(node, locks ? 0 : RPC_TIMEOUT_MS * 2 + 5_000, OWNER)
        if (since !== epoch) return
        if (!rec) {
          if (useRuleSync.getState().records.some((r) => r.node === node && r.state === 'pending')) scheduleRetry(node)
          break
        }
        try {
          let failure: RuleSyncRecord['error']
          if (rec.desired === 'deleted') {
            await rpc().request(node, 'rule.delete', { id: rec.rule_id })
            if (since !== epoch || !useNodes.getState().byId[node]) return
            useRules.getState().applyChanged(node, { id: rec.rule_id, deleted: true })
          } else {
            const params = { id: rec.rule_id, ...rec.desired }
            let rule = await rpc().request<Rule>(node, 'rule.add', params)
            if (since !== epoch || !useNodes.getState().byId[node]) return
            if (rule.text !== rec.desired.text || rule.status !== rec.desired.status || rule.sync !== 'group' || rule.project) {
              rule = await rpc().request<Rule>(node, 'rule.update', params)
            }
            if (since !== epoch || !useNodes.getState().byId[node]) return
            useRules.getState().applyChanged(node, { id: rule.id, rule, deleted: false })
            // 旧节点可能忽略未知的 sync 字段；回执没有应用期望时不能标成已同步。
            if (rule.text !== rec.desired.text || rule.status !== rec.desired.status || rule.sync !== 'group' || rule.project) failure = { code: 'unsupported' }
          }
          await db.finishRuleSync(rec.seq!, failure, false, OWNER)
        } catch (e) {
          if (since !== epoch) return
          const verdict = classifyRuleSyncError(rec.desired, e, useNodes.getState().byId[node]?.conn === 'ready')
          const counter = verdict.kind === 'retain' ? verdict.counter : undefined
          const count = counter ? (rec[counter] ?? 0) + 1 : 0
          const exhausted = counter !== undefined && count >= (counter === 'attempts' ? MAX_LINK_FAILURES : MAX_DROPS)
          await db.finishRuleSync(rec.seq!, exhausted ? { code: 'link' } : verdict.kind === 'failed' ? verdict.error : undefined, verdict.kind === 'retain', OWNER, counter ? { [counter]: count } : {})
          if (verdict.kind === 'done') useRules.getState().applyChanged(node, { id: rec.rule_id, deleted: true })
          if (verdict.kind === 'retain' && !exhausted) {
            if (useNodes.getState().byId[node]?.conn === 'ready') scheduleRetry(node)
            break
          }
        }
        await reloadRuleSync()
        broadcast()
      }
      if (since === epoch) {
        await reloadRuleSync()
        broadcast()
      }
    }
    if (locks) await locks.request(`anycon-rule-replay:${node}`, run)
    else await run()
  }
  const task = work().finally(() => {
    running.delete(node)
    if (since === epoch && useNodes.getState().byId[node]?.conn === 'ready' && useRuleSync.getState().records.some((r) => r.node === node && r.state === 'pending')) scheduleRetry(node)
  })
  running.set(node, task)
  return task
}

export async function retryRuleRecord(rec: RuleSyncRecord): Promise<void> {
  if (rec.seq === undefined) return
  await db.retryRuleSync(rec.seq)
  await reloadRuleSync()
  broadcast()
  kickRuleReplay(rec.node)
}
export async function discardRuleRecord(rec: RuleSyncRecord): Promise<void> {
  if (rec.seq === undefined) return
  await db.finishRuleSync(rec.seq)
  await reloadRuleSync()
  broadcast()
}

export type NodeRuleSnapshot = { node: string; rules: readonly Rule[] | null }
export interface RuleDifference { id: string; versions: Array<{ node: string; rule: Rule | null; unknown: boolean }> }
/** 离线或读取失败为未知，绝不当作缺失；node 副本也参与同 id 的内容和范围比较。 */
export function compareGroupRules(snapshots: readonly NodeRuleSnapshot[]): RuleDifference[] {
  const ids = new Set(snapshots.flatMap((s) => (s.rules ?? []).filter((r) => r.sync === 'group').map((r) => r.id)))
  const differences: RuleDifference[] = []
  for (const id of ids) {
    const versions = snapshots.map((s) => ({ node: s.node, rule: s.rules?.find((r) => r.id === id) ?? null, unknown: s.rules === null }))
    const first = versions[0]?.rule
    if (versions.every((v) => !v.unknown && v.rule && first && v.rule.text === first.text && v.rule.status === first.status && v.rule.sync === 'group' && !v.rule.project)) continue
    differences.push({ id, versions })
  }
  return differences
}

/** 对齐始终从在线节点读取一份；失败与离线都显示未知。 */
export async function readGroupRules(nodes: readonly string[]): Promise<NodeRuleSnapshot[]> {
  return Promise.all(nodes.map(async (node) => {
    if (useNodes.getState().byId[node]?.conn !== 'ready') return { node, rules: null }
    try {
      const res = await rpc().request<RuleListResult>(node, 'rule.list', {})
      return { node, rules: useNodes.getState().byId[node]?.conn === 'ready' ? res.rules : null }
    } catch { return { node, rules: null } }
  }))
}

export async function alignRule(nodes: readonly string[], id: string, version: Rule | 'deleted', source?: string): Promise<void> {
  const desired = version === 'deleted' ? 'deleted' : desiredRule({ ...version, sync: 'group' })
  const anchor = source ?? nodes[0]
  const members = anchor ? [anchor, ...groupPeers(anchor)] : []
  if (desired) await enqueueRuleSync(nodes.filter((node) => node !== source && members.includes(node)), id, desired)
}

/** 重连读取规则时同时恢复持久化队列；不把通知当作新的用户写入。 */
export async function resumeRuleSync(node: string): Promise<void> {
  await reloadRuleSync()
  broadcast()
  await replayRuleSync(node)
}

/** 清理内存和计时器，持久化队列由身份与节点的数据库清理负责。 */
export function resetRuleSync(): void {
  epoch++
  for (const timer of retries.values()) clearTimeout(timer)
  retries.clear()
  channel?.close()
  channel = null
  useRuleSync.setState({ records: [], review: null })
}

/** 忘记节点时同时移除内存中的同步提示和重试计时器。 */
export function forgetRuleSyncNode(node: string): void {
  const timer = retries.get(node)
  if (timer) clearTimeout(timer)
  retries.delete(node)
  useRuleSync.setState((s) => ({ records: s.records.filter((r) => r.node !== node), review: s.review === node ? null : s.review }))
  broadcast()
}
