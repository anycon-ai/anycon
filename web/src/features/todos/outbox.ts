// 待发队列怎么叠到界面上（docs/spec/daemon.md § 21.6、spec/web.md § 4.12）。全是纯函数，重放和 IndexedDB 在 `service.ts`。
//
// 队列是只追加的有序日志：用户的每个动作追加一条（`add` / `update` / `delete`），按顺序重放，**不合并、不取消**。
// 离线期间新增 + 修改 + 删除就是三个请求。合并和取消看起来省请求，但「回执丢了」时它们会把没发完的状态改写掉
// （合并进已经被 daemon 收下的 `add`、取消一个其实已经到了的 `add`），所以不做。
import type { Todo, TodoState } from '@/protocol'
import type { OutboxOp, OutboxRecord, UpdateFields, ViewTodo } from './types'
import { isClosed } from './types'

type AddParams = Extract<OutboxOp, { kind: 'add' }>['params']

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
      // 已关闭的项没有 `activity`（daemon 也不给）；打开的项的 `activity` 是 daemon 算的，这里不编
      delete out.activity
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
  const byId = new Map<string, OutboxRecord[]>()
  for (const r of ops) {
    if (r.node_id !== nodeId) continue
    const list = byId.get(r.id)
    if (list) list.push(r)
    else byId.set(r.id, [r])
  }
  for (const [id, list] of byId) {
    const v = overlayItem(nodeId, id, base.get(id), list)
    if (v) out.set(id, v)
    else out.delete(id)
  }
  return out
}

/** 一项的待发改动按顺序叠到它的缓存上。返回 `null`：这一项在界面上不存在（本地删了）。 */
export function overlayItem(nodeId: string, id: string, base: Todo | undefined, ops: readonly OutboxRecord[]): ViewTodo | null {
  let cur: ViewTodo | undefined = base ? { ...base, node_id: nodeId, unsynced: false } : undefined
  let failed: ViewTodo['failed']
  // 这一项前面已经有发不出去的（失败的）记录：排在它后面的删除也发不出去，不能把项藏起来
  let blocked = false
  for (const r of [...ops].sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))) {
    if (r.op.kind === 'add') {
      // add 幂等：daemon 那边已经有这一项（重放中途断了）就保留它的样子，只标未同步
      cur = cur ? { ...cur, unsynced: true } : NEW_TODO(nodeId, id, r.op.params, r.created_at)
    } else if (r.op.kind === 'delete') {
      // 被拒的删除、或者前面有失败记录挡着的删除：项还在，标「未保存」
      if (r.state === 'failed' || blocked) {
        if (cur) cur = { ...cur, unsynced: true }
      } else cur = undefined
    } else if (cur) {
      cur = { ...applyUpdate(cur, r.op.fields, r.created_at), unsynced: true }
    }
    if (r.state === 'failed') blocked = true
    if (cur && r.state === 'failed' && r.error) failed = r.error
  }
  if (!cur) return null
  return failed ? { ...cur, failed } : cur
}

/** 这个状态应该显示在哪一组（`waiting` / `review` 这一步没有自己的组，归在「处理中」，spec/web.md § 4.12）。 */
export type TodoGroup = 'working' | 'open' | 'closed'
export function groupOf(state: TodoState): TodoGroup {
  if (state === 'open') return 'open'
  if (isClosed(state)) return 'closed'
  return 'working'
}
