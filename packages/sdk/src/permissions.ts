/**
 * 权限处理。daemon 把 `permission.request` 发给所有在线客户端；SDK 对「自己正在用的会话」
 * （有进行中的 `send()` / `events()`）按 `onPermission` 策略代答，没有策略的请求交给调用方，
 * 超过 `permissionTimeoutMs`（默认 10 分钟）仍无人答复则由 SDK 代答 deny，避免 agent 永远挂着。
 *
 * 安全约定：
 * - **永远不会悄悄升级成 `allow_always`**：`'allow'` 只选 `allow_once`；请求里没有 `allow_once` 就当作策略无法执行
 *   （`Node` 发 `permission_error` 事件，请求照常走超时 deny），要「本会话都允许」必须显式给 `'allow_session'`。
 * - 现成的 `policies.allowAll` **不放行 `mode_change` 和 `plan`**（切换到危险模式 / 批准计划是提权，不是普通的
 *   工具调用），也不处理 `question`；需要时显式用 `policies.allowAllWith({ modeChange: true, plan: true })`。
 */
import type { PermissionOption, PermissionRequest, PermissionRespondParams } from '@anycon/protocol'

import type { Node } from './node.js'
import type { Session } from './session.js'

/** 策略的答复。返回 `undefined` 表示「我不管」，留给事件流里的调用方或超时。 */
export type PermissionDecision =
  | 'allow'
  | 'allow_session'
  | 'deny'
  /** 按 id 选 `options[]` 里的一项（ACP 等 adapter 的 option id 不固定）。 */
  | { option: string; message?: string }
  /** 拒绝并附一句话给 agent。 */
  | { deny: string }
  /** `kind = question` 的答复：`{ [question_id]: value }`。 */
  | { answers: Record<string, string | string[]> }
  | undefined
  | void

export interface PermissionContext {
  /** 请求所属的会话（SDK 持有句柄的才有）。 */
  session: Session | undefined
  node: Node
}

export type PermissionPolicy = (
  request: PermissionRequest,
  context: PermissionContext,
) => PermissionDecision | Promise<PermissionDecision>

type OptionPreference = readonly PermissionOption['kind'][]

// 只允许一次：不退回 allow_always（那是静默提权）。
const ALLOW_ONCE: OptionPreference = ['allow_once']
// 整个会话都允许：没有 allow_always 时退回 allow_once 只会收窄授权，不会放宽。
const ALLOW_SESSION: OptionPreference = ['allow_always', 'allow_once']
const DENY: OptionPreference = ['reject_once', 'reject_always']

function pick(
  options: readonly PermissionOption[],
  prefer: OptionPreference,
): PermissionOption | undefined {
  for (const kind of prefer) {
    const found = options.find((o) => o.kind === kind)
    if (found) return found
  }
  return undefined
}

/** 把策略答复翻译成 `permission.respond` 的参数；没有可用 option 时返回 `undefined`。 */
export function decisionToParams(
  request: Pick<PermissionRequest, 'request_id' | 'options'>,
  decision: PermissionDecision,
): PermissionRespondParams | undefined {
  if (decision === undefined || decision === null) return undefined
  const base = { request_id: request.request_id }
  if (typeof decision === 'string') {
    const prefer =
      decision === 'allow' ? ALLOW_ONCE : decision === 'allow_session' ? ALLOW_SESSION : DENY
    const option = pick(request.options, prefer)
    return option ? { ...base, option_id: option.id } : undefined
  }
  if ('answers' in decision) return { ...base, answers: decision.answers }
  if ('deny' in decision) {
    const option = pick(request.options, DENY)
    return option ? { ...base, option_id: option.id, message: decision.deny } : undefined
  }
  return {
    ...base,
    option_id: decision.option,
    ...(decision.message ? { message: decision.message } : {}),
  }
}

/** `allowAllWith` 的开关：默认都关。 */
export interface AllowAllOptions {
  /** 也放行 `plan`（批准 agent 的计划）。 */
  plan?: boolean
  /** 也放行 `mode_change`（切换权限模式，可能是危险模式）。 */
  modeChange?: boolean
}

/**
 * 自动放行：普通工具调用（`tool`）和 `other`；`plan` / `mode_change` 要显式开，`question` 没有「允许」的含义，
 * 一律交还给调用方。放行的是 `allow_once`。
 */
function allowAllWith(options: AllowAllOptions = {}): PermissionPolicy {
  return (request) => {
    switch (request.kind) {
      case 'tool':
      case 'other':
        return 'allow'
      case 'plan':
        return options.plan === true ? 'allow' : undefined
      case 'mode_change':
        return options.modeChange === true ? 'allow' : undefined
      case 'question':
        return undefined
    }
  }
}

/** 现成的策略。 */
export const policies = {
  /**
   * 放行普通工具调用（只在沙箱 / 一次性环境里用）。**不放行 `mode_change` 和 `plan`**，也不处理 `question`：
   * 这些请求留给调用方（或超时被 deny）。要连它们一起放行用 `allowAllWith({ modeChange: true, plan: true })`。
   */
  allowAll: allowAllWith(),
  /** 同 `allowAll`，但可以显式放行 `plan` / `mode_change`。 */
  allowAllWith,
  /** 全部拒绝。 */
  denyAll: (): PermissionDecision => 'deny',
  /** 只放行读类工具（file_read / search），其余一律拒绝；非 tool 类请求（plan / question）不处理。 */
  allowReadsDenyRest: (request: PermissionRequest): PermissionDecision => {
    if (request.kind !== 'tool') return undefined
    const category = request.detail.tool?.category
    return category === 'file_read' || category === 'search' ? 'allow' : 'deny'
  },
} as const satisfies Record<
  string,
  PermissionPolicy | ((options?: AllowAllOptions) => PermissionPolicy)
>
