// `error` 事件的分级与说明。各 agent 的 error code 不同（Claude Code：`agent_exited`、`auth_failed`…；Codex 另有
// `agent_reconnecting`、`agent_context_reset`、`unsupported_request`、`context_window_exceeded`、`sandbox_error`、`policy_violation`），
// 渲染只认 code 的分级，不认 agent。

/** notice：淡色一行，不是错误（重连中）；warning：黄色，需要知道但不用处理；error：红色错误块。 */
export type ErrorSeverity = 'notice' | 'warning' | 'error'

// 用 Map：code 来自 adapter，`constructor` / `__proto__` 这样的值不能从原型上查出东西
const SEVERITY: ReadonlyMap<string, ErrorSeverity> = new Map<string, ErrorSeverity>([
  ['agent_reconnecting', 'notice'],
  // 续接时原来的会话线程已经不在，adapter 开了新的：提示，不是错误
  ['agent_context_reset', 'notice'],
  ['unsupported_request', 'warning'],
])

export function errorSeverity(code: string): ErrorSeverity {
  return SEVERITY.get(code) ?? 'error'
}

/** 有本地化说明（`agentError.<code>`）的 code；其它 code 只显示 daemon 给的 message。 */
export const EXPLAINED_ERROR_CODES = ['agent_reconnecting', 'agent_context_reset', 'unsupported_request', 'context_window_exceeded', 'sandbox_error', 'policy_violation', 'rate_limited', 'auth_failed'] as const

const EXPLAINED: ReadonlySet<string> = new Set(EXPLAINED_ERROR_CODES)

export function isExplainedError(code: string): boolean {
  return EXPLAINED.has(code)
}
