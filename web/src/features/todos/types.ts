import type { Todo, TodoState } from '@/protocol'

/** 这一步界面上有的四个状态（`waiting` / `review` 由别的客户端写，归在「处理中」里显示，spec/web.md § 4.12）。 */
export const UI_STATES = ['open', 'working', 'done', 'dropped'] as const satisfies readonly TodoState[]
export const isClosed = (s: TodoState): boolean => s === 'done' || s === 'dropped'

/** `todo.update` 里用户改了的字段（只带改了的）。 */
export interface UpdateFields {
  title?: string
  body?: string
  project?: string | null
  due_ms?: number | null
  state?: TodoState
  reviewed?: boolean
  clear?: Array<'project' | 'due_ms'>
}

/** 离线期间留在本地的一条改动（spec/web.md § 4.12、daemon.md § 21.6）。 */
export type OutboxOp =
  | { kind: 'add'; params: { title: string; body: string; project?: string | null; due_ms?: number | null } }
  | { kind: 'update'; fields: UpdateFields }
  | { kind: 'delete' }

export interface OutboxError {
  code: string
  field?: string
  reason?: string
}

export interface OutboxRecord {
  /** 入队顺序（IndexedDB 自增）：重放按它的顺序。 */
  seq?: number
  node_id: string
  /** 项的 id（客户端生成）。 */
  id: string
  op: OutboxOp
  created_at: number
  /** `pending` 等着重放；`failed` daemon 拒绝了（带 `error`），项留在界面上标成失败，用户选重试或丢弃。 */
  state: 'pending' | 'failed'
  error?: OutboxError
  /** 正在被某个标签页重放（不再合并新的改动进来）。 */
  claimed_at?: number
}

/** 界面上用的一项：daemon 的 `Todo` 叠上本地还没同步的改动。 */
export interface ViewTodo extends Todo {
  node_id: string
  /** 有没同步的改动（离线期间做的，或正在发）。 */
  unsynced: boolean
  /** 有被 daemon 拒绝的改动，等用户处理。 */
  failed?: OutboxError
}
