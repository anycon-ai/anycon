// 分身页的只读概览：配置摘要、今天的数字、连续失败的提醒。
// 没有 `capabilities.twin_stats` 时，今天的唤醒次数和最近一次唤醒由已读到的运行记录推算（浏览器本地日），不显示花费和下一次唤醒。
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import type { TwinRun, TwinRunBrief, TwinRunCounts } from '@/protocol'
import { formatDateTime } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { twinHead } from './state'
import { useTwin } from './store'
import { usageText } from './usage'

const count = (n: number | undefined): number => n ?? 0

export function ConfigSummary({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const scope = useTwin((s) => s.nodes[node.id]?.value?.scope)
  if (!scope) return null
  const agent = node.agents.find((a) => a.id === scope.agent)
  const mode = agent?.modes?.find((m) => m.id === scope.mode)
  const parts = [
    t('twin.cfg.projects', { count: scope.projects.length }),
    // 没设就是「默认」；设了但在节点的代理列表里找不到时，如实写 id
    scope.agent ? (agent?.label ?? scope.agent) : t('twin.default'),
    scope.mode ? (mode?.label ?? scope.mode) : t('twin.default'),
    t('twin.cfg.noPush'),
    t('twin.cfg.sessions', { count: scope.max_sessions }),
    t('twin.cfg.daily', { count: scope.daily_dispatches }),
  ]
  return (
    <p className="text-sm text-fg-muted" data-testid="twin-config-summary">
      {parts.join(' · ')} ·{' '}
      <Link to="/n/$node/settings" params={{ node: shortNodeId(node.id, order) }} search={{ tab: 'twin' }} className="text-accent-text">
        {t('twin.settingsLink')}
      </Link>
    </p>
  )
}

/** 真实运行（不含试跑）的最近一次，以及今天（浏览器本地日）按结果的计数。 */
export function localToday(runs: readonly TwinRun[], now: number = Date.now()): { counts: TwinRunCounts; last?: TwinRun } {
  const midnight = new Date(now).setHours(0, 0, 0, 0)
  const real = runs.filter((r) => !r.dry_run).sort((a, b) => b.started_ms - a.started_ms)
  const counts: Record<keyof TwinRunCounts, number> = { done: 0, idle: 0, failed: 0, cancelled: 0, budget: 0 }
  for (const r of real) if (r.started_ms >= midnight && r.result in counts) counts[r.result as keyof TwinRunCounts]++
  return { counts, ...(real[0] ? { last: real[0] } : {}) }
}

function Tile({ label, testId, children, detail }: { label: string; testId: string; children: React.ReactNode; detail?: React.ReactNode }) {
  return (
    <div className="rounded border border-border p-3" data-testid={testId}>
      <dt className="text-xs text-fg-subtle">{label}</dt>
      <dd className="mt-1 break-words text-base font-semibold">{children}</dd>
      {detail ? <dd className="mt-1 break-words text-xs text-fg-muted">{detail}</dd> : null}
    </div>
  )
}

/** 还没有 `finished_ms` 的运行仍在进行，`result` 还不算数，写「进行中」。 */
function lastWakeText(t: TFunction, lang: string, run: Pick<TwinRunBrief, 'started_ms' | 'triggers' | 'result' | 'finished_ms'>): string {
  const outcome = run.finished_ms == null ? t('twin.today.inProgress') : t(`twin.result.${run.result}`)
  return [formatDateTime(run.started_ms, lang), run.triggers.map((x) => t(`twin.trigger.${x}`)).join(', '), outcome].join(' · ')
}

export function TodayTiles({ node }: { node: NodeEntry }) {
  const { t, i18n } = useTranslation()
  const entry = useTwin((s) => s.nodes[node.id])
  const value = entry?.value
  if (!value) return null
  const stats = node.info?.capabilities.twin_stats === true
  const head = twinHead(node, entry)
  const day = stats ? entry?.stats?.at(-1) : undefined
  const local = localToday(entry?.runs ?? [])
  const runs = day ? day.runs : local.counts
  const done = count(runs?.done)
  const idle = count(runs?.idle)
  const failed = count(runs?.failed)
  const other = count(runs?.cancelled) + count(runs?.budget)
  const total = done + idle + failed + other
  const last: Pick<TwinRunBrief, 'started_ms' | 'triggers' | 'result' | 'finished_ms'> | undefined = stats ? (value.last_run ?? undefined) : local.last
  const judgeCap = node.info?.judge?.daily_calls
  const cost = day ? usageText(t, day.usage) : null
  const active = head === 'idle' || head === 'running'
  return (
    <section aria-label={t('twin.today.title')}>
      <h3 className="mb-2 text-sm font-semibold">{t('twin.today.title')}</h3>
      <dl className="grid grid-cols-2 gap-2 lg:grid-cols-3" data-testid="twin-today">
        <Tile
          label={t('twin.today.wakes')}
          testId="twin-tile-wakes"
          detail={
            <>
              {t('twin.today.wakesDetail', { done, idle, failed })}
              {other ? ` · ${t('twin.today.wakesOther', { count: other })}` : ''}
            </>
          }
        >
          {total}
        </Tile>
        <Tile label={t('twin.today.dispatches')} testId="twin-tile-dispatches">
          {value.today.dispatches} / {value.scope.daily_dispatches}
        </Tile>
        <Tile label={t('twin.today.judgeCalls')} testId="twin-tile-judges">
          {typeof judgeCap === 'number' ? `${value.today.judge_calls} / ${judgeCap}` : value.today.judge_calls}
        </Tile>
        {cost ? (
          <Tile label={t('twin.today.cost')} testId="twin-tile-cost">
            {cost}
          </Tile>
        ) : null}
        <Tile label={t('twin.today.lastWake')} testId="twin-tile-last">
          {last ? lastWakeText(t, i18n.language, last) : t('twin.today.never')}
        </Tile>
        {stats ? (
          <Tile label={t('twin.today.nextWake')} testId="twin-tile-next">
            {value.next_wake_ms != null ? formatDateTime(value.next_wake_ms, i18n.language) : active ? <span className="text-sm font-normal">{t('twin.today.nextOnChange')}</span> : t('twin.today.nextNone')}
          </Tile>
        ) : null}
      </dl>
    </section>
  )
}

/** 最近两次真实运行都失败时提醒；连续失败几次就写几次（读到的运行里）。 */
export function HealthAlert({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const runs = useTwin((s) => s.nodes[node.id]?.runs)
  const real = [...(runs ?? [])].filter((r) => !r.dry_run).sort((a, b) => b.started_ms - a.started_ms)
  let streak = 0
  while (streak < real.length && real[streak]!.result === 'failed') streak++
  if (streak < 2) return null
  const err = real[0]!.error
  return (
    <div role="alert" className="rounded border border-danger/50 bg-danger-subtle p-3 text-sm" data-testid="twin-health">
      <p>{t('twin.health.failing', { count: streak, error: t(`twin.errors.${err?.code ?? 'unknown'}`, { status: err?.status ?? '?' }) })}</p>
      <Link to="/n/$node/settings" params={{ node: shortNodeId(node.id, order) }} search={{ tab: 'twin' }} className="text-accent-text">
        {t('twin.health.settingsLink')}
      </Link>
    </div>
  )
}
