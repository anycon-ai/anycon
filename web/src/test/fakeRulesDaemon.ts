// 规则的假 daemon：把 `rule.*` 的语义（docs/spec/daemon.md § 23.3）装进 `FakeRpc` 的 handlers，业务层和界面测试用。
// 记下收到的请求（`log`）；`if_updated_ms` 对不上回 `invalid` / `changed`；可以让某个方法按需失败（`failNext`）。
import type { ErrorCode, Rule, RuleBudget, RuleUpdateParams } from '@/protocol'
import type { FakeRpc } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'

export function ruleItem(over: Partial<Rule> & { id: string }): Rule {
  return { text: `rule ${over.id}`, status: 'active', sync: 'node', source: { kind: 'manual' }, created_ms: 1_000, updated_ms: 1_000, ...over }
}

export function ruleError(code: ErrorCode, extra: { field?: string; reason?: string; updated_ms?: number; bytes?: number; cap?: number } = {}): RpcCallError {
  return new RpcCallError('rpc', code, { code: -32000, message: code, data: { code, ...extra } })
}

export class FakeRulesDaemon {
  rules = new Map<string, Rule>()
  budget: RuleBudget = { cap: 4096, node_bytes: 0, projects: {} }
  log: Array<{ method: string; params: unknown }> = []
  failures: Array<{ method: string; error: unknown }> = []
  clock = 10_000
  /** `rule.changed` 的出口：每次真正的改动都调（测试里接到 `handleRuleChanged`）。 */
  onChange: ((id: string, rule: Rule | null) => void) | null = null

  install(rpc: FakeRpc): void {
    rpc.handlers['rule.list'] = (p) => this.handle('rule.list', p, () => ({ rules: [...this.rules.values()], budget: this.budget }))
    rpc.handlers['rule.add'] = (p) => this.handle('rule.add', p, () => this.add(p as { id: string; text: string; project?: string; status?: string; sync?: Rule['sync']; source?: Rule['source'] }))
    rpc.handlers['rule.update'] = (p) => this.handle('rule.update', p, () => this.update(p as RuleUpdateParams))
    rpc.handlers['rule.delete'] = (p) => this.handle('rule.delete', p, () => this.remove(p as { id: string; if_updated_ms?: number }))
  }

  seed(...rules: Rule[]): void {
    for (const r of rules) this.rules.set(r.id, r)
  }

  failNext(method: string, error: unknown): void {
    this.failures.push({ method, error })
  }

  calls(method: string): Array<Record<string, unknown>> {
    return this.log.filter((l) => l.method === method).map((l) => l.params as Record<string, unknown>)
  }

  private handle(method: string, params: unknown, run: () => unknown): unknown {
    this.log.push({ method, params })
    const i = this.failures.findIndex((f) => f.method === method)
    if (i >= 0) throw this.failures.splice(i, 1)[0]!.error
    return run()
  }

  private add(p: { id: string; text: string; project?: string; status?: string; sync?: Rule['sync']; source?: Rule['source'] }): Rule {
    const existing = this.rules.get(p.id)
    if (existing) return existing
    const rule = ruleItem({ id: p.id, text: p.text, ...(p.project ? { project: p.project } : {}), status: (p.status as Rule['status']) ?? 'active', sync: p.sync ?? 'node', source: p.source ?? { kind: 'manual' }, created_ms: ++this.clock, updated_ms: this.clock })
    this.rules.set(rule.id, rule)
    this.onChange?.(rule.id, rule)
    return rule
  }

  private update(p: RuleUpdateParams): Rule {
    const cur = this.rules.get(p.id)
    if (!cur) throw ruleError('not_found')
    if (p.if_updated_ms !== undefined && p.if_updated_ms !== cur.updated_ms) throw ruleError('invalid', { field: 'if_updated_ms', reason: 'changed', updated_ms: cur.updated_ms })
    const next: Rule = { ...cur, ...(typeof p.text === 'string' ? { text: p.text } : {}), ...(p.project !== undefined ? { project: p.project } : {}), ...(p.status ? { status: p.status } : {}), ...(p.sync ? { sync: p.sync } : {}) }
    if (p.clear?.includes('project')) delete next.project
    if (JSON.stringify(next) === JSON.stringify(cur)) return cur
    next.updated_ms = ++this.clock
    this.rules.set(next.id, next)
    this.onChange?.(next.id, next)
    return next
  }

  private remove(p: { id: string; if_updated_ms?: number }): Record<string, never> {
    const cur = this.rules.get(p.id)
    if (!cur) throw ruleError('not_found')
    if (p.if_updated_ms !== undefined && p.if_updated_ms !== cur.updated_ms) throw ruleError('invalid', { field: 'if_updated_ms', reason: 'changed', updated_ms: cur.updated_ms })
    this.rules.delete(p.id)
    this.onChange?.(p.id, null)
    return {}
  }

  /** 别的设备改了一条：不经过 `rpc`，直接改库并通知。 */
  changeElsewhere(id: string, patch: Partial<Rule>): Rule {
    const cur = this.rules.get(id)!
    const next = { ...cur, ...patch, updated_ms: ++this.clock }
    this.rules.set(id, next)
    this.onChange?.(id, next)
    return next
  }
}
