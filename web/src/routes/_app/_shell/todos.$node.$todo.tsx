import { createFileRoute } from '@tanstack/react-router'
import { TodoRoute } from '@/components/TabHost'

export const Route = createFileRoute('/_app/_shell/todos/$node/$todo')({
  component: function TodoItem() {
    const { node, todo } = Route.useParams()
    return <TodoRoute node={node} todo={todo} />
  },
})
