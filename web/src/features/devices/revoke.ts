// 吊销一台设备要做两步（docs/spec/hub.md § 2.3 `DELETE /api/devices/:id`：UI 两步一起做）：让 daemon 删公钥（`node.revoke_device`），再让 hub 删路由 / 配对。
// 两步都要成功才算完，失败必须让用户看见——只删了一半的设备要么还能连 daemon、要么还挂在 hub 上。
import { ApiError, apiFetch } from '@/api/client'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'

/**
 * 让一个在线节点删掉设备 `deviceId` 的公钥（`node.revoke_device`）。
 * 节点说 `not_found`（这台设备本来就没配对 / 上一次已经删掉了）当作已完成——所以失败后重试是安全的，不会误报「那边还留着公钥」。
 */
export async function revokeDeviceOnNode(nodeId: string, deviceId: string, timeoutMs = 15_000): Promise<void> {
  try {
    await rpc().request(nodeId, 'node.revoke_device', { device_id: deviceId }, timeoutMs)
  } catch (e) {
    if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') return
    throw e
  }
}

export interface RevokeOutcome {
  /** 当时连不上的节点：hub 已经不再路由给这台设备，但这些节点那边还留着它的公钥，等它们上线后要再移除一次。 */
  offlineNodes: string[]
}

/**
 * 吊销设备 `deviceId`。`nodeIds`：要让哪些节点删掉它的公钥（只有在线的会被问到）。
 * 任何一个在线节点或 hub 返回错误都抛出，由调用方显示。daemon 说「这台设备本来就没配对」（not_found）当作已完成，所以失败后可以直接重试。
 */
export async function revokeDevice(deviceId: string, nodeIds: string[]): Promise<RevokeOutcome> {
  const offlineNodes: string[] = []
  for (const nodeId of nodeIds) {
    if (!rpc().isReady(nodeId)) {
      offlineNodes.push(nodeId)
      continue
    }
    await revokeDeviceOnNode(nodeId, deviceId)
  }
  try {
    await apiFetch(`/api/devices/${encodeURIComponent(deviceId)}`, { method: 'DELETE' })
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 404)) throw e // 404：hub 那边已经没有这台设备了
  }
  return { offlineNodes }
}
