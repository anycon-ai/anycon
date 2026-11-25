// Bot 页「现在」：正在处理、在等你、排队中、上次没看的项。空的段不占标题，全空时只留一行说明。
// 排队中和上次没看的项只有 `capabilities.twin_stats` 的节点才给；旧节点不显示这两段。
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { formatDuration } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { twinInboxUrl } from '@/features/sync/pushText'
import { useTodos } from '@/features/todos/store'
import { useTwin } from './store'

export function NowSections({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const value = useTwin((s) => s.nodes[node.id]?.value)
  const questions = useTwin((s) => s.nodes[node.id]?.questions)
  const todos = useTodos((s) => s.nodes[node.id]?.items)
  if (!value) return null
  const param = shortNodeId(node.id, order)
  const stats = node.info?.capabilities.twin_stats === true
  const title = (id: string): string => todos?.[id]?.title ?? id
  const todoLink = (id: string) => (
    <Link to="/todos/$node/$todo" params={{ node: param, todo: id }} className="text-accent-text">
      {title(id)}
    </Link>
  )
  const now = Date.now()
  const sections: Array<{ key: string; heading: string; ids: string[]; hint?: string }> = [
    { key: 'working', heading: t('twin.working'), ids: value.working },
    { key: 'waiting', heading: t('twin.waiting'), ids: value.waiting },
    ...(stats
      ? [
          { key: 'queued', heading: t('twin.now.queued'), ids: value.queued ?? [] },
          { key: 'deferred', heading: t('twin.now.deferred'), ids: value.deferred ?? [], hint: t('twin.now.deferredHint') },
        ]
      : []),
  ]
  const shown = sections.filter((s) => s.ids.length)
  return (
    <section className="space-y-3" aria-label={t('twin.now.title')} data-testid="twin-now">
      <h3 className="text-sm font-semibold">{t('twin.now.title')}</h3>
      {!shown.length ? <p className="text-sm text-fg-subtle">{t('twin.now.none')}</p> : null}
      {shown.map((s) => (
        <div key={s.key} data-testid={`twin-now-${s.key}`}>
          <h4 className="text-sm font-semibold">
            {s.heading} ({s.ids.length})
          </h4>
          {s.hint ? <p className="text-xs text-fg-subtle">{s.hint}</p> : null}
          <ul>
            {s.ids.map((id) => {
              const open = s.key === 'waiting' ? questions?.find((q) => q.todo_id === id && q.status === 'open') : undefined
              return (
                <li key={id} className="min-h-8">
                  {open ? (
                    <>
                      <a
                        href={twinInboxUrl(param, id)}
                        className="text-accent-text"
                        title={t('twin.now.answerIt')}
                        onClick={(e) => {
                          e.preventDefault()
                          navigateTo(twinInboxUrl(param, id))
                        }}
                      >
                        {title(id)}
                      </a>{' '}
                      <span className="text-xs text-fg-muted">{t('twin.now.waitingFor', { duration: formatDuration(Math.max(0, now - open.created_ms)) })}</span>
                    </>
                  ) : (
                    todoLink(id)
                  )}
                </li>
              )
            })}
          </ul>
        </div>
      ))}
    </section>
  )
}
