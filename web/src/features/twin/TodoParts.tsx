// 清单中的分身入口和经过；处理人变化继续走原有待发队列。
import { useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TodoEvent } from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { shortNodeId } from '@/lib/ids'
import { useHistoryEntry } from '@/features/todos/history'
import { updateTodo } from '@/features/todos/service'
import { isClosed, type ViewTodo } from '@/features/todos/types'
import { Button } from '@/ui/Button'
import { useTwin } from './store'
import { TwinQuestionForm } from './Question'

export function TwinHandlerButton({ item }: { item: ViewTodo }) {
  const { t } = useTranslation()
  const supported = useNodes((s) => s.byId[item.node_id]?.info?.capabilities.twin === true)
  const [busy, setBusy] = useState(false)
  const [failed, setFailed] = useState(false)
  if (!supported || item.unreviewed || isClosed(item.state) || (item.handler !== 'twin' && item.state !== 'open')) return null
  return (
    <div className="space-y-2">
      <Button
        disabled={busy}
        data-testid="todo-twin-handler"
        onClick={() => {
          setBusy(true)
          setFailed(false)
          void updateTodo(item.node_id, item.id, { handler: item.handler === 'twin' ? 'me' : 'twin' })
            .then((r) => setFailed(!r.ok))
            .finally(() => setBusy(false))
        }}
      >
        {t(item.repeat ? (item.handler === 'twin' ? 'twin.meEach' : 'twin.giveEach') : item.handler === 'twin' ? 'twin.me' : 'twin.give')}
      </Button>
      {failed ? <p role="alert">{t('twin.saveFailed')}</p> : null}
    </div>
  )
}
export function TwinHistoryEvent({ nodeId, event }: { nodeId: string; event: TodoEvent }) {
  const { t } = useTranslation()
  const supported = useNodes((s) => s.byId[nodeId]?.info?.capabilities.twin === true)
  const questions = useTwin((s) => s.nodes[nodeId]?.questions)
  const order = useNodes((s) => s.order)
  const run = (id: string) =>
    supported ? (
      <Link to="/twin" search={{ node: shortNodeId(nodeId, order), run: id }} className="text-accent-text">
        {t('twin.openRun')}
      </Link>
    ) : null
  switch (event.kind) {
    case 'twin_decision':
      return (
        <div>
          <p className="text-xs text-fg-subtle">{t('twin.agentWritten')}</p>
          <p>{t('twin.decision', { action: t(`twin.action.${event.action}`) })}</p>
          <p className="whitespace-pre-wrap break-words">{event.reason}</p>
          {run(event.run_id)}
        </div>
      )
    case 'handed_back':
      return (
        <div>
          <p>{t('twin.handedBack')}</p>
          <p className="whitespace-pre-wrap break-words">{event.reason}</p>
          {event.run_id ? run(event.run_id) : null}
        </div>
      )
    case 'taken_over':
      return <p>{t('twin.takenOver')}</p>
    case 'handler':
      return (
        <p>
          {t('twin.handlerChanged', {
            from: t(event.from === 'twin' ? 'todos.handler.twin' : 'todos.handler.me'),
            to: t(event.to === 'twin' ? 'todos.handler.twin' : 'todos.handler.me'),
          })}
        </p>
      )
    case 'twin_question': {
      const q = questions?.find((x) => x.id === event.question_id)
      return q && supported ? <TwinQuestionForm key={q.id} nodeId={nodeId} question={q} /> : <p>{t('twin.questionLoading')}</p>
    }
    default:
      return null
  }
}
export function TwinReviewReason({ item }: { item: ViewTodo }) {
  const { t } = useTranslation()
  const history = useHistoryEntry(item.node_id, item.id)
  const decision = history.events.find((e) => e.kind === 'twin_decision')
  if (decision?.kind !== 'twin_decision') return null
  return (
    <div className="space-y-1 rounded border border-border p-3" data-testid="review-twin-reason">
      <h3 className="text-sm font-semibold">{t('twin.reviewReason')}</h3>
      <TwinHistoryEvent nodeId={item.node_id} event={decision} />
    </div>
  )
}
