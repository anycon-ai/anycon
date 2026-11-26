// 清单中的 Bot 入口和经过（「经过」里每条 Bot 事件是时间线上的一张卡片，见 `HistoryCard`）；处理人变化继续走原有待发队列。
import { useState, type ReactNode } from 'react'
import { ArrowLeftRight, Bot, FileText, GitPullRequest, MessageCircleQuestion, Network, Undo2, UserRound, type LucideIcon } from 'lucide-react'
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
import { usePeerName } from './peer'
import { Markdown } from '@/components/Markdown'
import { HistoryCard, historyQuoteClass, type HistoryTone } from '@/features/todos/ui/HistoryCard'

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
type FrameProps = { icon: LucideIcon; tone?: HistoryTone; title: ReactNode; note?: ReactNode; actions?: ReactNode; children?: ReactNode; testId?: string }
/**
 * 一条 Bot 事件的外框：给了 `time` 就是「经过」时间线里的一张卡片（`HistoryCard`），
 * 没给就是一段不带框的文字（验收页的「Bot 的决定和理由」自己有框）。
 */
function Frame({ time, ...p }: FrameProps & { time?: string }) {
  if (time !== undefined) return <HistoryCard time={time} {...p} />
  return (
    <div className="space-y-1" data-testid={p.testId}>
      {p.note ? <p className="text-xs text-fg-subtle">{p.note}</p> : null}
      <p>{p.title}</p>
      {p.children}
      {p.actions ? <div className="flex flex-wrap items-center gap-x-4 gap-y-1">{p.actions}</div> : null}
    </div>
  )
}
/** `time` 有值时画成「经过」时间线里的卡片（`TodoHistory`），否则是一段文字。 */
export function TwinHistoryEvent({ nodeId, event, time }: { nodeId: string; event: TodoEvent; time?: string }) {
  const { t } = useTranslation()
  const supported = useNodes((s) => s.byId[nodeId]?.info?.capabilities.twin === true)
  const questions = useTwin((s) => s.nodes[nodeId]?.questions)
  const order = useNodes((s) => s.order)
  const run = (id: string) =>
    supported ? (
      <Link to="/twin" search={{ node: shortNodeId(nodeId, order), run: id }} className="font-medium text-accent-text hover:underline">
        {t('twin.openRun')}
      </Link>
    ) : null
  switch (event.kind) {
    case 'twin_decision':
      return (
        <Frame time={time} icon={Bot} tone="accent" note={t('twin.agentWritten')} title={t('twin.decision', { action: t(`twin.action.${event.action}`) })} actions={run(event.run_id)}>
          <p className="whitespace-pre-wrap break-words">{event.reason}</p>
        </Frame>
      )
    case 'handed_back':
      return (
        <Frame time={time} icon={Undo2} tone="warning" title={t('twin.handedBack')} actions={event.run_id ? run(event.run_id) : null}>
          <p className="whitespace-pre-wrap break-words">{event.reason}</p>
        </Frame>
      )
    case 'taken_over':
      return event.node_id ? <TakenOverOn nodeId={event.node_id} time={time} /> : <Frame time={time} icon={UserRound} title={t('twin.takenOver')} />
    case 'handler':
      return (
        <Frame
          time={time}
          icon={ArrowLeftRight}
          title={t('twin.handlerChanged', {
            from: t(event.from === 'twin' ? 'todos.handler.twin' : 'todos.handler.me'),
            to: t(event.to === 'twin' ? 'todos.handler.twin' : 'todos.handler.me'),
          })}
        />
      )
    case 'pr_event': {
      // 只给 GitHub 的地址做链接：地址来自 gh 的输出，别的不当作链接
      const href = event.url?.startsWith('https://github.com/') ? event.url : null
      return (
        <Frame
          time={time}
          icon={GitPullRequest}
          tone={event.event === 'merged' ? 'success' : event.event === 'checks_failed' ? 'warning' : 'neutral'}
          title={t(`twin.prEvent.${event.event}`, { defaultValue: t('twin.prEvent.unknown') })}
          actions={
            href ? (
              <a href={href} target="_blank" rel="noopener noreferrer" className="font-medium text-accent-text hover:underline">
                {t('twin.prEvent.open')}
              </a>
            ) : null
          }
        />
      )
    }
    case 'remote_dispatched':
      return <RemoteDispatched event={event} time={time} />
    case 'remote_result':
      return <RemoteResult event={event} time={time} />
    case 'twin_question': {
      const q = questions?.find((x) => x.id === event.question_id)
      return (
        <Frame time={time} icon={MessageCircleQuestion} tone="accent" title={t('twin.questionTitle')}>
          {q && supported ? <TwinQuestionForm key={q.id} nodeId={nodeId} question={q} /> : <p className="text-fg-muted">{t('twin.questionLoading')}</p>}
        </Frame>
      )
    }
    default:
      return null
  }
}
/** 接手发生在 Bot 用的另一个节点上：写节点名（这台设备没和它配过对时用 id 的前 8 位）。 */
function TakenOverOn({ nodeId, time }: { nodeId: string; time?: string }) {
  const { t } = useTranslation()
  const name = usePeerName(nodeId)
  return <Frame time={time} icon={UserRound} title={<span data-testid="taken-over-on">{t('twin.takenOverOn', { node: name })}</span>} />
}
type RemoteDispatchedEvent = Extract<TodoEvent, { kind: 'remote_dispatched' }>
type RemoteResultEvent = Extract<TodoEvent, { kind: 'remote_result' }>
/** 这台设备也和那个节点配过对时，才有它的节点页；否则只写名字。 */
function useRemoteRoute(nodeId: string): string | null {
  const order = useNodes((s) => s.order)
  const paired = useNodes((s) => !!s.byId[nodeId])
  return paired ? shortNodeId(nodeId, order) : null
}
const linkClass = 'font-medium text-accent-text hover:underline'
function RemoteDispatched({ event, time }: { event: RemoteDispatchedEvent; time?: string }) {
  const { t } = useTranslation()
  const name = usePeerName(event.node_id)
  const route = useRemoteRoute(event.node_id)
  return (
    <Frame
      time={time}
      icon={Network}
      tone="accent"
      testId="remote-dispatched"
      title={t('twin.remote.dispatched', { node: name })}
      actions={
        route ? (
          <>
            <Link to="/todos/$node/$todo" params={{ node: route, todo: event.todo_id }} className={linkClass}>
              {t('twin.remote.openItem', { node: name })}
            </Link>
            <Link to="/n/$node/s/$session" params={{ node: route, session: event.session_id }} className={linkClass}>
              {t('twin.remote.openSession', { node: name })}
            </Link>
          </>
        ) : null
      }
    >
      {route ? null : <p className="text-xs text-fg-subtle">{t('twin.remote.notPaired')}</p>}
    </Frame>
  )
}
function RemoteResult({ event, time }: { event: RemoteResultEvent; time?: string }) {
  const { t } = useTranslation()
  const name = usePeerName(event.node_id)
  const route = useRemoteRoute(event.node_id)
  return (
    <Frame
      time={time}
      icon={FileText}
      tone="success"
      testId="remote-result"
      title={t('twin.remote.result', { node: name })}
      actions={
        route ? (
          <Link to="/n/$node/s/$session" params={{ node: route, session: event.session_id }} className={linkClass}>
            {t('twin.remote.openSession', { node: name })}
          </Link>
        ) : null
      }
    >
      {event.text ? (
        <div className={historyQuoteClass} data-testid="remote-result-text">
          <Markdown text={event.text} />
        </div>
      ) : (
        <p className="text-xs text-fg-subtle">{t('todos.history.noText')}</p>
      )}
    </Frame>
  )
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
