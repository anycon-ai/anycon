import { TwinHistoryEvent } from '@/features/twin/TodoParts'
import { useEffect, useMemo } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Markdown } from '@/components/Markdown'
import { SessionDot } from '@/components/StatusDot'
import { formatDateTime } from '@/lib/format'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import type { TodoEvent } from '@/protocol'
import { useHistoryEntry, loadHistory, loadMoreHistory } from '../history'
import { wasDispatched } from '../activity'
import type { ViewTodo } from '../types'

/** 到会话页的链接 + 它现在的状态点；会话不在了（被删了）就写明，不留一个点了没用的链接。 */
export function SessionRef({ nodeId, sessionId, label }: { nodeId: string; sessionId: string; label?: string }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const sess = useSessions((s) => s.list[sessionId])
  if (!sess) {
    return (
      <span className="text-xs text-fg-subtle" data-testid="history-session-gone">
        {t('todos.history.sessionGone')}
      </span>
    )
  }
  return (
    <Link to="/n/$node/s/$session" params={{ node: shortNodeId(nodeId, order), session: sessionId }} className="inline-flex min-h-8 items-center gap-1.5 text-sm font-medium text-accent-text hover:underline max-md:min-h-11" data-testid="history-session-link" data-status={sess.status}>
      <SessionDot status={sess.status} />
      <span>{label ?? t('todos.history.openSession')}</span>
      <span className="sr-only">{t(`status.session.${sess.status}`)}</span>
    </Link>
  )
}

/**
 * 「经过」（spec/web.md § 4.12）：派发、每一轮的结果、人改的状态，从旧到新；下面是每个关联会话和它现在的状态。
 * `result` 的文字是 agent 写的，按不可信的内容渲染（同一条消毒的 Markdown 管线，从不当 HTML）。只在派发过的项上出现。
 */
