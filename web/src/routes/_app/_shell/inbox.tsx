import { createFileRoute } from '@tanstack/react-router'
import { KeptRoute } from '@/components/PageHosts'

export const Route = createFileRoute('/_app/_shell/inbox')({
  // `?node=&session=`：从通知点进来，页面据此定位那个会话最早的一条待答复请求（spec/web.md § 7）
  validateSearch: (s: Record<string, unknown>): { node?: string; session?: string; todo?: string } => ({
    todo: typeof s.todo === 'string' && s.todo ? s.todo : undefined,
    node: typeof s.node === 'string' && s.node ? s.node : undefined,
    session: typeof s.session === 'string' && s.session ? s.session : undefined,
  }),
  component: () => <KeptRoute page="inbox" />,
})
