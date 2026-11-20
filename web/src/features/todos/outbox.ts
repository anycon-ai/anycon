// 离线期间的改动怎么入队、怎么叠到界面上（docs/spec/daemon.md § 21.6、spec/web.md § 4.12）。全是纯函数，重放和 IndexedDB 在 `service.ts`。
//
// 入队时合并（同一项的改动，只合并还没被某个标签页「认领」去发的）：
//   - 连续的 `update` 合并成一条（字段叠加，后改的覆盖先改的；`clear` 和设值互相抵消）：重放的是「用户改了的那几个字段」，
//     所以合并之后和一条一条重放的结果相同，只是少发几个请求；
//   - `add` 还没发（或发了被拒、还在界面上等用户处理）时，标题 / 正文 / 项目 / 期望时间的修改直接并进 `add` 的参数
//     （`add` 之后再 `update` 一个还不存在的项没有意义，也避免被拒的 `add` 后面跟着一串必然 `not_found` 的 `update`）；
//     状态和「收下」不能放进 `add`，仍作为 `add` 后面的 `update`；
//   - `delete`：这一项的 `add` 还没被认领（daemon 肯定没见过它）就把这一项的所有待发改动整个取消，什么都不发；
//     否则丢掉没认领的 `update`，追加 `delete`（`todo.delete` 得到 `not_found` 视为已完成，所以 `add` 在飞时也安全）。
import type { Todo, TodoState } from '@/protocol'
import type { OutboxOp, OutboxRecord, UpdateFields, ViewTodo } from './types'
import { isClosed } from './types'

type AddParams = Extract<OutboxOp, { kind: 'add' }>['params']

/** 把 `b`（后改的）叠到 `a` 上：字段覆盖；`clear` 和设值互相抵消。 */
export function mergeUpdate(a: UpdateFields, b: UpdateFields): UpdateFields {
  const out: UpdateFields = { ...a }
  const clear = new Set(a.clear ?? [])
  for (const k of ['title', 'body', 'state', 'reviewed'] as const) {
    if (b[k] !== undefined) (out as Record<string, unknown>)[k] = b[k]
  }
  for (const k of ['project', 'due_ms'] as const) {
    if (b[k] !== undefined && b[k] !== null) {
      ;(out as Record<string, unknown>)[k] = b[k]
      clear.delete(k)
    }
  }
  for (const k of b.clear ?? []) {
    clear.add(k)
    delete (out as Record<string, unknown>)[k]
  }
  if (clear.size > 0) out.clear = [...clear]
  else delete out.clear
  return out
}

const ADDABLE = ['title', 'body', 'project', 'due_ms', 'clear'] as const

function splitFields(f: UpdateFields): { addable: UpdateFields; rest: UpdateFields } {
  const addable: UpdateFields = {}
  const rest: UpdateFields = {}
  for (const [k, v] of Object.entries(f)) {
    if ((ADDABLE as readonly string[]).includes(k)) (addable as Record<string, unknown>)[k] = v
    else (rest as Record<string, unknown>)[k] = v
  }
  return { addable, rest }
}

function applyToAdd(p: AddParams, f: UpdateFields): AddParams {
  const out: AddParams = { ...p }
  if (f.title !== undefined) out.title = f.title
  if (f.body !== undefined) out.body = f.body
  if (f.project !== undefined && f.project !== null) out.project = f.project
  if (f.due_ms !== undefined && f.due_ms !== null) out.due_ms = f.due_ms
  for (const k of f.clear ?? []) delete out[k]
  return out
}

export interface EnqueuePlan {
  remove: number[]
  put: OutboxRecord[]
}

const unclaimed = (r: OutboxRecord): boolean => r.claimed_at === undefined

/**
 * `existing`：这个节点上这一项现有的改动（按入队顺序）。返回要删的 `seq` 和要写的记录（没有 `seq` 的是新增，数据库给它排在最后）。
 */