export function TodoHistory({ item }: { item: ViewTodo }) {
  const { t, i18n } = useTranslation()
  const order = useNodes((s) => s.order)
  const entry = useHistoryEntry(item.node_id, item.id)
  const ready = useNodes((s) => s.byId[item.node_id]?.conn === 'ready')
  const synced = useNodes((s) => s.byId[item.node_id]?.synced === true)
  const dispatched = wasDispatched(item)
  // 周期项（模板）的「经过」里有每次复制和跳过
  const isTemplate = !!item.repeat
  const twinSupported = useNodes((s) => s.byId[item.node_id]?.info?.capabilities.twin === true)
  // 打开时读第一页；节点重连并同步完之后再读一次（断线期间的事件没有通知）
  useEffect(() => {
    if ((dispatched || twinSupported || isTemplate) && ready && synced) void loadHistory(item.node_id, item.id)
  }, [dispatched, twinSupported, isTemplate, ready, synced, item.node_id, item.id])
  const chronological = useMemo(() => [...entry.events].reverse(), [entry.events])
  // 每个会话是派给哪个 agent 的（`result` 行用）
  const agentOf = useMemo(() => {
    const m = new Map<string, string>()
    for (const e of entry.events) if (e.kind === 'dispatched') m.set(e.session_id, e.agent)
    return m
  }, [entry.events])
  if (!dispatched && entry.events.length === 0 && !entry.loading && !entry.failed) return null

  const when = (ms: number): string => formatDateTime(ms, i18n.language)
  const stateText = (s: string): string => t(`todos.states.${s}`, { defaultValue: cleanHintText(s) })
  const row = (e: TodoEvent) => {
    switch (e.kind) {
      case 'dispatched':
        return (
          <li key={e.seq} className="space-y-1" data-testid="history-row" data-kind="dispatched">
            <p>
              <span className="text-fg-subtle">{when(e.ts_ms)}</span> {t('todos.history.dispatched', { agent: cleanHintText(e.agent) || '?' })}
              {' · '}
              {e.workspace_mode === 'worktree' ? t('todos.history.worktree') : t('todos.history.localDir')}
              {' · '}
              {e.no_push ? t('todos.history.noPush') : t('todos.history.pushAllowed')}
            </p>
            <SessionRef nodeId={item.node_id} sessionId={e.session_id} />
          </li>
        )
      case 'result':
        return (
          <li key={e.seq} className="space-y-1" data-testid="history-row" data-kind="result">
            <p>
              <span className="text-fg-subtle">{when(e.ts_ms)}</span> {t('todos.history.result', { agent: cleanHintText(agentOf.get(e.session_id)) || t('todos.history.agent'), turn: e.turn })}
            </p>
            {e.text ? (
              <div className="scroll-thin max-h-48 overflow-auto rounded-md border border-border p-2" data-testid="history-result-text">
                <Markdown text={e.text} />
              </div>
            ) : (
              <p className="text-xs text-fg-subtle">{t('todos.history.noText')}</p>
            )}
            {e.truncated ? <p className="text-xs text-fg-subtle">{t('todos.history.truncated')}</p> : null}
            <SessionRef nodeId={item.node_id} sessionId={e.session_id} />
          </li>
        )
      case 'state':
        // 周期项开着「Bot 做完自动关闭」时，关闭是 daemon 按设置做的，不是人改的
        return (
          <li key={e.seq} data-testid="history-row" data-kind="state" data-by={e.by.kind}>
            <span className="text-fg-subtle">{when(e.ts_ms)}</span> {e.by.kind === 'repeat' ? t('todos.history.closedByRepeat') : t('todos.history.state', { from: stateText(e.from), to: stateText(e.to) })}
          </li>
        )
      case 'twin_decision':
      case 'handed_back':
      case 'taken_over':
      case 'handler':
      case 'twin_question':
      case 'pr_event':
      case 'remote_dispatched':
      case 'remote_result':
        return <li key={e.seq} data-testid="history-row" data-kind={e.kind}><span className="text-fg-subtle">{when(e.ts_ms)}</span><TwinHistoryEvent nodeId={item.node_id} event={e} /></li>
      case 'repeat_created':
        return (
          <li key={e.seq} data-testid="history-row" data-kind="repeat_created">
            <span className="text-fg-subtle">{when(e.ts_ms)}</span> {e.manual ? t('todos.history.repeatCreatedManual') : t('todos.history.repeatCreated', { time: when(e.at_ms) })}{' '}
            <Link to="/todos/$node/$todo" params={{ node: shortNodeId(item.node_id, order), todo: e.todo_id }} className="font-medium text-accent-text hover:underline" data-testid="history-repeat-copy">
              {t('todos.history.openCopy')}
            </Link>
          </li>
        )
      case 'repeat_skipped':
        return (
          <li key={e.seq} data-testid="history-row" data-kind="repeat_skipped">
            <span className="text-fg-subtle">{when(e.ts_ms)}</span> {t('todos.history.repeatSkipped', { time: when(e.at_ms) })}
          </li>
        )
      default:
        // 更新的 daemon 加的种类：这个版本不认识，不画
        return null
    }
  }

  return (
    <section className="space-y-2" aria-labelledby="todo-history-title" data-testid="todo-history">
      <h3 id="todo-history-title" className="text-sm font-semibold">
        {t('todos.history.title')}
      </h3>
      {entry.failed ? (
        <div className="flex flex-wrap items-center gap-2 text-sm text-danger" role="alert" data-testid="history-failed">
          {t('todos.history.failed')}
          <Button size="sm" onClick={() => void loadHistory(item.node_id, item.id)} data-testid="history-retry">
            {t('todos.retry')}
          </Button>
        </div>
      ) : null}
      {entry.loading && !entry.loaded ? <Spinner className="size-5" /> : null}
      {entry.hasMore ? (
        <Button size="sm" disabled={entry.loading} onClick={() => void loadMoreHistory(item.node_id, item.id)} data-testid="history-more">
          {t('todos.history.earlier')}
        </Button>
      ) : null}
      <ol className="space-y-3 border-l border-border pl-3 text-sm" aria-label={t('todos.history.title')}>
        {chronological.map(row)}
      </ol>
      <ul className="space-y-1 text-sm" aria-label={t('todos.history.sessions')} data-testid="history-sessions">
        {(item.sessions ?? []).map((sid, i) => (
          <li key={sid} className="flex items-center gap-2">
            <span className="text-fg-subtle">{t('todos.history.sessionN', { n: i + 1 })}</span>
            <SessionRef nodeId={item.node_id} sessionId={sid} />
          </li>
        ))}
      </ul>
    </section>
  )
}
