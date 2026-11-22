// 分身页、清单页和侧栏共用的状态与决定。模型给的理由按纯文字展示。
import { useState } from 'react'
import { Collapsible } from 'radix-ui'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TwinRun } from '@/protocol'
import { shortNodeId } from '@/lib/ids'
import { formatDateTime } from '@/lib/format'
import { describeError } from '@/lib/errors'
import { useNodes } from '@/stores/nodes'
import { useTodos } from '@/features/todos/store'
import { Button } from '@/ui/Button'
import { setTwin } from './service'
import { groupRuns, useTwin, useTwinNodeIds } from './store'

export function Decisions({ nodeId, run }: { nodeId: string; run: TwinRun }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const todos = useTodos((s) => s.nodes[nodeId]?.items)
  return (
    <div className="space-y-2">
      <p className="text-xs text-fg-subtle">{t('twin.agentWritten')}</p>
      {run.error ? <p role="alert">{t(`twin.errors.${run.error.code}`, { status: run.error.status ?? '?' })}</p> : null}
      <ul className="space-y-2">
        {run.decisions.map((d, i) => (
          <li key={`${d.todo_id}:${i}`} className="rounded border border-border p-2" data-testid="twin-decision">
            <Link to="/todos/$node/$todo" params={{ node: shortNodeId(nodeId, order), todo: d.todo_id }} className="text-accent-text">
              {todos?.[d.todo_id]?.title ?? d.todo_id}
            </Link>
            <p>{t(`twin.action.${d.action}`)}</p>
            <p className="whitespace-pre-wrap break-words">{d.reason}</p>
            {d.action === 'dispatch' && d.note ? (
              <div>
                <p className="text-xs text-fg-subtle">{t('twin.dispatchNote')}</p>
                <p className="whitespace-pre-wrap break-words">{d.note}</p>
              </div>
            ) : null}
            {(d.action === 'ask_user' || d.action === 'ask_advisor') && d.question ? <p className="whitespace-pre-wrap break-words">{d.question}</p> : null}
            {d.action === 'ask_user' && d.options?.length ? (
              <ul className="list-inside list-disc">
                {d.options.map((option, index) => <li key={index} className="whitespace-pre-wrap break-words">{option}</li>)}
              </ul>
            ) : null}
            {d.outcome === 'skipped' ? <p>{t('twin.skipped', { reason: t(`twin.skip.${d.skip_reason ?? 'unknown'}`) })}</p> : null}
            {d.rules.map((id) => (
              <Link key={id} to="/rules" search={{ node: shortNodeId(nodeId, order), rule: id }} className="mr-2 text-accent-text">
                {t('twin.rules', { rules: id })}
              </Link>
            ))}
          </li>
        ))}
      </ul>
      {run.decisions.length === 0 ? <p>{t('twin.nothing')}</p> : null}
    </div>
  )
}
export function RunHistory({ nodeId, runs, runId }: { nodeId: string; runs: TwinRun[]; runId?: string }) {
  const { t, i18n } = useTranslation()
  const lastWake = [...runs].filter((r) => !r.dry_run).sort((a, b) => b.started_ms - a.started_ms)[0]
  return (
    <section className="space-y-2">
      <h3 className="font-semibold">{t('twin.runs')}</h3>
      {groupRuns(runs).map((group) =>
        group.length > 1 ? (
          <Collapsible.Root key={group[0]!.id} defaultOpen={group.some((r) => r.id === runId)}>
            <Collapsible.Trigger asChild>
              <Button>{t('twin.idleRuns', { count: group.length })}</Button>
            </Collapsible.Trigger>
            <Collapsible.Content className="space-y-2 pl-3">
              {group.map((r) => (
                <RunLine key={r.id} nodeId={nodeId} run={r} open={r.id === runId} />
              ))}
            </Collapsible.Content>
          </Collapsible.Root>
        ) : (
          <RunLine key={group[0]!.id} nodeId={nodeId} run={group[0]!} open={group[0]!.id === runId} />
        ),
      )}
      {!runs.length ? <p className="text-sm text-fg-subtle">{t('twin.noRuns')}</p> : null}
      {lastWake ? (
        <p className="text-xs text-fg-subtle">
          {t('twin.lastWake', { time: formatDateTime(lastWake.started_ms, i18n.language), reason: lastWake.triggers.map((x) => t(`twin.trigger.${x}`)).join(', ') })}
        </p>
      ) : null}
    </section>
  )
}
function RunLine({ nodeId, run, open }: { nodeId: string; run: TwinRun; open: boolean }) {
  const { t, i18n } = useTranslation()
  return (
    <Collapsible.Root defaultOpen={open} id={`run-${run.id}`} className="rounded border border-border p-2" data-testid="twin-run">
      <Collapsible.Trigger className="min-h-11 w-full text-left text-sm">
        {formatDateTime(run.started_ms, i18n.language)} · {run.triggers.map((x) => t(`twin.trigger.${x}`)).join(', ')} · {t('twin.processed', { count: run.items.length })} ·{' '}
        {t(`twin.result.${run.result}`)} · {t('twin.runJudges', { count: run.judge_calls.length })}
        {run.dry_run ? ` · ${t('twin.dryRun')}` : ''}
      </Collapsible.Trigger>
      <Collapsible.Content>
        <Decisions nodeId={nodeId} run={run} />
      </Collapsible.Content>
    </Collapsible.Root>
  )
}
export function TwinStatusBar({ sidebar = false }: { sidebar?: boolean }) {
  const { t } = useTranslation()
  const ids = useTwinNodeIds()
  const twins = useTwin((s) => s.nodes)
  const nodes = useNodes((s) => s.byId)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  if (!ids.length) return null
  const values = ids.flatMap((id) => (twins[id]?.value ? [twins[id]!.value!] : []))
  const pause = async (): Promise<void> => {
    setBusy(true)
    setError('')
    const results = await Promise.allSettled(ids.filter((id) => nodes[id]?.conn === 'ready').map((id) => setTwin(id, { paused: true })))
    const failed = results.find((r) => r.status === 'rejected')
    if (failed?.status === 'rejected') setError(describeError(failed.reason, t))
    setBusy(false)
  }
  return (
    <div className="space-y-1 border-t border-border p-3 text-xs" data-testid="twin-status-bar">
      <Link to="/twin" className="block text-accent-text">
        {t('twin.title')} · {t('twin.summary', { working: values.reduce((n, v) => n + v.working.length, 0), waiting: values.reduce((n, v) => n + v.waiting.length, 0) })} ·{' '}
        {!values.length
          ? t('twin.loading')
          : values.every((v) => v.scope.paused)
            ? t('twin.state.paused')
            : values.some((v) => v.scope.paused)
              ? t('twin.partlyPaused')
              : values.every((v) => !v.enabled)
                ? t('twin.off')
                : t('twin.on')}
      </Link>
      {ids.some((id) => nodes[id]?.conn !== 'ready') ? <p>{t('twin.offline')}</p> : null}
      {!sidebar ? (
        <>
          <p>
            {t('twin.usage', {
              dispatches: values.reduce((n, v) => n + v.today.dispatches, 0),
              budget: values.reduce((n, v) => n + v.scope.daily_dispatches, 0),
              judges: values.reduce((n, v) => n + v.today.judge_calls, 0),
            })}
          </p>
          <Button size="sm" variant="primary" disabled={busy || !ids.some((id) => nodes[id]?.conn === 'ready')} onClick={() => void pause()}>
            {t('twin.pauseAll')}
          </Button>
        </>
      ) : null}
      {error ? <p role="alert">{error}</p> : null}
    </div>
  )
}
