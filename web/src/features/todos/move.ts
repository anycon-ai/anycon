// 把一项移到另一个节点（docs/spec/daemon.md § 21.7，spec/web.md § 4.13）。每个节点各有一份清单，daemon 之间不通信，所以移动是客户端的事：
// 先在目标节点 `todo.add`（**新的 id**，标题 / 正文 / 期望时间照搬，**项目清掉**——项目名在另一个节点上不一定是同一个项目，不带来源会话；
// 不是 agent 加的：移过去的这一份是用户自己的），已关闭的项再在目标上改回关闭；目标**确认了**之后才在原节点 `todo.delete`。
// 中间任何一步失败，这一项都留在原节点：宁可两个节点上各有一份，不丢。这里直接发请求，**不经离线待发队列**——队列的重放顺序没法保证
// 「目标确认了才删原节点」，所以两个节点都要在线、这一项也不能有没发出去的改动。
//
// 一次移动是一台状态机，按「原节点 + 原 id」存在这个模块里（内存，不落盘），不属于任何对话框：对话框、行菜单、详情页只负责显示这里的状态、
// 向这里发命令（开始 / 接着做 / 放弃 / 删掉目标上的那一份）。关掉对话框、重新挂载、浏览器前进后退都不会起第二次并行的移动，也不会丢进度。
// 刷新页面之后半途的移动不会被记住，所以每一种不是干净成功的结果同时都有提示。
//
// 「目标上的那一份还在」「原节点那一项没变」都**在节点上检查**，不看这个标签页的缓存（缓存可能是旧的：已关闭的项重连后不会重新列、别的标签页
// 有没发出去的改动、重连之后还没同步完）：动手前 `todo.get` 原节点、删原节点之前 `todo.get` 目标，删原节点用 `todo.delete { if_updated_ms }`，
// 删目标上的那一份同样带条件。唯一剩下的窗口：删之前检查过目标那份还在、到删除请求到达原节点之间（一个来回）目标那份在别的设备上被删了。
import { create } from 'zustand'
import type { Todo, TodoState } from '@/protocol'
import { groupOf } from '@/lib/spaces'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { dirtyFields, draftKey, useDrafts } from './drafts'
import { applyConfirmedDelete, applyConfirmedTodo } from './service'
import { nodeSupportsTodo, useTodos } from './store'
import type { ViewTodo } from './types'
import { isHumanSource } from './source'
import { newTodoId } from './validate'
import i18n from '@/i18n'
import { resolveNodeParam, shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { cleanHintText } from '@/lib/hint'
import { toast, useUi } from '@/stores/ui'
import { outboxErrorText } from './ui/errors'

export type MoveBlock = 'offlineSource' | 'offlineTarget' | 'syncingSource' | 'syncingTarget' | 'queued' | 'unreviewed' | 'draft' | 'unsupported'

/**
 * 现在能不能移：能返回 null，不能返回原因（界面据此说明为什么）。`target` 还没选时只查和这一项自己有关的。
 * 两个节点都要在线**并且**这次连接已经同步完（重连之后、同步完之前缓存可能是旧的，待发队列也还没重放）。
 */
export function moveBlock(item: Pick<ViewTodo, 'node_id' | 'id' | 'unsynced' | 'unreviewed' | 'source'>, target?: string): MoveBlock | null {
  const st = useTodos.getState()
  const nodes = useNodes.getState().byId
  // 没收下的 agent 项移过去会被目标节点记成用户自己加的，丢了「agent 加的」这个标记：先收下
  if (item.unreviewed && !isHumanSource(item.source)) return 'unreviewed'
  const d = useDrafts.getState().drafts[draftKey(item.node_id, item.id)]
  if (d && dirtyFields(d).length > 0) return 'draft'
  if (item.unsynced || st.outbox.some((r) => r.node_id === item.node_id && r.id === item.id)) return 'queued'
  if (nodes[item.node_id]?.conn !== 'ready') return 'offlineSource'
  if (nodes[item.node_id]?.synced !== true) return 'syncingSource'
  if (target !== undefined) {
    if (!nodeSupportsTodo(target)) return 'unsupported'
    if (nodes[target]?.conn !== 'ready') return 'offlineTarget'
    if (nodes[target]?.synced !== true) return 'syncingTarget'
  }
  return null
}

/** 移过去要不要先让用户确认：目标节点和原节点不在同一个空间（分组不同，包括一边没有分组）。 */
export function crossesSpace(sourceNode: string, targetNode: string): boolean {
  const nodes = useNodes.getState().byId
  return groupOf(nodes[sourceNode]?.info) !== groupOf(nodes[targetNode]?.info)
}

/**
 * 目标节点上那一份现在是什么状况：
 * `none` 没有（没建过 / 被拒了）；`maybe` 请求在途时断了，不知道建没建；`open` 建了，但状态还没补成原来的；`full` 建好了、状态也对；
 * `gone` 建过，现在没了（被用户或别的设备删了）。
 */
export type CopyState = 'none' | 'maybe' | 'open' | 'full' | 'gone'

export type MoveFailure =
  | { stage: 'blocked'; block: MoveBlock }
  /** 原节点那一项变了（或没了）：什么都不再做，也不删它。`copy` 说目标上那时有没有一份。 */
  | { stage: 'changed'; gone: boolean }
  /** 读不了节点上的那一项（链路断了 / 超时 / 别的错误），没法确认：什么都不做。`side` 说是哪个节点。 */
  | { stage: 'unreadable'; side: 'source' | 'target' }
  /** 目标没有接受（`todo.add` 被拒）。 */
  | { stage: 'rejected'; code: string; field?: string; reason?: string }
  /** 请求在途时链路断了 / 超时：`add` 不知道目标有没有收下；`delete` 不知道原节点有没有删掉。 */
  | { stage: 'link'; at: 'add' | 'delete' }
  /** 目标上有了，补状态没成。`maybe`：补状态的请求断了 / 超时，状态可能已经补上了。 */
  | { stage: 'state'; maybe: boolean }
  /** 目标上有了，原节点的删被拒：两个节点上各有一份。 */
  | { stage: 'stuck' }
  /** 删原节点之前，目标上的那一份已经不在了：什么都没删。 */
  | { stage: 'copyGone' }
  /** 没料到的错误：`copy` 和 `at` 说它出在哪一步之后（出在请求发出之后的，按「可能已经生效」算）。 */
  | { stage: 'unexpected' }

export type MoveResult =
  | { ok: true; newId: string; /** 原节点那一项在这期间已经被别处删掉了：目标上的这一份还在，但这不是一次干净的移动。 */ sourceAlreadyGone: boolean }
  | ({ ok: false; copy: CopyState } & MoveFailure)

/** 「删掉目标上的那一份」的结果。和移动本身的结果分开存，不盖掉它（比如「原节点可能已经删掉了」）。 */
export type CopyDeleteOutcome =
  | { stage: 'blocked'; block: MoveBlock }
  /** 原节点那一项不在了或读不了：目标上这一份可能是仅存的一份，不删。 */
  | { stage: 'sourceGone' }
  /** 目标上那一份在这期间被改过：不删。 */
  | { stage: 'copyChanged' }
  | { stage: 'failed'; maybe: boolean }

export type MoveStage = 'idle' | 'checking' | 'adding' | 'restoring' | 'deleting' | 'removingCopy' | 'failed'

/** 一次移动。同一次移动里目标上的 id 不变（`todo.add` 按 id 幂等），做完的步骤不重做。 */
export interface MoveEntry {
  key: string
  sourceNode: string
  sourceId: string
  targetNode: string
  targetId: string
  /** 用户确认移动时原节点那一项的样子：目标上的内容照它来，之后每一步都拿它和原节点上现在的样子比。 */
  base: ViewTodo
  copy: CopyState
  /** 目标上那一份最后一次由我们的请求得到的 `updated_ms`（`add` / 补状态的回执）；不知道（请求断了）是 null。删目标上那一份时带它做条件。 */
  copyUpdatedMs: number | null
  stage: MoveStage
  /** 最近一次没成的结果（成功就整条记录删掉）。 */
  outcome: MoveResult | null
  /** 最近一次「删掉目标上那一份」没成的结果。 */
  copyOutcome: CopyDeleteOutcome | null
}

export const moveKey = (node: string, id: string): string => `${node}:${id}`

export const useMoves = create<{ moves: Record<string, MoveEntry> }>(() => ({ moves: {} }))

/** 这次移动已经在目标上留下东西了（目标不能再换；重试从失败的那一步接着做）。 */
export const isTouched = (e: MoveEntry | undefined): boolean => e !== undefined && e.copy !== 'none'
export const isRunning = (e: MoveEntry | undefined): boolean => e !== undefined && e.stage !== 'idle' && e.stage !== 'failed'

/**
 * 能不能给「删掉目标上的那一份」：目标上有一份、而且知道它最后一次的 `updated_ms`（不知道就没法带条件删，要先接着做把它弄清楚）；
 * 原节点那一项没有确认不在了或读不了，也不在「删原节点的请求断了、可能已经删掉」的状态（要先重新读过原节点）。
 */
export function canDeleteCopy(e: MoveEntry | undefined): boolean {
  if (!e || isRunning(e) || e.outcome === null || e.outcome.ok) return false
  if (e.copy !== 'open' && e.copy !== 'full') return false
  if (e.copyUpdatedMs === null) return false
  const o = e.outcome
  if (o.stage === 'link' && o.at === 'delete') return false
  if (o.stage === 'changed' && o.gone) return false
  if (o.stage === 'unreadable' || o.stage === 'copyGone' || o.stage === 'unexpected') return false
  return true
}

const inflight = new Map<string, Promise<MoveResult>>()

/** 测试用：丢掉所有移动的进度。 */
export function resetMoves(): void {
  inflight.clear()
  useMoves.setState({ moves: {} })
}

const get = (key: string): MoveEntry => {
  const e = useMoves.getState().moves[key]
  if (!e) throw new Error('move entry missing')
  return e
}

const patch = (key: string, p: Partial<MoveEntry>): void =>
  useMoves.setState((s) => (s.moves[key] ? { moves: { ...s.moves, [key]: { ...s.moves[key]!, ...p } } } : s))

const drop = (key: string): void =>
  useMoves.setState((s) => {
    const { [key]: _gone, ...rest } = s.moves
    return { moves: rest }
  })

const isLink = (e: unknown): boolean => e instanceof RpcCallError && e.kind !== 'rpc'
const isRpc = (e: unknown, code: string): boolean => e instanceof RpcCallError && e.kind === 'rpc' && e.code === code
/** `todo.delete` 的 `if_updated_ms` 对不上。 */
const isChanged = (e: unknown): boolean => isRpc(e, 'invalid') && (e as RpcCallError).reason === 'changed'

function fail(key: string, f: MoveFailure): MoveResult {
  const e = get(key)
  const r = { ok: false, copy: e.copy, ...f } as MoveResult
  patch(key, { stage: 'failed', outcome: r })
  return r
}

/** 节点上的这一项和用户确认时是不是一样：`updated_ms` 加上用户看得见的内容。 */
const sameAsBase = (live: Todo, base: Todo): boolean =>
  live.updated_ms === base.updated_ms && live.title === base.title && live.body === base.body && (live.due_ms ?? null) === (base.due_ms ?? null) && live.state === base.state

/** 向节点读一项：`not_found` 是 `'gone'`，别的错误（链路、超时、被拒）是 `'unreadable'`。 */
async function readOn(node: string, id: string): Promise<Todo | 'gone' | 'unreadable'> {
  try {
    return await rpc().request<Todo>(node, 'todo.get', { id })
  } catch (err) {
    return isRpc(err, 'not_found') ? 'gone' : 'unreadable'
  }
}

/** 前置条件：两个节点在线并已同步、没有排着的改动和没存的草稿。每一步之前、删之前再查一次。 */
function gate(key: string): MoveFailure | null {
  const e = get(key)
  const block = moveBlock(e.base, e.targetNode)
  return block ? { stage: 'blocked', block } : null
}

/**
 * 开始一次移动，或者接着做上一次没做完的：这一项已经有一次移动在跑就直接返回它（同一项同时只有一次）；上一次已经在目标上留下东西
 * （目标定了、id 定了）就接着做，`item` 和 `target` 都不再用；上一次什么都没留下（被拒、没动手）就用现在这一份重新开始。
 * 返回结果，不抛；不是干净成功的结果（失败、半途停下、原节点那一项在这期间被删）同时用一条提示报告出来，带「打开」直达那一项。
 */
export function moveTodo(item: ViewTodo, target: string): Promise<MoveResult> {
  const key = moveKey(item.node_id, item.id)
  const running = inflight.get(key)
  if (running) return running
  const cur = useMoves.getState().moves[key]
  if (!isTouched(cur)) {
    const entry: MoveEntry = {
      key,
      sourceNode: item.node_id,
      sourceId: item.id,
      targetNode: target,
      // 目标没换就沿用 id（上一次什么都没留下也不要紧：同一个 id 重发是幂等的）
      targetId: cur && cur.targetNode === target ? cur.targetId : newTodoId(),
      base: item,
      copy: 'none',
      copyUpdatedMs: null,
      stage: 'idle',
      outcome: null,
      copyOutcome: null,
    }
    useMoves.setState((s) => ({ moves: { ...s.moves, [key]: entry } }))
  }
  return drive(key)
}

/** 接着做这一项上一次没做完的移动（没有就什么都不做）。 */
export function resumeMove(node: string, id: string): Promise<MoveResult> | null {
  const key = moveKey(node, id)
  const e = useMoves.getState().moves[key]
  if (!e) return null
  return inflight.get(key) ?? moveTodo(e.base, e.targetNode)
}

/** 不再接着做（「两份都留着」）：忘掉这次移动；在跑的不能放弃。目标上已经有的那一份不动。 */
export function abandonMove(node: string, id: string): void {
  const key = moveKey(node, id)
  if (inflight.has(key)) return
  drop(key)
}

function drive(key: string): Promise<MoveResult> {
  const p = (async () => {
    let r: MoveResult
    try {
      r = await step(key)
    } catch (err) {
      r = unexpected(key, err)
    }
    try {
      finish(key, r)
    } catch (err) {
      // 报告本身出了问题：结果照样返回，不让对话框卡在「进行中」
      console.warn('move: reporting the result failed', err)
    }
    return r
  })()
  inflight.set(key, p)
  void p.finally(() => inflight.delete(key))
  return p
}

/** 没料到的错误：按它出在哪一步算——请求已经发出的步骤按「可能已经生效」，不报「什么都没变」。 */
function unexpected(key: string, err: unknown): MoveResult {
  console.warn('move: unexpected error', err)
  const e = useMoves.getState().moves[key]
  if (!e) return { ok: false, copy: 'none', stage: 'unexpected' }
  if (e.stage === 'adding' && e.copy === 'none') patch(key, { copy: 'maybe' })
  if (e.stage === 'restoring') {
    patch(key, { copyUpdatedMs: null })
    return fail(key, { stage: 'state', maybe: true })
  }
  if (e.stage === 'deleting') return fail(key, { stage: 'link', at: 'delete' })
  if (e.stage === 'adding') return fail(key, { stage: 'link', at: 'add' })
  return fail(key, { stage: 'unexpected' })
}

async function step(key: string): Promise<MoveResult> {
  patch(key, { copyOutcome: null })
  const { base, targetNode, targetId, sourceNode, sourceId } = get(key)

  // 1. 目标上建一份（新 id）。动手前先向原节点读这一项：它必须在、而且和用户确认时一样。`maybe` 的重发同一条，幂等
  if (get(key).copy === 'none' || get(key).copy === 'maybe') {
    const g = gate(key)
    if (g) return fail(key, g)
    patch(key, { stage: 'checking' })
    const live = await readOn(sourceNode, sourceId)
    if (live === 'unreadable') return fail(key, { stage: 'unreadable', side: 'source' })
    if (live === 'gone') return fail(key, { stage: 'changed', gone: true })
    if (!sameAsBase(live, base)) return fail(key, { stage: 'changed', gone: false })
    const g2 = gate(key)
    if (g2) return fail(key, g2)
    patch(key, { stage: 'adding' })
    try {
      const added = await rpc().request<Todo>(targetNode, 'todo.add', { id: targetId, title: base.title, body: base.body, ...(base.due_ms != null ? { due_ms: base.due_ms } : {}) })
      patch(key, { copy: base.state === 'open' ? 'full' : 'open', copyUpdatedMs: added.updated_ms })
      await applyConfirmedTodo(targetNode, added)
    } catch (err) {
      if (isLink(err)) {
        patch(key, { copy: 'maybe', copyUpdatedMs: null })
        return fail(key, { stage: 'link', at: 'add' })
      }
      if (!(err instanceof RpcCallError)) throw err
      return fail(key, { stage: 'rejected', code: err.code ?? 'error', ...(err.field ? { field: err.field } : {}), ...(err.reason ? { reason: err.reason } : {}) })
    }
  }

  // 2. 已关闭 / 处理中的项：状态在目标上补一次（`todo.add` 只会建出「打开」的）。补不上就停在这里：原节点的还在，目标上多了一份打开的
  if (get(key).copy === 'open') {
    const g = gate(key)
    if (g) return fail(key, g)
    patch(key, { stage: 'restoring' })
    try {
      const updated = await rpc().request<Todo>(targetNode, 'todo.update', { id: targetId, state: base.state })
      patch(key, { copy: 'full', copyUpdatedMs: updated.updated_ms })
      await applyConfirmedTodo(targetNode, updated)
    } catch (err) {
      if (!(err instanceof RpcCallError)) throw err
      // 请求断了：状态可能已经补上了，那一份现在的 `updated_ms` 不知道
      if (isLink(err)) patch(key, { copyUpdatedMs: null })
      return fail(key, { stage: 'state', maybe: isLink(err) })
    }
  }

  // 3. 删原节点之前：目标上那一份确实在（向目标节点读，不看缓存；确认不了就不删原节点：宁可两份，不能一份都没有）
  const g = gate(key)
  if (g) return fail(key, g)
  patch(key, { stage: 'checking' })
  const copy = await readOn(targetNode, targetId)
  if (copy === 'unreadable') return fail(key, { stage: 'unreadable', side: 'target' })
  if (copy === 'gone') {
    patch(key, { copy: 'gone' })
    return fail(key, { stage: 'copyGone' })
  }
  // 删之前再查一次（读目标花了一个来回，这期间可能有了没发出去的改动、节点掉线）
  const g3 = gate(key)
  if (g3) return fail(key, g3)
  patch(key, { stage: 'deleting' })
  let sourceAlreadyGone = false
  try {
    await rpc().request(sourceNode, 'todo.delete', { id: sourceId, if_updated_ms: base.updated_ms })
  } catch (err) {
    if (!(err instanceof RpcCallError)) throw err // 没料到的错误：交给外层按「可能已经生效」处理
    if (isRpc(err, 'not_found')) {
      // 原节点那一项已经不在了（别处删了）：目标上有了，原节点没有，结果是想要的，但不是一次干净的移动，要说出来
      sourceAlreadyGone = true
    } else if (isChanged(err)) {
      return fail(key, { stage: 'changed', gone: false })
    } else if (isLink(err)) {
      return fail(key, { stage: 'link', at: 'delete' })
    } else {
      return fail(key, { stage: 'stuck' })
    }
  }
  await applyConfirmedDelete(sourceNode, sourceId)
  useDrafts.getState().clear(draftKey(sourceNode, sourceId))
  return { ok: true, newId: targetId, sourceAlreadyGone }
}

/**
 * 删掉目标上的那一份（失败的移动之后「不要这份了」）。**只在原节点那一项还在时才删**：先向原节点读，不在或读不了就拒绝
 * （目标上这一份可能是仅存的一份）；再带 `if_updated_ms`（那一份最后一次由我们的请求得到的值）删，它在这期间被改过就拒绝。
 * 结果存在记录的 `copyOutcome` 里，不盖掉移动本身的结果。成功就忘掉这次移动。
 */
export async function removeCopy(node: string, id: string): Promise<CopyDeleteOutcome | { stage: 'done' } | null> {
  const key = moveKey(node, id)
  const e0 = useMoves.getState().moves[key]
  if (!e0 || isRunning(e0) || inflight.has(key)) return null
  if (!canDeleteCopy(e0)) return null
  const run = (async (): Promise<CopyDeleteOutcome | { stage: 'done' }> => {
    const refuse = (o: CopyDeleteOutcome): CopyDeleteOutcome => {
      patch(key, { stage: 'failed', copyOutcome: o })
      return o
    }
    try {
      const g = gate(key)
      if (g && g.stage === 'blocked') {
        // 动手前的条件和移动一样（草稿、待发改动、两个节点在线并已同步）；不满足就说为什么
        return refuse({ stage: 'blocked', block: g.block })
      }
      patch(key, { stage: 'checking', copyOutcome: null })
      const source = await readOn(e0.sourceNode, e0.sourceId)
      if (source === 'gone' || source === 'unreadable') return refuse({ stage: 'sourceGone' })
      patch(key, { stage: 'removingCopy' })
      try {
        await rpc().request(e0.targetNode, 'todo.delete', { id: e0.targetId, if_updated_ms: e0.copyUpdatedMs! })
      } catch (err) {
        if (isChanged(err)) return refuse({ stage: 'copyChanged' })
        if (!isRpc(err, 'not_found')) {
          if (isLink(err)) patch(key, { copy: 'maybe', copyUpdatedMs: null })
          return refuse({ stage: 'failed', maybe: isLink(err) })
        }
      }
      await applyConfirmedDelete(e0.targetNode, e0.targetId)
      drop(key)
      toast.success(i18n.t('spaces.move.copyDeleted', { node: nodeName(e0.targetNode) }))
      return { stage: 'done' }
    } catch (err) {
      console.warn('move: deleting the copy failed unexpectedly', err)
      patch(key, { copy: 'maybe', copyUpdatedMs: null })
      return refuse({ stage: 'failed', maybe: true })
    }
  })()
  const tracked = run.then(() => ({ ok: true, newId: e0.targetId, sourceAlreadyGone: false }) as MoveResult)
  inflight.set(key, tracked)
  void tracked.finally(() => inflight.delete(key))
  const r = await run
  if (r.stage !== 'done') reportCopy(get(key), r)
  return r
}

const nodeName = (id: string): string => cleanHintText(useNodes.getState().byId[id]?.name) || id.slice(0, 8)

/** 结果 → 一句话（对话框和提示共用）。 */
export function moveText(r: Exclude<MoveResult, { ok: true; sourceAlreadyGone: false }>, node: string, state?: TodoState): string {
  const t = i18n.t.bind(i18n)
  if (r.ok) return t('spaces.move.sourceGone', { node })
  const copied = r.copy !== 'none'
  switch (r.stage) {
    case 'blocked':
      return t(`spaces.move.blocked.${r.block === 'unsupported' ? 'offlineTarget' : r.block}`, { node })
    case 'changed':
      if (r.gone) return t(copied ? 'spaces.move.changedGoneCopy' : 'spaces.move.changedGone', { node })
      return t(copied ? 'spaces.move.changedCopy' : 'spaces.move.changed', { node })
    case 'unreadable':
      return t(r.side === 'source' ? (copied ? 'spaces.move.unreadableSourceCopy' : 'spaces.move.unreadableSource') : 'spaces.move.unreadableTarget', { node })
    case 'rejected':
      return t(r.copy === 'maybe' ? 'spaces.move.failedAddMaybe' : 'spaces.move.failedAdd', { node, reason: outboxErrorText(t as never, { code: r.code, ...(r.field ? { field: r.field } : {}), ...(r.reason ? { reason: r.reason } : {}) }) })
    case 'link':
      return t(r.at === 'add' ? 'spaces.move.failedLink' : 'spaces.move.deleteMaybe', { node })
    case 'state':
      return t(r.maybe ? 'spaces.move.stateMaybe' : 'spaces.move.stateFailed', { node, state: t(`todos.states.${state ?? 'done'}`) })
    case 'stuck':
      return t('spaces.move.stuck', { node })
    case 'copyGone':
      return t('spaces.move.copyGone', { node })
    case 'unexpected':
      return t(copied ? 'spaces.move.unexpectedCopy' : 'spaces.move.unexpected', { node })
  }
}

/** 「删掉目标上那一份」的结果 → 一句话。 */
export function copyText(o: CopyDeleteOutcome, node: string): string {
  const t = i18n.t.bind(i18n)
  switch (o.stage) {
    case 'blocked':
      return t(`spaces.move.blocked.${o.block === 'unsupported' ? 'offlineTarget' : o.block}`, { node })
    case 'sourceGone':
      return t('spaces.move.copySourceGone', { node })
    case 'copyChanged':
      return t('spaces.move.copyChanged', { node })
    case 'failed':
      return t(o.maybe ? 'spaces.move.copyDeleteMaybe' : 'spaces.move.copyDeleteFailed', { node })
  }
}

/** 现在是不是正停在这一项自己的页面上（详情路由 `/todos/:node/:todo`）：只有这时成功之后才把用户带回清单，别的页面不拉走。 */
function onItemRoute(node: string, id: string): boolean {
  const m = /^\/todos\/([^/]+)\/([^/]+)\/?$/.exec(location.pathname)
  if (!m) return false
  let nodeParam: string
  let todo: string
  try {
    nodeParam = decodeURIComponent(m[1]!)
    todo = decodeURIComponent(m[2]!)
  } catch {
    return false
  }
  // 和路由一样解析节点参数（完整 id 或唯一前缀）
  return todo === id && resolveNodeParam(nodeParam, useNodes.getState().order) === node
}

function finish(key: string, r: MoveResult): void {
  const e = useMoves.getState().moves[key]
  if (!e) return
  if (r.ok) {
    drop(key)
    if (r.sourceAlreadyGone) reportMove(e, r)
    else toast.success(i18n.t('spaces.move.moved', { node: nodeName(e.targetNode) }))
    // 用户这期间去了别处就不拉回来：只有还停在这一项的详情页上（它已经不在了）才回到清单
    if (onItemRoute(e.sourceNode, e.sourceId)) navigateTo('/todos', { replace: true })
    return
  }
  reportMove(e, r)
}

/** 不是干净成功的结果（包括没动手的 `blocked`）弹一条提示（`role="alert"`），带「打开」去那一项：对话框不管还在不在，用户都知道发生了什么。 */
function reportMove(e: MoveEntry, r: MoveResult): void {
  if (r.ok && !r.sourceAlreadyGone) return
  const order = useNodes.getState().order
  // 去哪里看：原节点的那一项还在就去它；已经没了（删了）就去目标上的那份
  const openTarget = r.ok || (r.stage === 'changed' && r.gone && r.copy !== 'none')
  const url = openTarget ? `/todos/${shortNodeId(e.targetNode, order)}/${e.targetId}` : `/todos/${shortNodeId(e.sourceNode, order)}/${e.sourceId}`
  useUi.getState().push('error', moveText(r as never, nodeName(e.targetNode), e.base.state), { label: i18n.t('todos.review'), run: () => navigateTo(url) }, 15_000)
}

/** 删掉目标上那一份没成：同样弹提示（`blocked` 也弹），带「打开」去原节点那一项。 */
function reportCopy(e: MoveEntry | undefined, o: CopyDeleteOutcome): void {
  if (!e) return
  const order = useNodes.getState().order
  const url = `/todos/${shortNodeId(e.sourceNode, order)}/${e.sourceId}`
  useUi.getState().push('error', copyText(o, nodeName(e.targetNode)), { label: i18n.t('todos.review'), run: () => navigateTo(url) }, 15_000)
}

/** 移过去的这一份是用户自己的：原来是 agent 加的（收下之后）的项要在确认里说一声。 */
export const wasAgentAdded = (item: ViewTodo): boolean => !isHumanSource(item.source) && !item.unreviewed
