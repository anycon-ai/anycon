// 从清单项派出的会话（`SessionSummary.todo_id`）：会话页顶栏的 chip 和右栏信息面板里列出那一项（spec/web.md § 4.6、§ 4.12）。
// 项的标题是用户 / agent 写的文字，一律清理成单行再显示。已经关闭的项不在没关闭的清单缓存里：缓存里没有就向节点要一次（`todo.get`），
// 要不到（`not_found`）才说「来源清单项已被删除」；节点连不上时什么都不猜。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ListChecks } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { Todo } from '@/protocol'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { applyConfirmedTodo } from '../service'
import { useTodos } from '../store'
import { cleanOneLine } from '../validate'

/** 在途的 `todo.get`：顶栏 chip 和信息面板同时要同一项时只问一次。 */
const asking = new Map<string, Promise<Todo>>()
function askNode(nodeId: string, todoId: string): Promise<Todo> {
  const key = `${nodeId}:${todoId}`
  let p = asking.get(key)
  if (!p) {
    p = rpc().request<Todo>(nodeId, 'todo.get', { id: todoId })
    asking.set(key, p)
    void p.then(
      () => asking.delete(key),
      () => asking.delete(key),
    )
  }
  return p
}

export type Origin = { state: 'loading' } | { state: 'found'; todo: Todo } | { state: 'deleted' } | { state: 'unknown' }

export function useOriginTodo(nodeId: string, todoId: string): Origin {
  const cached = useTodos((s) => s.nodes[nodeId]?.items[todoId])
  const ready = useNodes((s) => s.byId[nodeId]?.conn === 'ready' && s.byId[nodeId]?.info?.capabilities.todo === true)
  const [asked, setAsked] = useState<Origin | null>(null)
  useEffect(() => {
    if (cached || !ready) return
    let live = true
    askNode(nodeId, todoId).then(
        (todo) => {
          if (!live) return
          setAsked({ state: 'found', todo })
          void applyConfirmedTodo(nodeId, todo)
        },
        (e) => live && setAsked({ state: e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found' ? 'deleted' : 'unknown' }),
      )
    return () => {
      live = false
    }
  }, [cached, ready, nodeId, todoId])
  if (cached) return { state: 'found', todo: cached }
  return asked ?? { state: 'loading' }
}

/** 到那一项的链接（标题清理过）；项没了就写明，不留一个点了落空的链接。读不到（还没读完、节点连不上）时什么都不画。 */
export function TodoOriginLink({ nodeId, todoId, className }: { nodeId: string; todoId: string; className?: string }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const o = useOriginTodo(nodeId, todoId)
  if (o.state === 'found') {
    const title = cleanOneLine(o.todo.title, 80)
    return (
      <Link to="/todos/$node/$todo" params={{ node: shortNodeId(nodeId, order), todo: todoId }} title={title} aria-label={t('session.fromTodo', { title })} className={className} data-testid="session-todo-link">
        <ListChecks className="size-3.5 shrink-0" aria-hidden />
        <span className="truncate">{title}</span>
      </Link>
    )
  }
  if (o.state === 'deleted') {
    return (
      <span className={className} data-testid="session-todo-deleted">
        <ListChecks className="size-3.5 shrink-0" aria-hidden />
        <span className="truncate">{t('session.todoDeleted')}</span>
      </span>
    )
  }
  return null
}
