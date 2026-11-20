// 给节点设分组、给空间改名（spec/web.md § 4.13）。分组名存在节点上（`[hub] group`，`node.set_config { hub.group }`，任何已配对设备都能改），
// 所以改名就是对这个空间里每个**在线**的节点各发一次；不在线的列为「没改」，由用户等它回来再试——不假装成功。
import type { ConfigChange, NodeConfigResult, NodeInfo } from '@/protocol'
import { groupError } from '@/lib/spaces'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'

/** 重新读一次 `node.info` 并更新（会同时落盘）：配置变了（分组、名字）没有通知，改完要自己刷新，空间切换才跟着变。 */
export async function refreshNodeInfo(nodeId: string): Promise<void> {
  const info = await rpc().request<NodeInfo>(nodeId, 'node.info', {})
  useNodes.getState().setInfo(nodeId, info)
}

export type GroupResult =
  | { status: 'changed' }
  | { status: 'unchanged' }
  | { status: 'offline' }
  /** 本地校验没过（没发出去）。 */
  | { status: 'invalid'; reason: 'too_long' | 'control' }
  /** 节点拒绝了：`invalid`（不合格）或 `forbidden` + `env_override`（被环境变量接管）；别的错误 `code` 是 `other`。 */
  | { status: 'rejected'; code: 'invalid' | 'forbidden' | 'other'; reason?: string }

/** 设一个节点的分组（空 = 清掉）。 */
export async function setNodeGroup(nodeId: string, raw: string): Promise<GroupResult> {
  const bad = groupError(raw)
  if (bad) return { status: 'invalid', reason: bad }
  const group = raw.trim()
  const entry = useNodes.getState().byId[nodeId]
  if (!entry || entry.conn !== 'ready') return { status: 'offline' }
  if ((entry.info?.group ?? '').trim() === group) return { status: 'unchanged' }
  const change: ConfigChange = { key: 'hub.group', value: group }
  try {
    await rpc().request<NodeConfigResult>(nodeId, 'node.set_config', { changes: [change] })
  } catch (e) {
    if (e instanceof RpcCallError && e.kind === 'rpc' && (e.code === 'invalid' || e.code === 'forbidden')) return { status: 'rejected', code: e.code, ...(e.reason ? { reason: e.reason } : {}) }
    if (e instanceof RpcCallError && e.kind !== 'rpc') return { status: 'offline' }
    return { status: 'rejected', code: 'other' }
  }
  // 配置变了没有通知：刷新 node.info，空间切换才会跟着变。刷新失败不算没改成（节点上已经写进去了），下次同步会补上
  await refreshNodeInfo(nodeId).catch((e: unknown) => console.warn('anycon: could not refresh node.info after changing the group', e))
  return { status: 'changed' }
}

/** 空间改名：这个空间里每个节点各设一次新名字；返回每个节点的结果（在线的改了，不在线的没改）。 */
export async function renameSpace(nodeIds: readonly string[], to: string): Promise<Array<{ nodeId: string; result: GroupResult }>> {
  const bad = groupError(to)
  if (bad) return nodeIds.map((nodeId) => ({ nodeId, result: { status: 'invalid', reason: bad } as GroupResult }))
  return Promise.all(nodeIds.map(async (nodeId) => ({ nodeId, result: await setNodeGroup(nodeId, to) })))
}
