// `error` 事件的分级与说明。各 agent 的 error code 不同（Claude Code：`agent_exited`、`auth_failed`…；Codex 另有
// `agent_reconnecting`、`agent_context_reset`、`unsupported_request`、`context_window_exceeded`、`sandbox_error`、`policy_violation`、
// `model_not_found`；daemon 另有 `agent_config`；限额类 `usage_limit`、`rate_limited`、`overloaded`、`budget_exceeded` 两个 agent 都有），
// 渲染只认 code 的分级，不认 agent。
import type { TFunction } from 'i18next'
import type { ErrorBlock } from './reducer'
import { formatResetTime } from './notice'

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
export const EXPLAINED_ERROR_CODES = ['agent_reconnecting', 'agent_context_reset', 'unsupported_request', 'context_window_exceeded', 'sandbox_error', 'policy_violation', 'usage_limit', 'rate_limited', 'overloaded', 'budget_exceeded', 'auth_failed', 'attachment_failed', 'agent_config', 'model_not_found'] as const

const EXPLAINED: ReadonlySet<string> = new Set(EXPLAINED_ERROR_CODES)

export function isExplainedError(code: string): boolean {
  return EXPLAINED.has(code)
}

/**
 * 错误卡片里除 `message` 和 code 说明之外的补充行：
 * - 有恢复时间（`resets_at_ms`）就写本地化的恢复时间——任何 code 都写，不认识的新 code 带了也有用；
 * - `usage_limit` 且范围是某个模型（`limit_scope = model`）时提示换模型（输入区已有模型选择）。
 * 返回的 key 供测试和卡片用。
 */
export interface ErrorExtras {
  resets?: string
  modelHint?: string
}

export function errorExtras(t: TFunction, lang: string, b: ErrorBlock, now: number = Date.now()): ErrorExtras {
  const extras: ErrorExtras = {}
  if (b.resetsAtMs != null) extras.resets = t('agentError.resetsAt', { time: formatResetTime(b.resetsAtMs, lang, now) })
  if (b.code === 'usage_limit' && b.limitScope === 'model') extras.modelHint = t('agentError.limitScopeModel')
  return extras
}
