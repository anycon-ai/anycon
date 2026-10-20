// 与某个节点建立 Noise 会话后的同步（spec/web.md § 6.3）：
//   node.info（版本检查）→ 并发：agent.list{if_none_match} / workspace.list / session.list 比对（→ 当前打开的会话
//   session.watch + session.history）/ permission.list / node.devices / node.enroll_list → 重发未发送消息
// 每一步都经过一次 浏览器 → hub → daemon 的往返，串行发要多花好几个 RTT；它们之间没有先后依赖，所以一起发。
// daemon 对同一客户端的请求仍按到达顺序处理（protocol.md § 7.3），并发只省掉网络上的等待。
import type { AgentListResult, NodeDevicesResult, NodeEnrollListResult, NodeInfo, PermissionListResult, SessionListResult, SessionSummary, WorkspaceListResult } from '@/protocol'
import * as db from '@/db'
import { PROTOCOL_VERSION } from '@/lib/constants'
import { storageFailed } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { parseEnrollInfo, useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { describeError } from '@/lib/errors'
import { timed } from '@/lib/timing'
import i18n from '@/i18n'
import { flushUnsent, syncOpenSession } from '@/features/session/actions'
import { planSessionSync } from './plan'

/** 协议版本：相邻一个 minor 兼容（plan/protocol.md § 9）。 */
export function protocolCompatible(remote: number, local: number = PROTOCOL_VERSION): boolean {
  return Math.abs(remote - local) <= 1
}

async function step<T>(what: string, f: () => Promise<T>): Promise<T | undefined> {
  try {
    return await timed(`rpc ${what}`, f)
  } catch (e) {
    // 链路断了没必要继续往下走；其它错误（not_found / unsupported 等）只影响这一步
    if (e instanceof RpcCallError && (e.kind === 'offline' || e.kind === 'closed' || e.kind === 'undeliverable')) throw e
    console.warn(`anycon: sync step "${what}" failed; continuing with the cached data`, e)
    return undefined
  }
}

/** 读本地缓存里这个节点的会话（App 启动时调用一次，让离线也有东西可看）。 */
export async function hydrateNodeSessions(nodeId: string): Promise<void> {
  try {
    useSessions.getState().hydrateNode(nodeId, await db.listSessions(nodeId))
  } catch (e) {
    console.warn('anycon: could not read the cached sessions; the list fills in when the node is online', e)
  }
}

export async function syncNode(nodeId: string): Promise<void> {
  const nodes = useNodes.getState()
  if (!nodes.byId[nodeId]) return
  nodes.patch(nodeId, { syncing: true })
  try {
    const info = await timed('rpc node.info', () => rpc().request<NodeInfo>(nodeId, 'node.info', {}))
    useNodes.getState().setInfo(nodeId, info)
    if (!protocolCompatible(info.protocol_version)) {
      useNodes.getState().patch(nodeId, { versionMismatch: true, syncing: false })
      return
    }
    useNodes.getState().patch(nodeId, { versionMismatch: false })

    const cachedEtag = useNodes.getState().byId[nodeId]?.agentsEtag
    // 链路断了任何一步都会抛出（见 step），Promise.all 随之失败；其余各步自己吞掉错误、只影响自己那一块
    await Promise.all([
      step('agent.list', () => rpc().request<AgentListResult>(nodeId, 'agent.list', cachedEtag ? { if_none_match: cachedEtag } : {})).then((agents) => {
        if (agents && !agents.not_modified) useNodes.getState().setAgents(nodeId, agents.agents, agents.etag)
      }),
      info.capabilities.workspace
        ? step('workspace.list', () => rpc().request<WorkspaceListResult>(nodeId, 'workspace.list', {})).then((ws) => {
            if (ws) useNodes.getState().setProjects(nodeId, ws.root, ws.projects)
          })
        : undefined,
      step('session.list', () => rpc().request<SessionListResult>(nodeId, 'session.list', { include_archived: true })).then(async (list) => {
        if (list) await applySessionList(nodeId, list.sessions)
        // 当前打开的会话：立刻追历史 + watch。要等会话列表比对完：追历史靠它给的 last_seq 判断缓存是否作废 / 落后太多
        for (const o of Object.values(useSessions.getState().open)) {
          if (o.nodeId === nodeId) void syncOpenSession(nodeId, o.sessionId)
        }
      }),
      step('permission.list', () => rpc().request<PermissionListResult>(nodeId, 'permission.list', {})).then((perms) => {
        if (perms) useSessions.getState().setPending(nodeId, perms.requests)
      }),
      step('node.devices', () => rpc().request<NodeDevicesResult>(nodeId, 'node.devices', {})).then((devices) => {
        if (devices) useNodes.getState().setDevices(nodeId, devices.devices)
      }),
      // 还在等批准的接入请求（打开页面 / 重连后补齐；之后靠 node.enroll_requested / enroll_resolved 增量）
      step('node.enroll_list', () => rpc().request<NodeEnrollListResult>(nodeId, 'node.enroll_list', {})).then((enrolls) => {
        if (enrolls) useEnroll.getState().replaceNode(nodeId, enrolls.enrolls.map(parseEnrollInfo).filter((i) => i !== null))
      }),
    ])
    await flushUnsent(nodeId)
    useNodes.getState().patch(nodeId, { synced: true, syncing: false })
  } catch (e) {
    // 链路断了是常态（会再同步一次）；其它错误让用户知道数据可能不是最新的
    console.warn(`anycon: syncing node ${nodeId.slice(0, 8)} failed`, e)
    useNodes.getState().patch(nodeId, { syncing: false })
    if (!(e instanceof RpcCallError && (e.kind === 'offline' || e.kind === 'closed' || e.kind === 'undeliverable'))) toast.error(i18n.t('errors.syncFailed', { error: describeError(e, i18n.t.bind(i18n)) }))
  }
}

export async function applySessionList(nodeId: string, remote: SessionSummary[]): Promise<void> {
  const st = useSessions.getState()
  const local: Record<string, { cachedSeq: number }> = {}
  for (const s of Object.values(st.list)) if (s.node_id === nodeId) local[s.id] = { cachedSeq: s.cached_seq }
  const plan = planSessionSync(local, remote)

  // 正在看的会话在列表里不见了：先确认再判定已删除（list 可能被截断）
  const deleted: string[] = []
  for (const id of plan.deleted) {
    if (st.open[id]) {
      const still = await rpc()
        .request<SessionSummary>(nodeId, 'session.get', { session_id: id })
        .catch((e: unknown) => (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found' ? null : undefined))
      if (still) {
        useSessions.getState().upsertSummaries(nodeId, [still])
        continue
      }
      if (still === undefined) continue
    }
    deleted.push(id)
  }
  if (deleted.length > 0) useSessions.getState().removeSessions(deleted, nodeId)

  for (const id of plan.reset) await db.clearEvents(id).catch(storageFailed('clear the stale event cache'))
  useSessions.getState().upsertSummaries(nodeId, plan.upsert)
  if (plan.reset.length > 0)
    useSessions.setState((s) => {
      const list = { ...s.list }
      for (const id of plan.reset) if (list[id]) list[id] = { ...list[id]!, cached_seq: 0 }
      return { list }
    })
}
