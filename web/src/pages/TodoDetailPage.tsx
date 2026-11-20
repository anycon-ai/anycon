import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { useTodoView } from '@/features/todos/store'
import { TodoDetail } from '@/features/todos/ui/TodoDetail'
import { resolveNodeParam } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { cleanOneLine } from '@/features/todos/validate'

/** 手机上的单项详情：全屏的二级页（标签栏收起，返回走浏览器历史，spec/web.md § 4.12）。宽屏上由 `TodosPage` 在右半边显示。 */
export function TodoDetailPage({ node, todo }: { node: string; todo: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const order = useNodes((s) => s.order)
  const nodeId = resolveNodeParam(node, order)
  const view = useTodoView(nodeId ? [nodeId] : [])
  const item = view.find((v) => v.id === todo)
  const online = useNodes((s) => (nodeId ? s.byId[nodeId]?.conn === 'ready' : false))
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="todo-detail-page">
      <PageHeader crumbs={[{ label: t('todos.title'), to: { to: '/todos' } }, { label: item ? cleanOneLine(item.title, 40) : t('todos.title') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto p-4">
        {item ? <TodoDetail item={item} online={online} onDeleted={() => void navigate({ to: '/todos', replace: true })} /> : <p className="text-sm text-fg-subtle">{t('todos.gone')}</p>}
      </div>
    </div>
  )
}
