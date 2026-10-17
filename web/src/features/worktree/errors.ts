// git / worktree 相关错误的说明（ADR-34）：daemon 在 `data.reason` 里给稳定的原因，按原因查专门的说法，
// 没有再退回按错误码的通用说法（`describeError`）。
import type { TFunction } from 'i18next'
import { describeError } from '@/lib/errors'
import { RpcCallError } from '@/transport/rpcClient'

/** daemon 的 `data.reason`（只看 rpc 错误）。 */
export function gitErrorReason(e: unknown): string | undefined {
  return e instanceof RpcCallError && e.kind === 'rpc' ? e.reason : undefined
}

/** 这些原因的 daemon 消息里带着 git 的输出尾部，值得原样给用户看（调试用）。 */
const HAS_GIT_OUTPUT = new Set(['fetch_failed', 'git_failed'])

/** git 输出的尾部（只有 `fetch_failed` / `git_failed` 有），其它情况返回 undefined。 */
export function gitErrorOutput(e: unknown): string | undefined {
  const reason = gitErrorReason(e)
  return reason && HAS_GIT_OUTPUT.has(reason) && e instanceof Error && e.message.trim() ? e.message.trim() : undefined
}

export function describeGitError(e: unknown, t: TFunction): string {
  const reason = gitErrorReason(e)
  if (reason) {
    const text = t(`git.errors.${reason}`, { defaultValue: '' })
    if (text) return text
  }
  // `busy`：同步 base 时 turn 在跑（或另一次同步还没结束）
  if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'busy') return t('git.errors.busy')
  return describeError(e, t)
}

/**
 * 归档失败的说明。daemon 在会话运行 / 等审批时回 `busy`（所有会话）：要先停止或答复，
 * 不是同步 base 那句「agent 在跑 turn」。其余原因（worktree 处理失败等）照 `describeGitError`。
 */
export function describeArchiveError(e: unknown, t: TFunction): string {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'busy' ? t('info.archiveBusy') : describeGitError(e, t)
}

/**
 * 取消归档失败的说明。daemon 在重建被删掉的 worktree 失败时回 `internal` / `git_failed`，会话仍是归档的；
 * 这个原因在同步 base 时是「git 合并失败」，在这里不能照那句话说。
 */
export function describeUnarchiveError(e: unknown, t: TFunction): string {
  return gitErrorReason(e) === 'git_failed' ? t('git.errors.unarchive_failed') : describeGitError(e, t)
}

/** 新建会话（带 worktree）失败时的说明：分支已存在、项目不能开 worktree、base 解析不了各有专门的说法。 */
export function describeCreateError(e: unknown, t: TFunction, worktree: boolean): string {
  if (worktree && e instanceof RpcCallError && e.kind === 'rpc') {
    if (e.code === 'exists') return t('git.errors.branchExists')
    const reason = e.reason
    if (reason) {
      const text = t(`git.errors.${reason}`, { defaultValue: '' })
      if (text) return text
    }
  }
  return describeError(e, t)
}
