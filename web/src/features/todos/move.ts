// 把一项移到另一个节点（docs/spec/daemon.md § 21.7，spec/web.md § 4.13）。每个节点各有一份清单，daemon 之间不通信，所以移动是客户端的事：
// 先在目标节点 `todo.add`（**新的 id**，标题 / 正文 / 期望时间照搬，**项目清掉**——项目名在另一个节点上不一定是同一个项目，不带来源会话；
// 不是 agent 加的：移过去的这一份是用户自己的），已关闭的项再在目标上改回关闭；目标**确认了**之后才在原节点 `todo.delete`。
// 中间任何一步失败，这一项都留在原节点：宁可两个节点上各有一份，不丢。这里直接发请求，**不经离线待发队列**——队列的重放顺序没法保证
// 「目标确认了才删原节点」，所以两个节点都要在线、这一项也不能有没发出去的改动。
import type { Todo, TodoState } from '@/protocol'
import { groupOf } from '@/lib/spaces'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { dirtyFields, draftKey, useDrafts } from './drafts'
import { applyConfirmedDelete, applyConfirmedTodo } from './service'
import { nodeSupportsTodo, useTodos } from './store'
import type { ViewTodo } from './types'
import { newTodoId } from './validate'
import i18n from '@/i18n'
import { shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { cleanHintText } from '@/lib/hint'
import { useUi } from '@/stores/ui'
import { outboxErrorText } from './ui/errors'

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

/**
 * 一次移动做到了哪一步。同一次移动（包括失败后再试）共用一份：目标上的 id 在整次移动里不变（`todo.add` 按 id 幂等，丢了回执再发同一条不会多出
 * 一份），做完的步骤不重做（重试从失败的那一步接着做），而不是从头再来、再建一份。
 */
export interface MoveProgress {
  newId: string
  /** 目标上已经有了（`todo.add` 得到了确认）。 */
  added: Todo | null
  /** 状态已经在目标上补好（`open` 的项不需要）。 */
  stateRestored: boolean
}

export const newMoveProgress = (): MoveProgress => ({ newId: newTodoId(), added: null, stateRestored: false })

export type MoveResult =
  | { ok: true; newId: string; /** 原节点那一项在这期间已经被别处删掉了：目标上的这一份还在，但这不是一次干净的移动。 */ sourceAlreadyGone: boolean }
  | { ok: false; stage: 'blocked'; block: MoveBlock }
  /** 打开对话框之后这一项变了（或没了）：什么都没做（`copied`：目标上已经有一份了，原节点的没删）。 */
  | { ok: false; stage: 'changed'; gone: boolean; copied: boolean }
  /** 目标没有接受（`todo.add` 被拒）：什么都没变。 */
  | { ok: false; stage: 'rejected'; code: string; field?: string; reason?: string }
  /** 请求在途时链路断了 / 超时：不知道目标有没有收下，可能两个节点上各有一份；原节点的还在。 */
  | { ok: false; stage: 'link' }
  /** 目标上有了，但补状态没成：目标上那一份是打开的（不是原来的状态），原节点没动。`maybe`：补状态的请求断了 / 超时，状态可能已经补上了。 */
  | { ok: false; stage: 'state'; maybe: boolean }
  /** 目标上有了，原节点的删不掉（或删之前原节点那一项变了）：两个节点上各有一份。 */
  | { ok: false; stage: 'stuck' }

const isLink = (e: unknown): boolean => e instanceof RpcCallError && e.kind !== 'rpc'

/** 原节点缓存里这一项现在的样子（基础的那份，不含待发改动）；没有就是 `undefined`。 */
const liveBase = (node: string, id: string): Todo | undefined => useTodos.getState().nodes[node]?.items[id]

/**
 * 移动。返回结果，不抛；非干净成功的结果（失败、半途停下、原节点那一项在这期间被删）同时用一条提示报告出来，
 * 带「打开」直达那一项——结果不依赖对话框还挂着。`progress` 给同一次移动的上一次尝试留下的进度：重试从失败的那一步接着做。
 */
export async function moveTodo(item: ViewTodo, target: string, progress: MoveProgress = newMoveProgress()): Promise<MoveResult> {
  const r = await run(item, target, progress)
  reportMove(item, target, r, progress)
  return r
}

async function run(item: ViewTodo, target: string, progress: MoveProgress): Promise<MoveResult> {
  const block = moveBlock(item, target)
  if (block) return { ok: false, stage: 'blocked', block }
  // 对话框里显示的是打开它那一刻的样子：发之前再读一次，不一样了（别的设备改了 / 删了）就不动手，不然会把别处的改动丢掉、把删掉的项又建回来
  const live = liveBase(item.node_id, item.id)
  if (!live) return { ok: false, stage: 'changed', gone: true, copied: progress.added !== null }
  if (live.updated_ms !== item.updated_ms) return { ok: false, stage: 'changed', gone: false, copied: progress.added !== null }
  const newId = progress.newId
  if (!progress.added) {
    try {
      progress.added = await rpc().request<Todo>(target, 'todo.add', { id: newId, title: item.title, body: item.body, ...(item.due_ms != null ? { due_ms: item.due_ms } : {}) })
    } catch (e) {
      if (isLink(e)) return { ok: false, stage: 'link' }
      return { ok: false, stage: 'rejected', ...(e instanceof RpcCallError ? { code: e.code ?? 'error', ...(e.field ? { field: e.field } : {}), ...(e.reason ? { reason: e.reason } : {}) } : { code: 'error' }) }
    }
    await applyConfirmedTodo(target, progress.added)
  }
  // 已关闭 / 处理中的项：状态在目标上补一次（`todo.add` 只会建出「打开」的）。补不上就停在这里：原节点的还在，目标上多了一份打开的
  if (item.state !== 'open' && !progress.stateRestored) {
    try {
      const updated = await rpc().request<Todo>(target, 'todo.update', { id: newId, state: item.state })
      progress.stateRestored = true
      progress.added = updated
      await applyConfirmedTodo(target, updated)
    } catch (e) {
      return { ok: false, stage: 'state', maybe: isLink(e) }
    }
  }
  // 删之前再看一眼原节点：这期间变了，删掉就丢了那处改动
  const again = liveBase(item.node_id, item.id)
  if (again && again.updated_ms !== item.updated_ms) return { ok: false, stage: 'stuck' }
  let sourceAlreadyGone = false
  try {
    await rpc().request(item.node_id, 'todo.delete', { id: item.id })
  } catch (e) {
    // 原节点那一项已经不在了（别处删了）：目标上有了，原节点没有，结果是想要的，但不是一次干净的移动，要说出来
    if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') sourceAlreadyGone = true
    else return { ok: false, stage: 'stuck' }
  }
  await applyConfirmedDelete(item.node_id, item.id)
  useDrafts.getState().clear(draftKey(item.node_id, item.id))
  return { ok: true, newId, sourceAlreadyGone }
}

/** 结果 → 一句话（对话框和提示共用）。 */
export function moveText(r: Exclude<MoveResult, { ok: true; sourceAlreadyGone: false }>, node: string, state?: TodoState): string {
  const t = i18n.t.bind(i18n)
  if (r.ok) return t('spaces.move.sourceGone', { node })
  switch (r.stage) {
    case 'blocked':
      return t(`spaces.move.blocked.${r.block === 'unsupported' ? 'offlineTarget' : r.block}`, { node })
    case 'changed':
      return t(r.gone ? 'spaces.move.changedGone' : 'spaces.move.changed', { node })
    case 'rejected':
      return t('spaces.move.failedAdd', { node, reason: outboxErrorText(t as never, { code: r.code, ...(r.field ? { field: r.field } : {}), ...(r.reason ? { reason: r.reason } : {}) }) })
    case 'link':
      return t('spaces.move.failedLink', { node })
    case 'state':
      return t(r.maybe ? 'spaces.move.stateMaybe' : 'spaces.move.stateFailed', { node, state: t(`todos.states.${state ?? 'done'}`) })
    case 'stuck':
      return t('spaces.move.stuck', { node })
  }
}

/** 非干净成功的结果弹一条提示（`role="alert"`），带「打开」去那一项：对话框不管还在不在，用户都知道发生了什么。 */
function reportMove(item: ViewTodo, target: string, r: MoveResult, progress: MoveProgress): void {
  if (r.ok && !r.sourceAlreadyGone) return
  if (!r.ok && r.stage === 'blocked') return // 没动手，对话框里已经写明
  const nodes = useNodes.getState()
  const node = cleanHintText(nodes.byId[target]?.name) || target.slice(0, 8)
  const order = nodes.order
  // 去哪里看：原节点的那一项还在就去它；已经没了（原节点被删了）就去目标上的那份
  const openTarget = r.ok || (!r.ok && r.stage === 'changed' && r.gone)
  const url = openTarget && progress.added ? `/todos/${shortNodeId(target, order)}/${progress.newId}` : `/todos/${shortNodeId(item.node_id, order)}/${item.id}`
  useUi.getState().push('error', moveText(r as never, node, item.state), { label: i18n.t('todos.review'), run: () => navigateTo(url) }, 15_000)
}

/** 移过去的这一份是用户自己的：原来是 agent 加的（收下之后）的项要在确认里说一声。 */
export const wasAgentAdded = (item: ViewTodo): boolean => item.source.kind === 'agent' && !item.unreviewed

