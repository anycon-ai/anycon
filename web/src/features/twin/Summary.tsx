// Twin 页的只读概览：配置摘要、今天的数字、连续失败的提醒。
// 没有 `capabilities.twin_stats` 时，今天的唤醒次数和最近一次唤醒由已读到的运行记录推算（浏览器本地日），不显示花费和下一次唤醒。
import type { ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import { AlarmClock, Coins, History, Scale, Send, TriangleAlert, Zap, type LucideIcon } from 'lucide-react'
import type { TwinRun, TwinRunBrief, TwinRunCounts } from '@/protocol'
import { formatDateTime } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { twinHead } from './state'
import { useTwin } from './store'
import { costParts } from './usage'
import { Meter, ResultDot, SectionTitle, SplitBar, Swatch, type WakeKind } from './viz'

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

/** 今天的时间只写几点几分，更早的写日期和时间：不写「3 分钟前」，页面开着不刷新时也不会过时。 */
export function formatWhen(ts: number, lang: string, now: number = Date.now()): string {
  const sameDay = new Date(ts).toDateString() === new Date(now).toDateString()
  return sameDay ? new Date(ts).toLocaleTimeString(lang, { timeStyle: 'short' }) : formatDateTime(ts, lang)
}

/** 一块数字：标签（带图标）、大号的值（可带「/ 上限」）、一条小图、一行脚注。底色比卡片暗一档，不画边框。 */
function Kpi({ label, icon: Icon, testId, value, cap, viz, foot }: { label: string; icon: LucideIcon; testId: string; value: ReactNode; cap?: ReactNode; viz?: ReactNode; foot?: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-2 rounded-lg bg-bg-subtle p-3 sm:p-4" data-testid={testId}>
      <dt className="flex items-center gap-1.5 text-xs font-medium text-fg-muted">
        <Icon className="size-3.5 shrink-0" aria-hidden />
        {label}
      </dt>
      <dd className="flex min-w-0 items-baseline gap-1 text-2xl font-semibold tracking-tight">
        <span className="min-w-0 break-words">{value}</span>
        {cap !== undefined ? <span className="text-sm font-normal text-fg-subtle"> / {cap}</span> : null}
      </dd>
      {viz ? <dd className="pt-0.5">{viz}</dd> : null}
      {foot ? <dd className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-xs text-fg-muted">{foot}</dd> : null}
    </div>
  )
}

/** 额度块的脚注：还剩几次；用满时写明今天的额度用完了。 */
function leftText(t: TFunction, used: number, cap: number): string {
  return used >= cap ? t('twin.today.limitReached') : t('twin.today.left', { count: cap - used })
}

/** 还没有 `finished_ms` 的运行仍在进行，`result` 还不算数，写「进行中」。 */
function lastWakeText(t: TFunction, run: Pick<TwinRunBrief, 'triggers' | 'result' | 'finished_ms'>): string {
  const outcome = run.finished_ms == null ? t('twin.today.inProgress') : t(`twin.result.${run.result}`)
  return [run.triggers.map((x) => t(`twin.trigger.${x}`)).join(', '), outcome].join(' · ')
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
  const cost = costParts(t, day?.usage)
  const active = head === 'idle' || head === 'running'
  const dispatchCap = value.scope.daily_dispatches
  return (
    <section aria-labelledby={`twin-today-${node.id}`} className="space-y-2">
      <SectionTitle id={`twin-today-${node.id}`}>{t('twin.today.title')}</SectionTitle>
      <dl className="grid grid-cols-2 gap-2 lg:grid-cols-4" data-testid="twin-today">
        <Kpi
          label={t('twin.today.wakes')}
          icon={Zap}
          testId="twin-tile-wakes"
          value={total}
          viz={
            <SplitBar
              parts={[
                { kind: 'done', value: done },
                { kind: 'failed', value: failed },
                { kind: 'other', value: idle + other },
              ]}
            />
          }
          foot={
            <>
              <LegendItem kind="done">{t('twin.today.legendDone', { count: done })}</LegendItem>
              <LegendItem kind="failed">{t('twin.today.legendFailed', { count: failed })}</LegendItem>
              <LegendItem kind="other">{t('twin.today.legendIdle', { count: idle })}</LegendItem>
              {other ? (
                <LegendItem kind="other" title={t('twin.today.stoppedHint')}>
                  {t('twin.today.legendStopped', { count: other })}
                </LegendItem>
              ) : null}
            </>
          }
        />
        <Kpi label={t('twin.today.dispatches')} icon={Send} testId="twin-tile-dispatches" value={value.today.dispatches} cap={dispatchCap} viz={<Meter value={value.today.dispatches} max={dispatchCap} />} foot={leftText(t, value.today.dispatches, dispatchCap)} />
        {typeof judgeCap === 'number' ? (
          <Kpi label={t('twin.today.judgeCalls')} icon={Scale} testId="twin-tile-judges" value={value.today.judge_calls} cap={judgeCap} viz={<Meter value={value.today.judge_calls} max={judgeCap} />} foot={leftText(t, value.today.judge_calls, judgeCap)} />
        ) : (
          <Kpi label={t('twin.today.judgeCalls')} icon={Scale} testId="twin-tile-judges" value={value.today.judge_calls} />
        )}
        {cost ? <Kpi label={t('twin.today.cost')} icon={Coins} testId="twin-tile-cost" value={cost.main} foot={cost.sub} /> : null}
      </dl>
      <dl className="flex flex-wrap gap-x-6 gap-y-1.5 px-1 pt-1 text-sm">
        <div className="flex min-w-0 items-center gap-1.5" data-testid="twin-tile-last">
          <History className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />
          <dt className="shrink-0 text-fg-subtle">{t('twin.today.lastWake')}</dt>
          <dd className="flex min-w-0 items-center gap-1.5">
            {last ? (
              <>
                <time dateTime={new Date(last.started_ms).toISOString()} title={formatDateTime(last.started_ms, i18n.language)} className="shrink-0 tabular-nums">
                  {formatWhen(last.started_ms, i18n.language)}
                </time>
                <ResultDot run={last} />
                <span className="min-w-0 truncate text-fg-muted">{lastWakeText(t, last)}</span>
              </>
            ) : (
              <span className="text-fg-muted">{t('twin.today.never')}</span>
            )}
          </dd>
        </div>
        {stats ? (
          <div className="flex min-w-0 items-center gap-1.5" data-testid="twin-tile-next">
            <AlarmClock className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />
            <dt className="shrink-0 text-fg-subtle">{t('twin.today.nextWake')}</dt>
            <dd className="min-w-0 text-fg-muted">
              {value.next_wake_ms != null ? (
                <time dateTime={new Date(value.next_wake_ms).toISOString()} title={formatDateTime(value.next_wake_ms, i18n.language)} className="tabular-nums text-fg">
                  {formatWhen(value.next_wake_ms, i18n.language)}
                </time>
              ) : active ? (
                t('twin.today.nextOnChange')
              ) : (
                t('twin.today.nextNone')
              )}
            </dd>
          </div>
        ) : null}
      </dl>
    </section>
  )
}

function LegendItem({ kind, title, children }: { kind: WakeKind; title?: string; children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5 whitespace-nowrap" title={title}>
      <Swatch kind={kind} />
      {children}
    </span>
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
    <div role="alert" className="flex gap-2.5 rounded-lg border border-danger/40 bg-danger-subtle p-3 text-sm" data-testid="twin-health">
      <TriangleAlert className="mt-0.5 size-4 shrink-0 text-danger" aria-hidden />
      <div className="min-w-0 space-y-1">
      <p>{t('twin.health.failing', { count: streak, error: t(`twin.errors.${err?.code ?? 'unknown'}`, { status: err?.status ?? '?' }) })}</p>
      <Link to="/n/$node/settings" params={{ node: shortNodeId(node.id, order) }} search={{ tab: 'twin' }} className="text-accent-text">
        {t('twin.health.settingsLink')}
      </Link>
      </div>
    </div>
  )
}
