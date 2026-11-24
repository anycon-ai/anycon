// 分身页只看运行状态：状态头和操作、今天的数字、连续失败提醒、现在在做什么、趋势、运行记录。
// 范围、预算、判断后端和分身连接在节点设置 →「分身」；记录想做的事仍然通过清单。
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TwinRun } from '@/protocol'
import { PageHeader } from '@/components/PageHeader'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { resolveNodeParam } from '@/lib/ids'
import { describeError } from '@/lib/errors'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { RunHistory, TwinStatusBar } from '@/features/twin/Parts'
import { StatusHead } from '@/features/twin/StatusHead'
import { ConfigSummary, HealthAlert, TodayTiles } from '@/features/twin/Summary'
import { NowSections } from '@/features/twin/Now'
import { Trend } from '@/features/twin/Trend'
import { syncTwin, wantTwinStats } from '@/features/twin/service'
import { useTwin, useTwinNodeIds } from '@/features/twin/store'

export function TwinPage({ node, run }: { node?: string; run?: string }) {
  const { t } = useTranslation()
  const selectedIds = useTwinNodeIds()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const target = node ? resolveNodeParam(node, order) : undefined
  const ids = node ? (target && byId[target]?.info?.capabilities.twin ? [target] : []) : selectedIds
  // 连接变化才重新读；通常初次同步已经加载，直接打开此页也可补读。
  const ready = ids.filter((id) => byId[id]?.conn === 'ready').join(',')
  useEffect(() => {
    for (const id of ready.split(',').filter(Boolean)) void syncTwin(id)
  }, [ready])
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="twin-page">
      <PageHeader root crumbs={[{ label: t('twin.title') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 space-y-4 overflow-auto p-4">
        {ids.length > 1 ? <TwinStatusBar /> : null}
        {!ids.length ? <p>{target && byId[target] ? t('twin.nodeUnsupported', { node: byId[target].name }) : t('twin.noSupport')}</p> : null}
        {ids.length && target && !selectedIds.includes(target) ? <p>{t('todos.outsideFilter', { node: byId[target]?.name ?? target })}</p> : null}
        {ids.map((id) => (
          <TwinCard key={id} node={byId[id]!} runId={run} />
        ))}
      </div>
    </div>
  )
}
function TwinCard({ node, runId }: { node: NodeEntry; runId?: string }) {
  const { t } = useTranslation()
  const entry = useTwin((s) => s.nodes[node.id])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [before, setBefore] = useState<number | null>(null)
  const [older, setOlder] = useState<TwinRun[]>([])
  const [runMissing, setRunMissing] = useState(false)
  const online = node.conn === 'ready'
  // 从「经过」直达较早的运行时，顺着游标补页直到找到；记录被清理时明确说明。
  useEffect(() => {
    if (!runId || !online || !entry?.runs) return
    if (entry.runs.some((r) => r.id === runId)) {
      setRunMissing(false)
      return
    }
    let live = true
    void (async () => {
      const pages: TwinRun[] = []
      let cursor = entry.runs.at(-1)?.started_ms
      while (cursor !== undefined && live) {
        const result = await rpc().request<{ runs: TwinRun[] }>(node.id, 'twin.runs', { before_ms: cursor, limit: 50 })
        if (!result.runs.length) break
        pages.push(...result.runs)
        if (result.runs.some((r) => r.id === runId)) break
        const next = result.runs.at(-1)!.started_ms
        if (next >= cursor) break
        cursor = next
      }
      if (live) {
        setOlder(pages)
        setRunMissing(!pages.some((r) => r.id === runId))
      }
    })().catch((e: unknown) => {
      if (live) setError(describeError(e, t))
    })
    return () => {
      live = false
    }
  }, [runId, online, entry?.runs, node.id, t])
  const value = entry?.value
  const act = async (task: () => Promise<void>): Promise<void> => {
    setBusy(true)
    setError('')
    try {
      await task()
    } catch (e) {
      setError(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  const configured = !!value?.configured
  // 卡片挂着就说明有人在看统计：今天的数字和趋势都用它
  useEffect(() => wantTwinStats(node.id), [node.id])
  return (
    <article className="space-y-4 rounded-lg border border-border bg-surface p-4" data-testid="twin-card">
      <StatusHead node={node} onError={setError} />
      {/* 没配置好时只留状态头和「去设置」；有运行记录才显示历史 */}
      {configured ? (
        <>
          <ConfigSummary node={node} />
          <TodayTiles node={node} />
          <HealthAlert node={node} />
          <NowSections node={node} />
          {node.info?.capabilities.twin_stats === true ? <Trend nodeId={node.id} /> : null}
        </>
      ) : null}
      {runMissing ? <p role="status">{t('twin.runGone')}</p> : null}
      {entry?.runs.length || configured || runId ? <RunHistory key={runId} nodeId={node.id} runs={[...(entry?.runs ?? []), ...older].filter((r, i, all) => all.findIndex((x) => x.id === r.id) === i)} runId={runId} /> : null}
      {(entry?.runs.length ?? 0) >= 50 && before !== 0 ? (
        <Button
          disabled={!online || busy}
          onClick={() =>
            void act(async () => {
              const ms = older.at(-1)?.started_ms ?? entry!.runs.at(-1)!.started_ms
              const result = await rpc().request<{ runs: TwinRun[] }>(node.id, 'twin.runs', { before_ms: ms, limit: 50 })
              setOlder((old) => [...old, ...result.runs])
              setBefore(result.runs.length ? ms : 0)
            })
          }
        >
          {t('twin.older')}
        </Button>
      ) : null}
      {error ? (
        <p role="alert" className="text-danger">
          {error}
        </p>
      ) : null}
    </article>
  )
}
