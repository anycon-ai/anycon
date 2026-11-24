import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { TodoDetailPane } from '@/features/todos/ui/TodoDetailPane'

/** 手机上的单项详情：下一级页（顶栏有返回，走浏览器历史，spec/web.md § 4.12）。宽屏上由 `TodosPage` 在右半边显示同一个 `TodoDetailPane`。 */
export function TodoDetailPage({ node, todo }: { node: string; todo: string }) {
  const { t } = useTranslation()
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="todo-detail-page">
      <PageHeader crumbs={[{ label: t('todos.title'), to: { to: '/todos' } }, { label: t('todos.item') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto p-4">
        <TodoDetailPane node={node} todo={todo} />
      </div>
    </div>
  )
}
