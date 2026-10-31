import { createFileRoute } from '@tanstack/react-router'
import { parseSeqParam } from '@/lib/nav'
import { SessionPage } from '@/pages/SessionPage'

export const Route = createFileRoute('/_app/_shell/n/$node/s/$session')({
  // `?seq=`：搜索命中带来的事件 seq，会话页据此加载并定位到那条消息
  validateSearch: (s: Record<string, unknown>): { seq?: number } => ({ seq: parseSeqParam(s.seq) }),
  component: function Session() {
    const { session } = Route.useParams()
    const { seq } = Route.useSearch()
    return <SessionPage key={session} sessionId={session} focusSeq={seq} />
  },
})
