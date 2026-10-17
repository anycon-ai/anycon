// 在已配对的设备上处理别的设备发来的接入请求（ADR-36）：批准 / 拒绝 / 补读进行中的请求。
import type { NodeEnrollApproveParams, NodeEnrollApproveResult, NodeEnrollIdParams, NodeEnrollListResult } from '@/protocol'
import { RpcCallError } from '@/transport/rpcClient'
import { rpc } from '@/transport/instance'
import { parseEnrollInfo, useEnroll } from '@/stores/enroll'

/** `code` 请求：批准，daemon 返回 6 位码（只给批准的这一台）。 */
export function approveCode(nodeId: string, enrollId: string): Promise<NodeEnrollApproveResult> {
  const params: NodeEnrollApproveParams = { enroll_id: enrollId }
  return rpc().request<NodeEnrollApproveResult>(nodeId, 'node.enroll_approve', params)
}

/** `scan` 请求：带二维码里的密钥（hex）批准，不返回码。 */
export function approveScan(nodeId: string, enrollId: string, secretHex: string): Promise<NodeEnrollApproveResult> {
  const params: NodeEnrollApproveParams = { enroll_id: enrollId, secret: secretHex }
  return rpc().request<NodeEnrollApproveResult>(nodeId, 'node.enroll_approve', params)
}

export async function denyEnroll(nodeId: string, enrollId: string): Promise<void> {
  const params: NodeEnrollIdParams = { enroll_id: enrollId }
  await rpc().request(nodeId, 'node.enroll_deny', params)
}

/** `node.enroll_list`：把这个节点上进行中的请求同步进 store。返回读到的请求（读不到返回 null）。 */
export async function refreshEnrolls(nodeId: string): Promise<ReturnType<typeof parseEnrollInfo>[] | null> {
  try {
    const res = await rpc().request<NodeEnrollListResult>(nodeId, 'node.enroll_list', {})
    const infos = res.enrolls.map(parseEnrollInfo)
    useEnroll.getState().replaceNode(
      nodeId,
      infos.filter((i) => i !== null),
    )
    return infos
  } catch (e) {
    console.warn('anycon: could not read the pending enrollment requests; keeping the cached ones', e)
    return null
  }
}

/** 别的设备先处理了（`already_resolved`）。 */
export function isAlreadyResolved(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'already_resolved'
}

/** 请求已经不存在（过期 / 被清掉）。 */
export function isGone(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found'
}

/** 6 位码显示成 `482 913`。 */
export function groupCode(code: string): string {
  return code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code
}
