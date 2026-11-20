// 把一项移到另一个节点（docs/spec/daemon.md § 21.7，spec/web.md § 4.13）。每个节点各有一份清单，daemon 之间不通信，所以移动是客户端的事：
// 先在目标节点 `todo.add`（**新的 id**，标题 / 正文 / 期望时间照搬，**项目清掉**——项目名在另一个节点上不一定是同一个项目，不带来源会话；
// 不是 agent 加的：移过去的这一份是用户自己的），已关闭的项再在目标上改回关闭；目标**确认了**之后才在原节点 `todo.delete`。
// 中间任何一步失败，这一项都留在原节点：宁可两个节点上各有一份，不丢。这里直接发请求，**不经离线待发队列**——队列的重放顺序没法保证
// 「目标确认了才删原节点」，所以两个节点都要在线、这一项也不能有没发出去的改动。
import type { Todo } from '@/protocol'
import { groupOf } from '@/lib/spaces'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { dirtyFields, draftKey, useDrafts } from './drafts'
import { applyConfirmedDelete, applyConfirmedTodo } from './service'
import { nodeSupportsTodo, useTodos } from './store'
import type { ViewTodo } from './types'
import { newTodoId } from './validate'

export type MoveBlock = 'offlineSource' | 'offlineTarget' | 'queued' | 'unreviewed' | 'draft' | 'unsupported'

/** 现在能不能移：能返回 null，不能返回原因（界面据此说明为什么）。`target` 还没选时只查和这一项自己有关的。 */
export function moveBlock(item: ViewTodo, target?: string): MoveBlock | null {
  const st = useTodos.getState()
  const nodes = useNodes.getState().byId
  // 没收下的 agent 项移过去会被目标节点记成用户自己加的，丢了「agent 加的」这个标记：先收下
  if (item.unreviewed && item.source.kind === 'agent') return 'unreviewed'
  const d = useDrafts.getState().drafts[draftKey(item.node_id, item.id)]
  if (d && dirtyFields(d).length > 0) return 'draft'
  if (item.unsynced || st.outbox.some((r) => r.node_id === item.node_id && r.id === item.id)) return 'queued'
  if (nodes[item.node_id]?.conn !== 'ready') return 'offlineSource'
  if (target !== undefined) {
    if (!nodeSupportsTodo(target)) return 'unsupported'
    if (nodes[target]?.conn !== 'ready') return 'offlineTarget'
  }
  return null
}

/** 移过去要不要先让用户确认：目标节点和原节点不在同一个空间（分组不同，包括一边没有分组）。 */
export function crossesSpace(sourceNode: string, targetNode: string): boolean {
  const nodes = useNodes.getState().byId
  return groupOf(nodes[sourceNode]?.info) !== groupOf(nodes[targetNode]?.info)
}

export type MoveResult =
  | { ok: true; newId: string }
  | { ok: false; stage: 'blocked'; block: MoveBlock }
  /** 目标没有接受（`todo.add` 被拒）：什么都没变。 */
  | { ok: false; stage: 'rejected'; reason: string }
  /** 请求在途时链路断了 / 超时：不知道目标有没有收下，可能两个节点上各有一份；原节点的还在。 */
  | { ok: false; stage: 'link'; copied: boolean }
  /** 目标上已经有了，原节点的删不掉：两个节点上各有一份。 */
  | { ok: false; stage: 'stuck' }

const isLink = (e: unknown): boolean => e instanceof RpcCallError && e.kind !== 'rpc'

/** 移动。返回结果，不抛。 */
export async function moveTodo(item: ViewTodo, target: string): Promise<MoveResult> {
  const block = moveBlock(item, target)
  if (block) return { ok: false, stage: 'blocked', block }
  const newId = newTodoId()
  let created: Todo
  try {
    created = await rpc().request<Todo>(target, 'todo.add', { id: newId, title: item.title, body: item.body, ...(item.due_ms != null ? { due_ms: item.due_ms } : {}) })
  } catch (e) {
    if (isLink(e)) return { ok: false, stage: 'link', copied: false }
    return { ok: false, stage: 'rejected', reason: e instanceof RpcCallError ? (e.reason ?? e.code ?? 'error') : 'error' }
  }
  // 已关闭 / 处理中的项：状态在目标上补一次（`todo.add` 只会建出「打开」的）。补不上就停在这里：原节点的还在，目标上多了一份打开的
  if (item.state !== 'open') {
    try {
      created = await rpc().request<Todo>(target, 'todo.update', { id: newId, state: item.state })
    } catch (e) {
      await applyConfirmedTodo(target, created)
      return isLink(e) ? { ok: false, stage: 'link', copied: true } : { ok: false, stage: 'stuck' }
    }
  }
  await applyConfirmedTodo(target, created)
  try {
    await rpc().request(item.node_id, 'todo.delete', { id: item.id })
  } catch (e) {
    // 原节点那一项已经不在了（别处删了）：目标上有了、原节点没有，正好是想要的结果
    if (!(e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found')) return { ok: false, stage: 'stuck' }
  }
  await applyConfirmedDelete(item.node_id, item.id)
  useDrafts.getState().clear(draftKey(item.node_id, item.id))
  return { ok: true, newId }
}

/** 移过去的这一份是用户自己的：原来是 agent 加的（收下之后）的项要在确认里说一声。 */
export const wasAgentAdded = (item: ViewTodo): boolean => item.source.kind === 'agent' && !item.unreviewed

