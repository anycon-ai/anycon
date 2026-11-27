// Bot 页「趋势」：最近 7 / 30 天。上面是每天唤醒次数的堆叠柱状图（按结果分：动手、失败、其余）和一行读数（悬停 / 方向键选中的那天，缺省是今天），
// 下面是区间汇总（Bot 做了什么、结果怎样），逐日表收在「按天的明细」里（图的无障碍等价物，图上的每个数字表里都有）。
// 只有 `capabilities.twin_stats` 的节点才有。数字来自节点保留的运行记录和待办经过，是事实计数，不做判断。
// 图是普通的 div，不引入图表库；配色和规则见 viz.tsx 的文件头。
import { useId, useState, type KeyboardEvent, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import { ChevronRight } from 'lucide-react'
import type { TwinDayStats } from '@/protocol'
import { formatDuration } from '@/lib/format'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Segmented } from '@/ui/Misc'
import { retryTwinStats, setTwinStatsRange } from './service'
import { useTwin } from './store'
import { costParts, sumUsage, usageText } from './usage'
import { SectionTitle, Swatch, WAKE_FILL, WAKE_KINDS, type WakeKind } from './viz'

const n = (v: number | undefined): number => v ?? 0
const wakes = (d: TwinDayStats) => {
  const r = d.runs
  const done = n(r?.done)
  const idle = n(r?.idle)
  const failed = n(r?.failed)
  const stopped = n(r?.cancelled) + n(r?.budget)
  return { done, idle, failed, stopped, other: idle + stopped, total: done + idle + failed + stopped }
}

export function summarize(days: readonly TwinDayStats[]) {
  const sum = (f: (d: TwinDayStats) => number): number => days.reduce((a, d) => a + f(d), 0)
  const accepted = sum((d) => n(d.accepted))
  return {
    wakes: sum((d) => wakes(d).total),
    dispatches: sum((d) => n(d.decisions?.dispatch)),
    questions: sum((d) => n(d.decisions?.ask_user)),
    handedBack: sum((d) => n(d.handed_back)),
    takenOver: sum((d) => n(d.taken_over)),
    accepted,
    dropped: sum((d) => n(d.dropped)),
    turnaroundMs: accepted ? sum((d) => n(d.turnaround_ms)) / accepted : null,
    usage: sumUsage(days.map((d) => d.usage)),
  }
}

/** 纵轴的上限：取「1、2、5 × 10 的幂」里不小于最大值一半的那个，再乘 2，所以中间那条刻度也是整数。全是 0 时给 2。 */
export function niceMax(max: number): number {
  if (max <= 0) return 2
  const half = Math.ceil(max / 2)
  const pow = 10 ** Math.floor(Math.log10(half))
  const step = [1, 2, 5, 10].map((m) => m * pow).find((s) => s >= half)!
  return step * 2
}

