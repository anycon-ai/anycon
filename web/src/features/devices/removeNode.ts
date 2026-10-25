// 在这个浏览器里忘记一台节点：配对记录、会话缓存、内存里的 store。「从账号移除」（api/queries.ts 的 useRemoveNode）是先让 hub 吊销它，再忘记。
// 节点机器上的项目和会话历史都不动——那是 daemon 数据目录里的唯一副本（architecture.md § 4），浏览器碰不到。
import { getClient } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'

/** 只在这个浏览器里忘记节点。本地库删不掉时抛出（节点还在，调用方要告诉用户）。 */
export async function forgetNode(nodeId: string): Promise<void> {
  await getClient().removeNode(nodeId)
  const sessions = Object.values(useSessions.getState().list)
    .filter((s) => s.node_id === nodeId)
    .map((s) => s.id)
  if (sessions.length > 0) useSessions.getState().removeSessions(sessions, nodeId)
  useNodes.getState().remove(nodeId)
}
