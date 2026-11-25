// 「现在执行一次」（`todo.repeat_now`，docs/spec/daemon.md § 26.9，spec/web.md § 14.6）：周期项（模板）立刻复制出一项，不动它的下一次时间。
// 和派发一样直接发请求、不进离线待发队列：节点要在线，复制是一次性的动作，排队重放会在不知道的时候多出一项。
import { useMemo } from 'react'
import type { TFunction } from 'i18next'
import type { Todo } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { applyConfirmedTodo, todoTickNow } from './service'
import { useTodos } from './store'
import { isClosed } from './types'

/** 现在执行一次。成功后把新的一项并进缓存（列表里马上就有）。 */
export async function runRepeatNow(nodeId: string, templateId: string): Promise<Todo> {
  const sentAt = todoTickNow()
  const copy = await rpc().request<Todo>(nodeId, 'todo.repeat_now', { id: templateId })
  await applyConfirmedTodo(nodeId, copy, sentAt)
  return copy
}

/** 缓存里这个模板复制出来的、还没关闭的最新一份（有的话「现在执行」会被节点拒绝：上一份没关闭）。 */
export function useOpenCopy(nodeId: string, templateId: string): Todo | undefined {
  const items = useTodos((s) => s.nodes[nodeId]?.items)
  return useMemo(() => {
    const open = Object.values(items ?? {}).filter((t) => t.source.kind === 'repeat' && t.source.todo_id === templateId && !isClosed(t.state))
    return open.sort((a, b) => b.created_ms - a.created_ms)[0]
  }, [items, templateId])
}

/** 失败的说明。链路断了的情况要说清：这次可能已经复制出来了，先看一眼列表再决定。 */
export function describeRepeatNowError(e: unknown, t: TFunction): string {
  if (e instanceof RpcCallError) {
    if (e.kind !== 'rpc') return t('todos.repeat.runNow.errors.link')
    switch (e.code) {
      case 'invalid':
        if (e.reason === 'not_template') return t('todos.repeat.runNow.errors.notTemplate')
        if (e.reason === 'closed') return t('todos.repeat.runNow.errors.closed')
        if (e.reason === 'unreviewed') return t('todos.repeat.runNow.errors.unreviewed')
        if (e.reason === 'previous_open') return t('todos.repeat.runNow.errors.previousOpen')
        if (e.reason === 'too_many') return t('todos.errors.full')
        break
      case 'forbidden':
        return t('todos.repeat.runNow.errors.forbidden')
      case 'not_found':
        return t('todos.repeat.runNow.errors.notFound')
      case 'unsupported':
        return t('todos.repeat.runNow.errors.unsupported')
    }
  }
  return t('todos.repeat.runNow.errors.failed')
}
