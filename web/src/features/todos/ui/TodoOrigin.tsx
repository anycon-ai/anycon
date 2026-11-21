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
import { applyConfirmedTodo, todoTickNow } from '../service'
import { useTodos } from '../store'
import { cleanOneLine } from '../validate'

/** 在途的 `todo.get`：顶栏 chip 和信息面板同时要同一项时只问一次。 */
const asking = new Map<string, { promise: Promise<Todo>; sentAt: number }>()
function askNode(nodeId: string, todoId: string): { promise: Promise<Todo>; sentAt: number } {
  const key = `${nodeId}:${todoId}`
  let a = asking.get(key)
  if (!a) {
    // 发出之前的通知序号：回执回来时如果这一项在这期间收到过 `todo.changed`，回执就不盖过它（`applyConfirmedTodo`）
    a = { promise: rpc().request<Todo>(nodeId, 'todo.get', { id: todoId }), sentAt: todoTickNow() }
    asking.set(key, a)
    void a.promise.then(
      () => asking.delete(key),
      () => asking.delete(key),
    )
  }
  return a
}

/**
 * `deleted`：节点明确回了 `not_found`。`unreachable`：链路断了 / 超时（说不清有没有），`retry` 再问一次。
 * `offline`：节点现在连不上而缓存里又没有这一项。`unknown`：节点回了别的错误。
 */
export type Origin = { state: 'loading' } | { state: 'found'; todo: Todo } | { state: 'deleted' } | { state: 'unreachable'; retry: () => void } | { state: 'offline' } | { state: 'unknown'; retry: () => void }

export function useOriginTodo(nodeId: string, todoId: string): Origin {
  const cached = useTodos((s) => s.nodes[nodeId]?.items[todoId])
  const ready = useNodes((s) => s.byId[nodeId]?.conn === 'ready' && s.byId[nodeId]?.info?.capabilities.todo === true)
  const [asked, setAsked] = useState<{ key: string; origin: Origin } | null>(null)
  const [attempt, setAttempt] = useState(0)
  const key = `${nodeId}:${todoId}`
  useEffect(() => {
    if (cached || !ready) return
    let live = true
    const retryAsk = (): void => {
      setAsked(null)
      setAttempt((n) => n + 1)
    }
    const a = askNode(nodeId, todoId)
    a.promise.then(
      (todo) => {
        if (!live) return
        setAsked({ key, origin: { state: 'found', todo } })
        void applyConfirmedTodo(nodeId, todo, a.sentAt)
      },
      (e) => {
        if (!live) return
        if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') setAsked({ key, origin: { state: 'deleted' } })
        else if (e instanceof RpcCallError && e.kind !== 'rpc') setAsked({ key, origin: { state: 'unreachable', retry: () => retryAsk() } })
        else setAsked({ key, origin: { state: 'unknown', retry: () => retryAsk() } })
      },
    )
    return () => {
      live = false
    }
  }, [cached, ready, nodeId, todoId, key, attempt])
  if (cached) return { state: 'found', todo: cached }
  if (!ready) return { state: 'offline' }
  return asked && asked.key === key ? asked.origin : { state: 'loading' }
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
