// 清单的假 daemon：把 `todo.*` 的语义（docs/spec/daemon.md § 21.3）装进 `FakeRpc` 的 handlers，业务层测试用。
// 记下收到的请求顺序（`log`），可以让某个方法按需失败（`failNext`）。
import type { Todo, TodoListParams, TodoUpdateParams, TodoAddParams } from '@/protocol'
import type { FakeRpc } from '@/test/fakeRpc'
import { rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'

export class FakeTodoDaemon {
  todos = new Map<string, Todo>()
  /** 收到的每个请求（方法 + 参数），按到达顺序。 */
  log: Array<{ method: string; params: unknown }> = []
  /** 下一次这个方法抛这个错（一次性）。 */
  failures: Array<{ method: string; error: unknown; after?: boolean }> = []
  now = 10_000
  openLimit = 1000
  pageSize = 200
  /** 给 `todo.changed` 用：每次改动都会调。 */
  onChange: ((id: string, todo: Todo | null) => void) | null = null

  install(rpc: FakeRpc): void {
    rpc.handlers['todo.list'] = (p) => this.list(p as TodoListParams)
    rpc.handlers['todo.add'] = (p) => this.handle('todo.add', p, () => this.add(p as TodoAddParams))
    rpc.handlers['todo.update'] = (p) => this.handle('todo.update', p, () => this.update(p as TodoUpdateParams))
    rpc.handlers['todo.delete'] = (p) => this.handle('todo.delete', p, () => this.delete((p as { id: string }).id))
  }

  seed(...todos: Todo[]): void {
    for (const t of todos) this.todos.set(t.id, t)
  }

  /** 下一次 `method` 失败。`after`：先执行再丢回执（回执丢了的情形）。 */
  failNext(method: string, error: unknown, opts: { after?: boolean } = {}): void {
    this.failures.push({ method, error, ...(opts.after ? { after: true } : {}) })
  }

  methods(): string[] {
    return this.log.map((l) => l.method)
  }

  private handle(method: string, params: unknown, run: () => unknown): unknown {
    this.log.push({ method, params })
    const i = this.failures.findIndex((f) => f.method === method)
    const f = i >= 0 ? this.failures.splice(i, 1)[0] : undefined
    if (f && !f.after) throw f.error
    const out = run()
    if (f?.after) throw f.error
    return out
  }

  private list(p: TodoListParams): { todos: Todo[]; has_more: boolean } {
    this.log.push({ method: 'todo.list', params: p })
    const all = [...this.todos.values()]
      .filter((t) => p.include_closed || (t.state !== 'done' && t.state !== 'dropped'))
      .sort((a, b) => b.updated_ms - a.updated_ms || (a.id < b.id ? 1 : -1))
    const after = p.before ? all.filter((t) => t.updated_ms < p.before!.updated_ms || (t.updated_ms === p.before!.updated_ms && t.id < p.before!.id)) : all
    const limit = Math.min(p.limit ?? this.pageSize, this.pageSize)
    return { todos: after.slice(0, limit), has_more: after.length > limit }
  }

  private add(p: TodoAddParams): Todo {
    const have = this.todos.get(p.id)
    if (have) return have
    if ([...this.todos.values()].filter((t) => t.state !== 'done' && t.state !== 'dropped').length >= this.openLimit) throw rpcError('invalid', 'todos', 'too_many')
    const t: Todo = { id: p.id, title: p.title, body: p.body ?? '', ...(p.project ? { project: p.project } : {}), ...(p.due_ms != null ? { due_ms: p.due_ms } : {}), state: 'open', source: { kind: p.agent ? 'agent' : 'device', device_id: 'dev' }, unreviewed: !!p.agent, created_ms: ++this.now, updated_ms: this.now }
    this.todos.set(t.id, t)
    this.onChange?.(t.id, t)
    return t
  }

  private update(p: TodoUpdateParams): Todo {
    const cur = this.todos.get(p.id)
    if (!cur) throw rpcError('not_found')
    const next: Todo = { ...cur }
    const reopening = (cur.state === 'done' || cur.state === 'dropped') && p.state && p.state !== 'done' && p.state !== 'dropped'
    if (reopening && [...this.todos.values()].filter((t) => t.state !== 'done' && t.state !== 'dropped').length >= this.openLimit) throw rpcError('invalid', 'todos', 'too_many')
    if (p.title != null) next.title = p.title
    if (p.body != null) next.body = p.body
    if (p.project != null) next.project = p.project
    if (p.due_ms != null) next.due_ms = p.due_ms
    for (const k of p.clear ?? []) delete next[k]
    if (p.state) {
      next.state = p.state
      if (p.state === 'done' || p.state === 'dropped') next.closed_ms ??= this.now
      else delete next.closed_ms
    }
    if (p.reviewed) next.unreviewed = false
    if (JSON.stringify(next) === JSON.stringify(cur)) return cur
    next.updated_ms = ++this.now
    this.todos.set(next.id, next)
    this.onChange?.(next.id, next)
    return next
  }

  private delete(id: string): Record<string, never> {
    if (!this.todos.delete(id)) throw rpcError('not_found')
    this.onChange?.(id, null)
    return {}
  }
}

export const linkError = (kind: 'offline' | 'timeout' | 'closed' | 'undeliverable' = 'timeout'): RpcCallError => new RpcCallError(kind, `link: ${kind}`)
