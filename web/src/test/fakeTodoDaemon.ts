// 清单的假 daemon：把 `todo.*` 的语义（docs/spec/daemon.md § 21.3）装进 `FakeRpc` 的 handlers，业务层测试用。
// 记下收到的请求顺序（`log`），可以让某个方法按需失败（`failNext`）。
import type { Todo, TodoListParams, TodoUpdateParams, TodoAddParams } from '@/protocol'
import type { FakeRpc } from '@/test/fakeRpc'
import { rpcError } from '@/test/fakeRpc'
import { RpcCallError, type RpcCloseReason } from '@/transport/rpcClient'

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

  /** 只取某一个方法的处理函数（多个假 daemon 按节点路由时用）。 */
  routeFor(method: string): (p: unknown, nodeId: string) => unknown {
    const bag = { handlers: {} as Record<string, (p: unknown, n: string) => unknown> }
    this.install(bag as unknown as FakeRpc)
    return bag.handlers[method]!
  }

  install(rpc: FakeRpc): void {
    rpc.handlers['todo.list'] = (p) => this.list(p as TodoListParams)
    rpc.handlers['todo.get'] = (p) => this.handle('todo.get', p, () => this.get((p as { id: string }).id))
    rpc.handlers['todo.add'] = (p) => this.handle('todo.add', p, () => this.add(p as TodoAddParams))
    rpc.handlers['todo.update'] = (p) => this.handle('todo.update', p, () => this.update(p as TodoUpdateParams))
    rpc.handlers['todo.delete'] = (p) => this.handle('todo.delete', p, () => this.delete(p as { id: string; if_updated_ms?: number }))
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
    const t: Todo = { ...(p.handler ? { handler: p.handler } : {}), id: p.id, title: p.title, body: p.body ?? '', ...(p.project ? { project: p.project } : {}), ...(p.due_ms != null ? { due_ms: p.due_ms } : {}), ...(p.repeat ? { repeat: p.repeat } : {}), state: 'open', source: { kind: p.agent ? 'agent' : 'device', device_id: 'dev' }, unreviewed: !!p.agent, created_ms: ++this.now, updated_ms: this.now }
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
    if (p.handler === 'twin' && cur.unreviewed && !p.reviewed) throw rpcError('invalid', 'handler', 'unreviewed')
    if (p.handler) next.handler = p.handler
    if (p.title != null) next.title = p.title
    if (p.body != null) next.body = p.body
    if (p.project != null) next.project = p.project
    if (p.due_ms != null) next.due_ms = p.due_ms
    if (p.repeat) {
      if (next.handler === 'twin') throw rpcError('invalid', 'handler', 'repeat_template')
      next.repeat = p.repeat
    }
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

  private get(id: string): Todo {
    const t = this.todos.get(id)
    if (!t) throw rpcError('not_found')
    return t
  }

  private delete(p: { id: string; if_updated_ms?: number }): Record<string, never> {
    const cur = this.todos.get(p.id)
    if (!cur) throw rpcError('not_found')
    // 带条件的删除（docs/spec/daemon.md § 21.3）：现在的 updated_ms 对不上就拒绝，什么都不删
    if (p.if_updated_ms !== undefined && cur.updated_ms !== p.if_updated_ms) {
      throw new RpcCallError('rpc', 'changed', { code: -32602, message: 'changed', data: { code: 'invalid', field: 'if_updated_ms', reason: 'changed', updated_ms: cur.updated_ms } })
    }
    this.todos.delete(p.id)
    this.onChange?.(p.id, null)
    return {}
  }

  /** 直接改节点上的一项（别的设备 / 别的标签页改的）：`updated_ms` 照规则加一，**不**通知这个浏览器标签页，缓存因此会是旧的。 */
  editBehindTheBrowser(id: string, patch: Partial<Todo>): Todo {
    const cur = this.todos.get(id)
    if (!cur) throw new Error(`no such todo ${id}`)
    const next = { ...cur, ...patch, updated_ms: ++this.now }
    this.todos.set(id, next)
    return next
  }

  /** 直接从节点上删掉一项，不通知这个浏览器标签页。 */
  deleteBehindTheBrowser(id: string): void {
    this.todos.delete(id)
  }
}

export const linkError = (kind: 'offline' | 'timeout' | 'closed' | 'undeliverable' = 'timeout', closeReason?: RpcCloseReason): RpcCallError => new RpcCallError(kind, `link: ${kind}`, undefined, closeReason)
