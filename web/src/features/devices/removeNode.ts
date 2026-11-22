// 在这个浏览器里忘记一台节点：配对记录、会话缓存、内存里的 store。「从账号移除」（api/queries.ts 的 useRemoveNode）是先让 hub 吊销它，再忘记。
// 节点机器上的项目和会话历史都不动——那是 daemon 数据目录里的唯一副本（architecture.md § 4），浏览器碰不到。
import { getClient } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useDrafts } from '@/features/todos/drafts'
import { forgetTwin } from '@/features/twin/service'
import { forgetNodeRules } from '@/features/rules/service'
import { forgetNodeTodos } from '@/features/todos/service'
import { useTodos } from '@/features/todos/store'
import { toast } from '@/stores/ui'
import i18n from '@/i18n'

/** 只在这个浏览器里忘记节点。本地库删不掉时抛出（节点还在，调用方要告诉用户）。 */
export async function forgetNode(nodeId: string): Promise<void> {
  // 这台节点上还没同步的清单改动（库里的缓存和队列由 `removeNode` 一起删）：丢掉之前数一数，告诉用户
  const unsynced = useTodos.getState().outbox.filter((r) => r.node_id === nodeId).length
  await getClient().removeNode(nodeId)
  // 内存里的清单和待发改动一起忘掉（库里的已经随节点记录删了）；别的标签页收到通知也会放掉
  await forgetNodeTodos(nodeId)
  forgetTwin(nodeId)
  forgetNodeRules(nodeId)
  useDrafts.getState().clearNode(nodeId)
  if (unsynced > 0) toast.info(i18n.t('todos.forgotUnsynced', { count: unsynced }))
  const sessions = Object.values(useSessions.getState().list)
    .filter((s) => s.node_id === nodeId)
    .map((s) => s.id)
  if (sessions.length > 0) useSessions.getState().removeSessions(sessions, nodeId)
  useNodes.getState().remove(nodeId)
}
