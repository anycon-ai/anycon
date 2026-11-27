// Twin 页「现在」：正在处理、在等你、排队中、上次没看的项。空的段不占标题，全空时只留一行说明。
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
import { cn } from '@/ui/cn'
import { CircleCheck } from 'lucide-react'
import { SectionTitle } from './viz'

/** 每一段标题前的小圆点：正在处理 = accent、在等你 = warning（要你动手），排队和上次没看的中性。旁边总有段名。 */
const DOT: Record<string, string> = { working: 'bg-accent-text', waiting: 'bg-warning', queued: 'bg-fg-subtle', deferred: 'bg-fg-subtle' }

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
    <section className="space-y-2" aria-label={t('twin.now.title')} data-testid="twin-now">
      <SectionTitle>{t('twin.now.title')}</SectionTitle>
      {!shown.length ? (
        <p className="flex items-center gap-2 rounded-lg bg-bg-subtle px-3 py-2.5 text-sm text-fg-muted">
          <CircleCheck className="size-4 shrink-0 text-fg-subtle" aria-hidden />
          {t('twin.now.none')}
        </p>
      ) : null}
      {shown.length ? <div className="grid gap-2 md:grid-cols-2">
      {shown.map((s) => (
        <div key={s.key} className="min-w-0 rounded-lg border border-border" data-testid={`twin-now-${s.key}`}>
          <div className="border-b border-border px-3 py-2">
            <h4 className="flex items-center gap-2 text-sm font-medium">
              <span className={cn('size-1.5 shrink-0 rounded-full', DOT[s.key])} aria-hidden />
              {s.heading} <span className="font-normal text-fg-subtle">({s.ids.length})</span>
            </h4>
            {s.hint ? <p className="mt-0.5 text-xs text-fg-subtle">{s.hint}</p> : null}
          </div>
          <ul className="divide-y divide-border/60 px-3">
            {s.ids.map((id) => {
              const open = s.key === 'waiting' ? questions?.find((q) => q.todo_id === id && q.status === 'open') : undefined
              return (
                <li key={id} className="flex min-h-9 min-w-0 flex-wrap items-center gap-x-2 py-1.5 text-sm">
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
      </div> : null}
    </section>
  )
}