export function planEnqueue(nodeId: string, id: string, existing: readonly OutboxRecord[], op: OutboxOp, now: number): EnqueuePlan {
  const fresh = (o: OutboxOp): OutboxRecord => ({ node_id: nodeId, id, op: o, created_at: now, state: 'pending' })

  if (op.kind === 'delete') {
    const add = existing.find((r) => r.op.kind === 'add')
    if (add && unclaimed(add)) return { remove: existing.map((r) => r.seq!).filter((q) => q !== undefined), put: [] }
    return { remove: existing.filter((r) => r.op.kind === 'update' && unclaimed(r)).map((r) => r.seq!), put: [fresh(op)] }
  }

  if (op.kind === 'add') return { remove: [], put: [fresh(op)] }

  // update
  const add = existing.find((r) => r.op.kind === 'add' && unclaimed(r))
  let fields = op.fields
  const put: OutboxRecord[] = []
  if (add && add.op.kind === 'add') {
    const { addable, rest } = splitFields(fields)
    // 并进 add：被拒的 add 改过之后重新排队（用户改它就是想让它再试一次）
    const { error: _e, ...base } = add
    put.push({ ...base, state: 'pending', op: { kind: 'add', params: applyToAdd(add.op.params, addable) } })
    if (Object.keys(rest).length === 0) return { remove: [], put }
    fields = rest
  }
  const last = existing.filter((r) => r.op.kind !== 'add' || !unclaimed(r)).at(-1)
  if (last && last.op.kind === 'update' && unclaimed(last) && last.state === 'pending') {
    put.push({ ...last, op: { kind: 'update', fields: mergeUpdate(last.op.fields, fields) } })
  } else {
    put.push(fresh({ kind: 'update', fields }))
  }
  return { remove: [], put }
}

const NEW_TODO = (nodeId: string, id: string, p: AddParams, at: number): ViewTodo => ({
  id,
  node_id: nodeId,
  title: p.title,
  body: p.body,
  ...(p.project ? { project: p.project } : {}),
  ...(p.due_ms != null ? { due_ms: p.due_ms } : {}),
  state: 'open',
  source: { kind: 'device' },
  unreviewed: false,
  created_ms: at,
  updated_ms: at,
  unsynced: true,
})

function applyUpdate(t: ViewTodo, f: UpdateFields, at: number): ViewTodo {
  const out: ViewTodo = { ...t }
  if (f.title !== undefined) out.title = f.title
  if (f.body !== undefined) out.body = f.body
  if (f.project !== undefined && f.project !== null) out.project = f.project
  if (f.due_ms !== undefined && f.due_ms !== null) out.due_ms = f.due_ms
  for (const k of f.clear ?? []) delete out[k]
  if (f.state !== undefined) {
    out.state = f.state
    if (isClosed(f.state)) {
      if (!isClosed(t.state)) out.closed_ms = at
    } else delete out.closed_ms
  }
  // 人改了它（任何字段）就不再是「待收下」（和 daemon 的规则一样）
  if (f.reviewed || f.title !== undefined || f.body !== undefined || f.project !== undefined || f.due_ms !== undefined || f.state !== undefined || (f.clear?.length ?? 0) > 0) out.unreviewed = false
  out.updated_ms = Math.max(t.updated_ms, at)
  return out
}

/**
 * 缓存里的项（daemon 最近一次给的）叠上还没同步的改动，得到界面上显示的样子。按入队顺序逐条叠。
 * 返回的 `Map` 里：被本地删掉的项不在；本地新建的项在；有改动的项 `unsynced`，有被拒的改动的项带 `failed`。
 */
export function overlayOutbox(nodeId: string, base: ReadonlyMap<string, Todo>, ops: readonly OutboxRecord[]): Map<string, ViewTodo> {
  const out = new Map<string, ViewTodo>()
  for (const [id, t] of base) out.set(id, { ...t, node_id: nodeId, unsynced: false })
  for (const r of [...ops].sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))) {
    if (r.node_id !== nodeId) continue
    const cur = out.get(r.id)
    if (r.op.kind === 'add') {
      // add 幂等：daemon 那边已经有这一项（重放中途断了）就保留它的样子，只标未同步
      out.set(r.id, cur ? { ...cur, unsynced: true } : NEW_TODO(nodeId, r.id, r.op.params, r.created_at))
    } else if (r.op.kind === 'delete') {
      out.delete(r.id)
      continue
    } else if (cur) {
      out.set(r.id, applyUpdate(cur, r.op.fields, r.created_at))
    } else continue
    const v = out.get(r.id)!
    v.unsynced = true
    if (r.state === 'failed' && r.error) v.failed = r.error
  }
  return out
}

/** 这个状态应该显示在哪一组（`waiting` / `review` 这一步没有自己的组，归在「处理中」，spec/web.md § 4.12）。 */
export type TodoGroup = 'working' | 'open' | 'closed'
export function groupOf(state: TodoState): TodoGroup {
  if (state === 'open') return 'open'
  if (isClosed(state)) return 'closed'
  return 'working'
}