export function Trend({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const days = useTwin((s) => s.nodes[nodeId]?.statsDays ?? 7)
  const stats = useTwin((s) => s.nodes[nodeId]?.stats)
  const failed = useTwin((s) => s.nodes[nodeId]?.statsFailed === true)
  const loaded = stats && stats.length === days ? stats : undefined
  const titleId = useId()
  return (
    <section className="space-y-3" aria-labelledby={titleId} data-testid="twin-trend">
      <SectionTitle
        id={titleId}
        aside={
          <Segmented
            size="sm"
            value={String(days) as '7' | '30'}
            onChange={(v) => void setTwinStatsRange(nodeId, Number(v))}
            ariaLabel={t('twin.trend.range')}
            options={[
              { value: '7', label: t('twin.trend.days7'), testId: 'twin-range-7' },
              { value: '30', label: t('twin.trend.days30'), testId: 'twin-range-30' },
            ]}
          />
        }
      >
        {t('twin.trend.title')}
      </SectionTitle>
      {!loaded && failed ? (
        <div role="alert" className="flex flex-wrap items-center gap-2 text-sm text-danger">
          {t('twin.trend.failed')}
          <Button onClick={() => void retryTwinStats(nodeId)}>{t('todos.retry')}</Button>
        </div>
      ) : !loaded ? (
        <p role="status" className="text-sm text-fg-subtle">
          {t('twin.trend.loading')}
        </p>
      ) : (
        <>
          <WakeChart days={loaded} />
          <TrendSummary days={loaded} />
          <DayTable days={loaded} />
          <p className="text-xs leading-relaxed text-fg-subtle">{t('twin.trend.note')}</p>
        </>
      )}
    </section>
  )
}

// ---- 图 ----

/** 某一天的读数（悬停 / 键盘选中时的浮层，也给读屏的那一句用）。 */
function dayFacts(t: TFunction, d: TwinDayStats) {
  const w = wakes(d)
  return {
    w,
    rest: [
      t('twin.trend.tipDispatches', { count: n(d.decisions?.dispatch) }),
      t('twin.trend.tipAccepted', { count: n(d.accepted) }),
      ...(usageText(t, d.usage) ? [usageText(t, d.usage)!] : []),
    ],
  }
}

function WakeChart({ days }: { days: readonly TwinDayStats[] }) {
  const { t } = useTranslation()
  const [active, setActive] = useState<number | null>(null)
  const count = days.length
  const rows = days.map(wakes)
  const max = niceMax(Math.max(0, ...rows.map((r) => r.total)))
  const empty = rows.every((r) => r.total === 0)
  // 日期隔几天标一次（7 天每天、30 天每 5 天；窄屏再隔一倍），从今天往回数，保证今天总有标签
  const every = count > 10 ? 5 : 1
  const tick = (i: number): { text: string; wide: boolean } | null => {
    const back = count - 1 - i
    if (back % every !== 0) return null
    return { text: i === count - 1 ? t('twin.trend.today') : days[i]!.date.slice(5), wide: back % (every * 2) !== 0 }
  }
  const onKey = (e: KeyboardEvent): void => {
    const cur = active ?? count - 1
    const next = e.key === 'ArrowLeft' ? cur - 1 : e.key === 'ArrowRight' ? cur + 1 : e.key === 'Home' ? 0 : e.key === 'End' ? count - 1 : null
    if (next === null) return
    e.preventDefault()
    setActive(Math.max(0, Math.min(count - 1, next)))
  }
  const legend: Record<WakeKind, string> = { done: t('twin.trend.legendDone'), failed: t('twin.trend.legendFailed'), other: t('twin.trend.legendOther') }
  const shown = active ?? count - 1
  const facts = dayFacts(t, days[shown]!)
  return (
    <figure className="rounded-lg bg-bg-subtle p-3 sm:p-4" data-testid="twin-trend-chart">
      <figcaption className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-xs">
        <span className="font-medium text-fg-muted">{t('twin.trend.chartTitle')}</span>
        <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-fg-muted">
          {WAKE_KINDS.map((k) => (
            <span key={k} className="inline-flex items-center gap-1.5">
              <Swatch kind={k} />
              {legend[k]}
            </span>
          ))}
        </span>
      </figcaption>
      <DayReadout date={days[shown]!.date} isToday={shown === count - 1} facts={facts} legend={legend} />
      <div className="mt-3 flex gap-2">
        {/* 纵轴：0、一半、上限三条刻度 */}
        <div className="relative h-36 w-6 shrink-0 text-right text-xs tabular-nums text-fg-subtle" aria-hidden>
          {[0, 0.5, 1].map((f) => (
            <span key={f} className="absolute right-0 translate-y-1/2 leading-none" style={{ bottom: `${f * 100}%` }}>
              {max * f}
            </span>
          ))}
        </div>
        <div className="min-w-0 flex-1">
          <div
            className="relative h-36 rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-4 focus-visible:ring-offset-bg-subtle"
            tabIndex={0}
            role="group"
            aria-label={t('twin.trend.chartLabel')}
            onKeyDown={onKey}
            onFocus={() => setActive((a) => a ?? count - 1)}
            onBlur={() => setActive(null)}
            onPointerLeave={(e) => {
              if (document.activeElement !== e.currentTarget) setActive(null)
            }}
          >
            {/* 横向网格线：实线、细、比底色深一档 */}
            {[0, 0.5, 1].map((f) => (
              <div key={f} className="absolute inset-x-0 border-t border-border" style={{ bottom: `${f * 100}%` }} aria-hidden />
            ))}
            {empty ? <p className="absolute inset-0 grid place-items-center text-xs text-fg-subtle">{t('twin.trend.noWakes')}</p> : null}
            <div className="absolute inset-0 flex items-end" aria-hidden>
              {rows.map((r, i) => (
                <div key={days[i]!.date} className="relative flex h-full flex-1 items-end justify-center" onPointerEnter={() => setActive(i)} onPointerDown={() => setActive(i)} data-testid="twin-chart-day">
                  {active === i ? <div className="absolute inset-y-0 inset-x-[8%] rounded-sm bg-bg-hover" /> : null}
                  {r.total > 0 ? (
                    <div className="relative flex w-[60%] max-w-6 flex-col-reverse gap-0.5 overflow-hidden rounded-t-[4px]" style={{ height: `${(r.total / max) * 100}%` }}>
                      {WAKE_KINDS.map((k) => (r[k] > 0 ? <div key={k} className={WAKE_FILL[k]} style={{ flexGrow: r[k], flexBasis: 0 }} /> : null))}
                    </div>
                  ) : null}
                </div>
              ))}
            </div>
          </div>
          {/* 横轴日期 */}
          <div className="mt-1.5 flex text-xs tabular-nums text-fg-subtle" aria-hidden>
            {days.map((d, i) => {
              const l = tick(i)
              return (
                <span key={d.date} className="flex-1 whitespace-nowrap text-center">
                  {l ? <span className={l.wide ? 'hidden sm:inline' : undefined}>{l.text}</span> : null}
                </span>
              )
            })}
          </div>
        </div>
      </div>
      {/* 键盘逐天查看时给读屏念一句；鼠标悬停不念 */}
      <p className="sr-only" aria-live="polite">
        {active !== null ? `${days[active]!.date}: ${t('twin.trend.tipWakes', { count: facts.w.total })}, ${legend.done} ${facts.w.done}, ${legend.failed} ${facts.w.failed}, ${legend.other} ${facts.w.other}; ${facts.rest.join(', ')}` : ''}
      </p>
    </figure>
  )
}

/** 图上方的读数行：一天的唤醒总数、按结果分的次数（带色块）、派发、验收和花费。没选中哪天时是今天（最后一天）。
 *  放在图外面而不是浮在柱子上：不挡柱子和坐标，手机上也放得下。 */
function DayReadout({ date, isToday, facts, legend }: { date: string; isToday: boolean; facts: ReturnType<typeof dayFacts>; legend: Record<WakeKind, string> }) {
  const { t } = useTranslation()
  return (
    <p className="mt-2 flex min-h-10 flex-wrap items-baseline gap-x-3 gap-y-0.5 text-xs text-fg-muted sm:min-h-6" data-testid="twin-chart-readout">
      <span className="tabular-nums text-fg-subtle">{isToday ? `${t('twin.trend.today')} · ${date}` : date}</span>
      <span className="text-sm font-semibold text-fg">{t('twin.trend.tipWakes', { count: facts.w.total })}</span>
      {WAKE_KINDS.map((k) => (
        <span key={k} className="inline-flex items-center gap-1.5 whitespace-nowrap">
          <Swatch kind={k} />
          <span className="font-medium tabular-nums text-fg">{facts.w[k]}</span>
          {legend[k]}
        </span>
      ))}
      {/* 窄屏只留唤醒的分项，派发、验收和花费在「按天的明细」里 */}
      {facts.rest.map((x) => (
        <span key={x} className="hidden whitespace-nowrap sm:inline">
          {x}
        </span>
      ))}
    </p>
  )
}

// ---- 汇总 ----

function Stat({ id, label, children, zero, sub }: { id: string; label: string; children: ReactNode; zero?: boolean; sub?: ReactNode }) {
  return (
    <div className="min-w-0" data-testid={`twin-trend-${id}`}>
      <dt className="text-xs text-fg-muted">{label}</dt>
      <dd className={cn('mt-0.5 text-lg font-semibold tracking-tight', zero && 'text-fg-subtle')}>{children}</dd>
      {sub ? <dd className="text-xs text-fg-muted">{sub}</dd> : null}
    </div>
  )
}

function TrendSummary({ days }: { days: readonly TwinDayStats[] }) {
  const { t } = useTranslation()
  const sum = summarize(days)
  const cost = costParts(t, sum.usage)
  const group = 'rounded-lg border border-border p-3 sm:p-4'
  const heading = 'mb-3 text-xs font-medium text-fg-muted'
  return (
    <div role="group" aria-label={t('twin.trend.summary')} className="grid gap-2 lg:grid-cols-[2fr_3fr]" data-testid="twin-trend-summary">
      <div className={group}>
        <p className={heading}>{t('twin.trend.groupActivity')}</p>
        <dl className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-4 lg:grid-cols-2 xl:grid-cols-4">
          <Stat id="wakes" label={t('twin.trend.wakes')} zero={!sum.wakes}>{sum.wakes}</Stat>
          <Stat id="dispatches" label={t('twin.trend.dispatches')} zero={!sum.dispatches}>{sum.dispatches}</Stat>
          <Stat id="questions" label={t('twin.trend.questions')} zero={!sum.questions}>{sum.questions}</Stat>
          {cost ? (
            <Stat id="cost" label={t('twin.trend.cost')} sub={cost.sub}>
              {cost.main}
            </Stat>
          ) : null}
        </dl>
      </div>
      <div className={group}>
        <p className={heading}>{t('twin.trend.groupOutcome')}</p>
        <dl className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-5 lg:grid-cols-3 xl:grid-cols-5">
          <Stat id="accepted" label={t('twin.trend.accepted')} zero={!sum.accepted}>{sum.accepted}</Stat>
          <Stat id="dropped" label={t('twin.trend.dropped')} zero={!sum.dropped}>{sum.dropped}</Stat>
          <Stat id="handed-back" label={t('twin.trend.handedBack')} zero={!sum.handedBack}>{sum.handedBack}</Stat>
          <Stat id="taken-over" label={t('twin.trend.takenOver')} zero={!sum.takenOver}>{sum.takenOver}</Stat>
          <Stat id="turnaround" label={t('twin.trend.turnaround')} zero={sum.turnaroundMs === null}>
            {sum.turnaroundMs === null ? t('twin.trend.none') : formatDuration(sum.turnaroundMs)}
          </Stat>
        </dl>
      </div>
    </div>
  )
}

// ---- 逐日表 ----

function Num({ v }: { v: number }) {
  return <td className={cn('py-1.5 pl-3 text-right tabular-nums', v === 0 && 'text-fg-subtle')}>{v}</td>
}

function DayTable({ days }: { days: readonly TwinDayStats[] }) {
  const { t } = useTranslation()
  const th = 'py-1.5 pl-3 text-right text-xs font-medium text-fg-subtle'
  return (
    <details className="group rounded-lg border border-border">
      <summary className="flex min-h-10 cursor-pointer list-none items-center gap-1.5 px-3 text-sm text-fg-muted hover:text-fg [&::-webkit-details-marker]:hidden">
        <ChevronRight className="size-4 shrink-0 transition-transform group-open:rotate-90" aria-hidden />
        {t('twin.trend.showTable')}
      </summary>
      <div className="scroll-thin overflow-x-auto border-t border-border px-3 pb-2">
        <table className="w-full min-w-[40rem] text-sm" data-testid="twin-trend-table">
          <caption className="sr-only">{t('twin.trend.caption')}</caption>
          <thead>
            <tr className="border-b border-border">
              <th scope="col" className="py-1.5 pr-3 text-left text-xs font-medium text-fg-subtle">{t('twin.trend.date')}</th>
              <th scope="col" className={th}>{t('twin.trend.wakes')}</th>
              <th scope="col" className={th}>{t('twin.trend.dispatches')}</th>
              <th scope="col" className={th}>{t('twin.trend.questions')}</th>
              <th scope="col" className={th}>{t('twin.trend.handedBack')}</th>
              <th scope="col" className={th}>{t('twin.trend.takenOver')}</th>
              <th scope="col" className={th}>{t('twin.trend.accepted')}</th>
              <th scope="col" className={th}>{t('twin.trend.cost')}</th>
            </tr>
          </thead>
          <tbody>
            {[...days].reverse().map((d) => {
              const w = wakes(d)
              const cost = usageText(t, d.usage)
              return (
                <tr key={d.date} className={cn('border-b border-border/50 last:border-0', !w.total && 'text-fg-subtle')} data-testid="twin-trend-row">
                  <th scope="row" className="whitespace-nowrap py-1.5 pr-3 text-left font-normal tabular-nums">{d.date}</th>
                  <td className="whitespace-nowrap py-1.5 pl-3 text-right tabular-nums">
                    {w.total ? (
                      <>
                        {w.total} <span className="text-xs text-fg-subtle">({t('twin.trend.wakesCell', { done: w.done, idle: w.idle, failed: w.failed })})</span>
                      </>
                    ) : (
                      0
                    )}
                  </td>
                  <Num v={n(d.decisions?.dispatch)} />
                  <Num v={n(d.decisions?.ask_user)} />
                  <Num v={n(d.handed_back)} />
                  <Num v={n(d.taken_over)} />
                  <Num v={n(d.accepted)} />
                  <td className={cn('whitespace-nowrap py-1.5 pl-3 text-right tabular-nums', !cost && 'text-fg-subtle')}>{cost ?? t('twin.trend.none')}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
    </details>
  )
}
