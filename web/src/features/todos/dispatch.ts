// 让 agent 处理一项（`todo.dispatch` / `todo.dispatch_preview`，docs/spec/daemon.md § 22，spec/web.md § 4.12）：
// 全部逻辑（开会话、关联、`activity`、「不 push、不开 PR」、第一条消息）在 daemon 里，这里只发请求、把结果并进缓存、把错误翻成人话。
// 派发直接发请求，不进离线待发队列：节点要在线，会话是一次性的，重放没有意义。
import type { TFunction } from 'i18next'
import type { AgentInfo, SessionWorkspace, SessionSettings, TodoDispatchParams, TodoDispatchPreviewResult, TodoDispatchResult } from '@/protocol'
import { describeCreateError } from '@/features/worktree/errors'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useSessions } from '@/stores/sessions'
import { applyConfirmedTodo, todoTickNow } from './service'

/** agent 有没有声明「不 push、不开 PR」这个选项（`AgentInfo.options` 里 id 为 `no_push` 的布尔选项）。 */
export const declaresNoPush = (agent: Pick<AgentInfo, 'options'> | undefined): boolean => (agent?.options ?? []).some((o) => o.id === 'no_push' && o.kind === 'bool')

export interface DispatchArgs {
  id: string
  agent: string
  project: string
  workspace?: SessionWorkspace
  settings: SessionSettings
  noPush: boolean
  /** 幂等：同一个值的重试（回执丢了）返回第一次的结果，不开第二个会话。 */
  messageId: string
}

/** 派发。成功后把更新的项和新会话并进缓存（会话的状态点马上就有）。 */
export async function dispatchTodo(nodeId: string, a: DispatchArgs): Promise<TodoDispatchResult> {
  const params: TodoDispatchParams = {
    id: a.id,
    agent: a.agent,
    project: a.project,
    ...(a.workspace ? { workspace: a.workspace } : {}),
    settings: a.settings,
    no_push: a.noPush,
    message_id: a.messageId,
  }
  const sentAt = todoTickNow()
  const res = await rpc().request<TodoDispatchResult>(nodeId, 'todo.dispatch', params)
  await applyConfirmedTodo(nodeId, res.todo, sentAt)
  useSessions.getState().upsertSummaries(nodeId, [res.session])
  return res
}

/** 此刻派发会发出的第一条消息（`todo.dispatch_preview`），一字不差。 */
export async function previewDispatch(nodeId: string, id: string, noPush: boolean): Promise<string> {
  const res = await rpc().request<TodoDispatchPreviewResult>(nodeId, 'todo.dispatch_preview', { id, no_push: noPush })
  return res.first_message
}

/**
 * 派发 / 预览失败的说明。先看派发自己的原因（已关闭、没有项目、「不 push」做不到、20 次上限、旧 daemon），
 * 再是新建会话的那些（agent 不可用、设置不被接受、worktree 的各种问题），最后是通用的。
 * 链路断了的情况要说清：这次派发可能已经成功，重试是安全的（同一个 `message_id` 不会开第二个会话）。
 */
export function describeDispatchError(e: unknown, t: TFunction, worktree: boolean): string {
  if (e instanceof RpcCallError) {
    if (e.kind !== 'rpc') return `${describeCreateError(e, t, worktree)} ${t('dispatch.errors.linkRetry')}`
    const reason = e.reason
    switch (e.code) {
      case 'invalid':
        if (reason === 'closed') return t('dispatch.errors.closed')
        if (reason === 'too_many_sessions') return t('dispatch.errors.tooManySessions')
        if (e.field === 'project') return t('dispatch.errors.noProject')
        if (e.field === 'message_id') return t('dispatch.errors.messageId')
        break
      case 'unsupported':
        // 「不 push」做不到：所选 agent 没有这个选项；别的 `unsupported`（旧 daemon 没有这个方法）是节点不认识派发
        return reason === 'no_push' || e.field === 'no_push' ? t('dispatch.errors.noPush') : t('dispatch.errors.unsupported')
      case 'forbidden':
        return t('dispatch.errors.forbidden')
      case 'not_found':
        return t('dispatch.errors.notFound')
    }
  }
  return describeCreateError(e, t, worktree)
}
