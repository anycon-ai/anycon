import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { TodoReviewPane } from '@/features/todos/ui/TodoReviewPane'

/** 验收页（`/todos/:node/:todo/review`，spec/web.md § 2、§ 4.12）：全屏的二级页，手机上标签栏收起，返回走浏览器历史。 */
export function TodoReviewPage({ node, todo }: { node: string; todo: string }) {
  const { t } = useTranslation()
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="todo-review-page">
      <PageHeader crumbs={[{ label: t('todos.title'), to: { to: '/todos' } }, { label: t('todos.item'), to: { to: '/todos/$node/$todo', params: { node, todo } } }, { label: t('todos.reviewPage.page') }]} showNodeStatus={false} />
      <TodoReviewPane node={node} todo={todo} />
    </div>
  )
}
