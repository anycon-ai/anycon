// Bot 页「趋势」：最近 7 / 30 天的汇总和逐日表，只有数字，不画图。只有 `capabilities.twin_stats` 的节点才有。
// 数字来自节点保留的运行记录和待办经过，是事实计数，不做判断。
import { useTranslation } from 'react-i18next'
import type { TwinDayStats } from '@/protocol'
import { formatDuration } from '@/lib/format'
import { Button } from '@/ui/Button'
import { Segmented } from '@/ui/Misc'
import { retryTwinStats, setTwinStatsRange } from './service'
import { useTwin } from './store'
import { sumUsage, usageText } from './usage'

const n = (v: number | undefined): number => v ?? 0
const wakes = (d: TwinDayStats) => {
  const r = d.runs
  return { done: n(r?.done), idle: n(r?.idle), failed: n(r?.failed), total: n(r?.done) + n(r?.idle) + n(r?.failed) + n(r?.cancelled) + n(r?.budget) }
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

export function Trend({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const days = useTwin((s) => s.nodes[nodeId]?.statsDays ?? 7)
  const stats = useTwin((s) => s.nodes[nodeId]?.stats)
  const failed = useTwin((s) => s.nodes[nodeId]?.statsFailed === true)
  const loaded = stats && stats.length === days ? stats : undefined
  const sum = loaded ? summarize(loaded) : null
  const cost = sum ? usageText(t, sum.usage) : null
  const items: Array<[string, string, string]> = sum
    ? [
        ['wakes', t('twin.trend.wakes'), String(sum.wakes)],
        ['dispatches', t('twin.trend.dispatches'), String(sum.dispatches)],
        ['questions', t('twin.trend.questions'), String(sum.questions)],
        ['handed-back', t('twin.trend.handedBack'), String(sum.handedBack)],
        ['taken-over', t('twin.trend.takenOver'), String(sum.takenOver)],
        ['accepted', t('twin.trend.accepted'), String(sum.accepted)],
        ['dropped', t('twin.trend.dropped'), String(sum.dropped)],
        ['turnaround', t('twin.trend.turnaround'), sum.turnaroundMs === null ? t('twin.trend.none') : formatDuration(sum.turnaroundMs)],
        ...(cost ? [['cost', t('twin.trend.cost'), cost] as [string, string, string]] : []),
      ]
    : []
  return (
    <section className="space-y-3" aria-label={t('twin.trend.title')} data-testid="twin-trend">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-sm font-semibold">{t('twin.trend.title')}</h3>
        <Segmented
          value={String(days) as '7' | '30'}
          onChange={(v) => void setTwinStatsRange(nodeId, Number(v))}
          ariaLabel={t('twin.trend.range')}
          options={[
            { value: '7', label: t('twin.trend.days7'), testId: 'twin-range-7' },
            { value: '30', label: t('twin.trend.days30'), testId: 'twin-range-30' },
          ]}
        />
      </div>
      {!loaded && failed ? (
        <div role="alert" className="flex flex-wrap items-center gap-2 text-sm text-danger">
          {t('twin.trend.failed')}
          <Button onClick={() => void retryTwinStats(nodeId)}>
            {t('todos.retry')}
          </Button>
        </div>
      ) : !loaded ? (
        <p role="status" className="text-sm text-fg-subtle">
          {t('twin.trend.loading')}
        </p>
      ) : (
        <>
          <dl aria-label={t('twin.trend.summary')} className="grid grid-cols-2 gap-2 lg:grid-cols-3" data-testid="twin-trend-summary">
            {items.map(([key, label, text]) => (
              <div key={key} className="rounded border border-border p-2" data-testid={`twin-trend-${key}`}>
                <dt className="text-xs text-fg-subtle">{label}</dt>
                <dd className="font-semibold">{text}</dd>
              </div>
            ))}
          </dl>
          <p className="text-xs text-fg-subtle">{t('twin.trend.note')}</p>
          <div className="scroll-thin overflow-x-auto">
            <table className="w-full min-w-[40rem] text-left text-sm" data-testid="twin-trend-table">
              <caption className="mb-1 text-left text-xs text-fg-subtle">{t('twin.trend.caption')}</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.date')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.wakes')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.dispatches')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.questions')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.handedBack')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.takenOver')}</th>
                  <th scope="col" className="py-1 pr-3 font-medium">{t('twin.trend.accepted')}</th>
                  <th scope="col" className="py-1 font-medium">{t('twin.trend.cost')}</th>
                </tr>
              </thead>
              <tbody>
                {[...loaded].reverse().map((d) => {
                  const w = wakes(d)
                  return (
                    <tr key={d.date} className="border-b border-border/50" data-testid="twin-trend-row">
                      <th scope="row" className="whitespace-nowrap py-1 pr-3 font-normal">{d.date}</th>
                      <td className="py-1 pr-3">{w.total ? `${w.total} (${t('twin.trend.wakesCell', { done: w.done, idle: w.idle, failed: w.failed })})` : 0}</td>
                      <td className="py-1 pr-3">{n(d.decisions?.dispatch)}</td>
                      <td className="py-1 pr-3">{n(d.decisions?.ask_user)}</td>
                      <td className="py-1 pr-3">{n(d.handed_back)}</td>
                      <td className="py-1 pr-3">{n(d.taken_over)}</td>
                      <td className="py-1 pr-3">{n(d.accepted)}</td>
                      <td className="py-1">{usageText(t, d.usage) ?? t('twin.trend.none')}</td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        </>
      )}
    </section>
  )
}
