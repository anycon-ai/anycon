// 分身是监督和配置页；记录想做的事仍然通过清单。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TwinRun } from '@/protocol'
import { PageHeader } from '@/components/PageHeader'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { resolveNodeParam, shortNodeId } from '@/lib/ids'
import { describeError } from '@/lib/errors'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { Decisions, RunHistory, TwinStatusBar } from '@/features/twin/Parts'
import { ScopeForm } from '@/features/twin/ScopeForm'
import { TwinLinks } from '@/features/twin/TwinLinks'
import { dryRunTwin, setTwin, syncTwin, wakeTwin } from '@/features/twin/service'
import { useTwin, useTwinNodeIds } from '@/features/twin/store'
import { useTodos } from '@/features/todos/store'

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
  const todos = useTodos((s) => s.nodes[node.id]?.items)
  const order = useNodes((s) => s.order)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [preview, setPreview] = useState<TwinRun | null>(null)
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
  const triage = node.info?.judge?.backends.find((b) => b.name === node.info?.judge?.triage)
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
  const trial = async (): Promise<void> => {
    setPreview((await dryRunTwin(node.id)).run)
  }
  const unpause = async (): Promise<void> => {
    await trial()
  }
  const settings = (
    <Link to="/n/$node/settings" params={{ node: shortNodeId(node.id, order) }} search={{ tab: 'limits' }} className="text-accent-text">
      {t('twin.judgeSettings')}
    </Link>
  )
  return (
    <article className="space-y-4 rounded-lg border border-border bg-surface p-4" data-testid="twin-card">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="font-semibold">{node.name}</h2>
          <p data-testid="twin-state">{!online ? t('twin.offline') : value ? t(`twin.state.${value.state}`) : t('twin.loading')}</p>
        </div>
        <Button variant="primary" size="lg" disabled={!online || !value || busy || value.scope.paused} onClick={() => void act(() => setTwin(node.id, { paused: true }))}>
          {t('twin.pause')}
        </Button>
      </div>
      {entry?.failed ? (
        <div role="alert">
          {t('twin.loadFailed')}{' '}
          <Button disabled={!online || busy} onClick={() => void act(() => syncTwin(node.id))}>
            {t('todos.retry')}
          </Button>
        </div>
      ) : null}
      {value ? (
        <>
          {!value.enabled ? <p>{t('twin.machineDisabled')}</p> : null}
          {!value.configured ? (
            <div>
              <p>{t('twin.notConfigured')}</p>
              {!value.scope.projects.length ? <p>{t('twin.noProjects')}</p> : null}
              {!triage || triage.problem ? <p>{t('twin.noTriage')}</p> : null}
              {settings}
              <a href={`#scope-${node.id}`} className="ml-3 text-accent-text">
                {t('twin.scope')}
              </a>
            </div>
          ) : null}
          <p>{t('twin.usage', { dispatches: value.today.dispatches, budget: value.scope.daily_dispatches, judges: value.today.judge_calls })}</p>
          {(['working', 'waiting'] as const).map((kind) => (
            <section key={kind}>
              <h3 className="text-sm font-semibold">
                {t(`twin.${kind}`)} ({value[kind].length})
              </h3>
              <ul>
                {value[kind].map((id) => (
                  <li key={id}>
                    <Link to="/todos/$node/$todo" params={{ node: shortNodeId(node.id, order), todo: id }} className="text-accent-text">
                      {todos?.[id]?.title ?? id}
                    </Link>
                  </li>
                ))}
              </ul>
            </section>
          ))}
          <section className="space-y-2">
            <h3 className="font-semibold">{t('twin.what')}</h3>
            <p className="text-sm text-fg-subtle">{t('twin.trialHint')}</p>
            <Button disabled={!online || busy || !value.enabled || !value.configured} onClick={() => void act(value.scope.paused ? unpause : trial)}>
              {t('twin.dryRun')}
            </Button>
            {preview ? (
              <>
                <Decisions nodeId={node.id} run={preview} />
                <Button
                  disabled={!online || busy || !value.enabled || !value.configured || !['done', 'idle'].includes(preview.result)}
                  onClick={() =>
                    void act(async () => {
                      await setTwin(node.id, { paused: false })
                      await wakeTwin(node.id)
                      setPreview(null)
                    })
                  }
                >
                  {t('twin.doIt')}
                </Button>
              </>
            ) : null}
          </section>
          <ScopeForm
            key={`${node.id}:${JSON.stringify(value.scope)}`}
            node={node}
            value={value}
            busy={busy}
            onSave={(params) =>
              act(async () => {
                setPreview(null)
                const first = !value.configured
                await setTwin(node.id, { ...params, ...(first ? { paused: true } : {}) })
                if (first && params.projects?.length && value.enabled) await trial()
              })
            }
          />
          {settings}
        </>
      ) : null}
      <TwinLinks node={node} />
      {runMissing ? <p role="status">{t('twin.runGone')}</p> : null}
      <RunHistory key={runId} nodeId={node.id} runs={[...(entry?.runs ?? []), ...older].filter((r, i, all) => all.findIndex((x) => x.id === r.id) === i)} runId={runId} />
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
