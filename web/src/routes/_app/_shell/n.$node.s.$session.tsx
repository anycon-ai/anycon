import { createFileRoute } from '@tanstack/react-router'
import { parseSeqParam } from '@/lib/nav'
import { SessionPage } from '@/pages/SessionPage'

export const Route = createFileRoute('/_app/_shell/n/$node/s/$session')({
  // `?seq=`：搜索命中带来的事件 seq，会话页据此加载并定位到那条消息
  // `?view=`：从清单的验收页 / 待处理的「打开改动」来的，直接打开「改动」视图（页面读到之后把它去掉）
  validateSearch: (s: Record<string, unknown>): { seq?: number; view?: 'changes' } => ({ seq: parseSeqParam(s.seq), ...(s.view === 'changes' ? { view: 'changes' as const } : {}) }),
  component: function Session() {
    const { session } = Route.useParams()
    const { seq, view } = Route.useSearch()
    return <SessionPage key={session} sessionId={session} focusSeq={seq} {...(view ? { openView: view } : {})} />
  },
})
