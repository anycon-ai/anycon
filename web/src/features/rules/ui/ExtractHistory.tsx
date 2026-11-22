// 规则页的提炼记录：只在展开期间读取并订阅，按节点隔离，请求交错时只保留最新一次结果。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { Collapsible } from 'radix-ui'
import { ChevronRight } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { ExtractRun, RuleExtractRunsResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { shortNodeId } from '@/lib/ids'
import { cleanHintText } from '@/lib/hint'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import { extractResultText, onExtractDone } from '../extract'

export function ExtractHistory({ nodeIds }: { nodeIds: readonly string[] }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const nodes = useNodes((s) => s.byId)
  const supporting = nodeIds.filter((id) => nodes[id]?.info?.capabilities.rule_extract)
  if (supporting.length === 0) return null
  return (
    <Collapsible.Root open={open} onOpenChange={setOpen} className="space-y-3" data-testid="extract-history">
      <Collapsible.Trigger className="group flex min-h-11 items-center gap-1.5 text-sm font-medium">
        <ChevronRight className="size-4 transition-transform group-data-[state=open]:rotate-90" aria-hidden />{t('rules.extract.history')}
      </Collapsible.Trigger>
      <Collapsible.Content className="space-y-4">
        {open ? supporting.map((id) => <NodeHistory key={id} nodeId={id} />) : null}
      </Collapsible.Content>
    </Collapsible.Root>
  )
}

function NodeHistory({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const node = useNodes((s) => s.byId[nodeId])
  const online = node?.conn === 'ready'
  const [result, setResult] = useState<RuleExtractRunsResult | null>(null)
  const [failed, setFailed] = useState(false)
  const [retry, setRetry] = useState(0)
  useEffect(() => {
    if (!online) return
    let alive = true
    let generation = 0
    const refresh = (): void => {
      const current = ++generation
      void rpc().request<RuleExtractRunsResult>(nodeId, 'rule.extract_runs', { limit: 20 }).then((data) => {
        if (!alive || current !== generation) return
        setResult(data)
        setFailed(false)
      }).catch(() => {
        if (alive && current === generation) setFailed(true)
      })
    }
    const off = onExtractDone((id) => { if (id === nodeId) refresh() })
    refresh()
    return () => { alive = false; off() }
  }, [nodeId, online, retry])
  return (
    <section className="space-y-2" data-testid="extract-node" data-node={nodeId}>
      <h3 className="text-sm font-medium">{cleanHintText(node?.name) || nodeId.slice(0, 8)}</h3>
      {!online ? <p className="text-sm text-fg-subtle">{t('rules.extract.offline')}</p> : null}
      {failed && online ? <div role="alert" className="text-sm"><p>{t('rules.extract.loadFailed')}</p><Button size="sm" onClick={() => setRetry((v) => v + 1)}>{t('rules.extract.retry')}</Button></div> : null}
      {result ? (
        <>
          <p className="text-sm text-fg-muted">{t('rules.extract.usage', { used: result.used_today, limit: result.daily_calls })}</p>
          {result.runs.length === 0 ? <p className="text-sm text-fg-subtle">{t('rules.extract.noHistory')}</p> : null}
          <ul className="space-y-2">
            {result.runs.map((run) => (
              <li key={run.id} className="space-y-1 rounded-md border border-border p-3 text-sm" data-testid="extract-run">
                <div className="flex flex-wrap gap-2 text-xs text-fg-muted">
                  <time dateTime={new Date(run.started_ms).toISOString()}>{new Date(run.started_ms).toLocaleString(i18n.language)}</time>
                  <RunSession nodeId={nodeId} run={run} />
                  <span>{t(run.trigger === 'manual' ? 'rules.extract.manual' : run.trigger === 'auto' ? 'rules.extract.auto' : 'rules.extract.unknownTrigger')}</span>
                  <span>{t(run.status === 'done' ? 'rules.extract.succeeded' : run.status === 'running' ? 'rules.extract.running' : 'rules.extract.failed')}</span>
                </div>
                <p>{extractResultText(run, t)}</p>
                <p className="text-xs text-fg-subtle">{t('rules.extract.counts', { added: run.added.length, dropped: run.dropped.duplicate + run.dropped.invalid + run.dropped.full })}</p>
              </li>
            ))}
          </ul>
        </>
      ) : online && !failed ? <Spinner /> : null}
    </section>
  )
}

function RunSession({ nodeId, run }: { nodeId: string; run: ExtractRun }) {
  const { t } = useTranslation()
  const session = useSessions((s) => s.list[run.session_id])
  const order = useNodes((s) => s.order)
  if (!session || session.node_id !== nodeId) return <span>{t('rules.source.sessionGone')}</span>
  return <Link to="/n/$node/s/$session" params={{ node: shortNodeId(nodeId, order), session: run.session_id }} className="text-accent-text hover:underline">{cleanHintText(session.title) || t('rules.source.session')}</Link>
}
