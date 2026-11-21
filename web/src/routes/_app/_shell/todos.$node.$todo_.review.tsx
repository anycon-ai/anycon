import { createFileRoute } from '@tanstack/react-router'
import { TodoReviewPage } from '@/pages/TodoReviewPage'

// `$todo_`：不嵌在 `/todos/$node/$todo`（单项详情）下面，验收页是自己的全屏页面
export const Route = createFileRoute('/_app/_shell/todos/$node/$todo_/review')({
  component: function TodoReviewRoute() {
    const { node, todo } = Route.useParams()
    return <TodoReviewPage node={node} todo={todo} />
  },
})
