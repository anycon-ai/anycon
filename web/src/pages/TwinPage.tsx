// 分身是监督和配置页；记录想做的事仍然通过清单。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { NodeConfigResult, TwinGetResult, TwinRun, TwinSetParams } from '@/protocol'
import { PageHeader } from '@/components/PageHeader'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { resolveNodeParam, shortNodeId } from '@/lib/ids'
import { describeError } from '@/lib/errors'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { Input, Select } from '@/ui/Field'
import { Decisions, RunHistory, TwinStatusBar } from '@/features/twin/Parts'
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
        {!ids.length ? <p>{t('twin.noSupport')}</p> : null}
        {target && !selectedIds.includes(target) ? <p>{t('inbox.outsideFilter', { node: byId[target]?.name ?? target })}</p> : null}
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
function ScopeForm({ node, value, busy, onSave }: { node: NodeEntry; value: TwinGetResult; busy: boolean; onSave: (params: TwinSetParams) => Promise<void> }) {
  const { t } = useTranslation()
  const [projects, setProjects] = useState(value.scope.projects)
  const [agent, setAgent] = useState(value.scope.agent ?? '')
  const [model, setModel] = useState(value.scope.model ?? '')
  const [mode, setMode] = useState(value.scope.mode ?? '')
  const [max, setMax] = useState(value.scope.max_sessions)
  const [daily, setDaily] = useState(value.scope.daily_dispatches)
  const [cap, setCap] = useState<number | null>(null)
  const [capFailed, setCapFailed] = useState(false)
  const [retry, setRetry] = useState(0)
  const ready = node.conn === 'ready'
  useEffect(() => {
    if (!ready) return
    let live = true
    void rpc()
      .request<NodeConfigResult>(node.id, 'node.get_config', {})
      .then(
        (r) => {
          if (!live) return
          const n = r.items.find((x) => x.key === 'sessions.max_concurrent')?.value
          if (typeof n === 'number') {
            setCap(Math.max(0, n - 1))
            setCapFailed(false)
          } else setCapFailed(true)
        },
        () => {
          if (live) setCapFailed(true)
        },
      )
    return () => {
      live = false
    }
  }, [node.id, ready, retry])
  const selected = node.agents.find((a) => a.id === agent)
  const modes = (selected?.modes ?? []).filter((m) => m.risk !== 'dangerous')
  const unattended = modes.find((m) => m.unattended)
  const valid = Number.isInteger(max) && max >= 1 && cap !== null && max <= cap && Number.isInteger(daily) && daily >= 0 && (!mode || modes.some((m) => m.id === mode))
  return (
    <form
      id={`scope-${node.id}`}
      className="space-y-3"
      onSubmit={(e) => {
        e.preventDefault()
        if (valid)
          void onSave({
            projects,
            ...(agent ? { agent } : {}),
            ...(model ? { model } : {}),
            ...(mode ? { mode } : {}),
            clear: [...(!agent ? ['agent' as const] : []), ...(!model ? ['model' as const] : []), ...(!mode ? ['mode' as const] : [])],
            max_sessions: max,
            daily_dispatches: daily,
          })
      }}
    >
      <h3 className="font-semibold">{t('twin.scope')}</h3>
      <fieldset disabled={!ready || busy} className="space-y-3">
        <legend className="text-sm">{t('twin.projects')}</legend>
        {[...new Set([...node.projects.map((p) => p.name), ...projects])].map((p) => (
          <label key={p} className="mr-4 inline-flex min-h-11 items-center gap-2">
            <input type="checkbox" checked={projects.includes(p)} onChange={(e) => setProjects((cur) => (e.target.checked ? [...cur, p] : cur.filter((x) => x !== p)))} />
            {p}
          </label>
        ))}
        <label className="block">
          {t('twin.agent')}
          <Select
            value={agent}
            onChange={(e) => {
              setAgent(e.target.value)
              setModel('')
              setMode('')
            }}
          >
            <option value="">{t('twin.default')}</option>
            {node.agents
              .filter((a) => a.enabled)
              .map((a) => (
                <option key={a.id} value={a.id}>
                  {a.label}
                </option>
              ))}
          </Select>
        </label>
        {!selected ? <p className="text-xs text-fg-subtle">{t('twin.chooseAgent')}</p> : null}
        <label className="block">
          {t('twin.model')}
          <Input disabled={!selected} list={`models-${node.id}`} value={model} onChange={(e) => setModel(e.target.value)} placeholder={t('twin.default')} />
          <datalist id={`models-${node.id}`}>
            {selected?.models?.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
              </option>
            ))}
          </datalist>
        </label>
        <label className="block">
          {t('twin.mode')}
          <Select disabled={!selected} value={mode} onChange={(e) => setMode(e.target.value)}>
            <option value="">
              {t('twin.default')}
              {unattended ? ` · ${unattended.label} (${t('twin.unattended')})` : ''}
            </option>
            {modes.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
                {m.unattended ? ` · ${t('twin.unattended')}` : ''}
              </option>
            ))}
          </Select>
        </label>
        <label className="block">
          {t('twin.maxSessions')}
          <Input type="number" min={1} max={cap ?? undefined} value={max} onChange={(e) => setMax(Number(e.target.value))} />
        </label>
        <p className="text-xs text-fg-subtle">{cap === null ? t('twin.capacityLoading') : cap < 1 ? t('twin.capacityZero') : t('twin.capacity', { max: cap })}</p>
        {capFailed ? (
          <p role="alert">
            {t('twin.capacityFailed')} <Button onClick={() => setRetry((n) => n + 1)}>{t('todos.retry')}</Button>
          </p>
        ) : null}
        <label className="block">
          {t('twin.dailyDispatches')}
          <Input type="number" min={0} value={daily} onChange={(e) => setDaily(Number(e.target.value))} />
        </label>
        <Button type="submit" disabled={!valid}>
          {t('twin.save')}
        </Button>
      </fieldset>
    </form>
  )
}
