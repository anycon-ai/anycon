import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import { nodeSupportsTodo, useTodoView, useTodos } from '../store'
import { resolveTodoRoute } from './TodoDetailPane'
import { useOriginTodo } from './TodoOrigin'
import { TodoReview } from './TodoReview'
import { useMemo } from 'react'

/** 地址里的 `:node/:todo` 解析成验收页的内容，说清每一种「看不到」的原因（和单项详情同一套）。 */
export function TodoReviewPane({ node, todo }: { node: string; todo: string }) {
  const { t } = useTranslation()
  const loaded = useNodes((s) => s.loaded)
  const order = useNodes((s) => s.order)
  const nodeId = resolveTodoRoute(node, order)
  const entry = useNodes((s) => (nodeId ? s.byId[nodeId] : undefined))
  const hydrated = useTodos((s) => (nodeId ? s.nodes[nodeId]?.hydrated === true : false))
  const ids = useMemo(() => (nodeId ? [nodeId] : []), [nodeId])
  const item = useTodoView(ids).find((v) => v.id === todo)
  // 已经关闭的项（刚通过的、刷新之后再打开的）不在没关闭的清单缓存里：缓存里没有就向节点要一次，要到的并进缓存
  const asked = useOriginTodo(nodeId ?? '', todo)
  const note = (text: string, testId: string) => (
    <p className="p-4 text-sm text-fg-subtle" role="status" data-testid={testId}>
      {text}
    </p>
  )
  const loading = (
    <div className="flex justify-center py-8" role="status" data-testid="review-loading">
      <Spinner className="size-6" />
    </div>
  )
  if (!loaded) return loading
  if (!nodeId || !entry) return note(t('todos.unknownNode'), 'review-unknown-node')
  if (!entry.info) return entry.conn === 'ready' ? loading : note(t('todos.reviewPage.offlineNotCached'), 'review-offline')
  if (!nodeSupportsTodo(nodeId)) return note(t('todos.nodeUnsupported', { node: cleanHintText(entry.name) || nodeId.slice(0, 8) }), 'review-unsupported-node')
  if (!hydrated) return loading
  if (!item) {
    // 只有节点明确说「没有这一项」才算没了；连不上、超时都说连不上（可以再试），节点离线又没缓存这一项就说明这一点，不转圈
    switch (asked.state) {
      case 'deleted':
        return note(t('todos.gone'), 'review-gone')
      case 'offline':
        return note(t('todos.reviewPage.offlineNotCached'), 'review-offline')
      case 'unreachable':
      case 'unknown':
        return (
          <div className="space-y-2 p-4 text-sm text-fg-subtle" role="status" data-testid={asked.state === 'unreachable' ? 'review-unreachable' : 'review-load-failed'}>
            <p>{asked.state === 'unreachable' ? t('todos.reviewPage.unreachable') : t('todos.reviewPage.loadFailed')}</p>
            <Button size="sm" onClick={asked.retry} data-testid="review-retry">
              {t('todos.reviewPage.retry')}
            </Button>
          </div>
        )
      default:
        return loading
    }
  }
  return <TodoReview key={`${item.node_id}:${item.id}`} item={item} online={entry.conn === 'ready'} />
}
