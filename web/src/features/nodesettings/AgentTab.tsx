import { useEffect, useMemo, useState } from 'react'
import { RefreshCw, Save } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AgentInfo, AgentListResult } from '@/protocol'
import { AgentIssueCard } from '@/components/AgentIssueCards'
import { SettingsControls } from '@/components/SettingsControls'
import { resolveDefaults, type Selection } from '@/features/settings/resolve'
import { describeError } from '@/lib/errors'
import { warnOnError } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card, Chip, type Tone } from '@/ui/Misc'

const STATUS_TONE: Record<AgentInfo['status'], Tone> = {
  ready: 'success',
  loading: 'neutral',
  disabled: 'neutral',
  not_installed: 'warning',
  not_authenticated: 'warning',
  unsupported_version: 'warning',
  error: 'danger',
}

export function AgentTab({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const [busy, setBusy] = useState(false)
  const offline = entry?.conn !== 'ready'

  const refresh = async (): Promise<void> => {
    setBusy(true)
    try {
      const res = await rpc().request<AgentListResult>(nodeId, 'agent.list', { refresh: true }, 60_000)
      useNodes.getState().setAgents(nodeId, res.agents, res.etag)
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }

  if (!entry) return null
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <p className="text-sm text-fg-muted">{t('nodeSettings.agentsHint')}</p>
        <Button variant="outline" size="sm" icon={<RefreshCw className="size-4" />} loading={busy} disabled={offline} onClick={() => void refresh()} data-testid="agents-refresh">
          {t('nodeSettings.redetect')}
        </Button>
      </div>
      {entry.agents.length === 0 ? <p className="text-sm text-fg-subtle">{t('agent.noneDetected')}</p> : null}
      <div className="space-y-4">
        {entry.agents.map((a) => (
          <AgentCard key={a.id} nodeId={nodeId} agent={a} offline={offline} />
        ))}
      </div>
    </div>
  )
}

function AgentCard({ nodeId, agent, offline }: { nodeId: string; agent: AgentInfo; offline: boolean }) {
  const { t } = useTranslation()
  const defaults = useNodes((s) => s.byId[nodeId]?.agentDefaults[agent.id])
  const dangerousForbidden = useNodes((s) => s.byId[nodeId]?.dangerousForbidden)
  const initial = useMemo(() => resolveDefaults(agent, [defaults]), [agent, defaults])
  const [sel, setSel] = useState<Selection>(initial)
  const [busy, setBusy] = useState(false)
  useEffect(() => setSel(initial), [initial])
  const dirty = JSON.stringify(sel) !== JSON.stringify(initial)

  const save = async (): Promise<void> => {
    setBusy(true)
    const d = { model: sel.model, thinking: sel.thinking, mode: sel.mode, options: sel.options }
    try {
      await rpc().request(nodeId, 'node.set_agent_defaults', { agent: agent.id, ...d })
      useNodes.getState().setAgentDefaults(nodeId, agent.id, d)
      toast.success(t('nodeSettings.defaultsSaved'))
      // 默认值变了 → etag 也会变，顺手刷新缓存
      void rpc().request<AgentListResult>(nodeId, 'agent.list', {}).then((r) => !r.not_modified && useNodes.getState().setAgents(nodeId, r.agents, r.etag)).catch(warnOnError('refresh the agent list after saving the defaults'))
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="p-4" testId="agent-card">
      <div data-agent={agent.id} data-status={agent.status}>
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="text-base font-semibold">{agent.label}</h3>
          <Chip tone={STATUS_TONE[agent.status]} testId="agent-status">
            {t(`agent.status.${agent.status}`)}
          </Chip>
          {agent.version ? <span className="text-xs text-fg-subtle">v{agent.version}</span> : null}
          <span className="text-xs text-fg-subtle">· {agent.driver}</span>
        </div>
        {agent.path ? <p className="mt-1 break-all font-mono text-xs text-fg-subtle">{agent.path}</p> : null}
        {agent.status !== 'ready' && agent.status !== 'disabled' && agent.status !== 'loading' ? (
          <div className="mt-3">
            <AgentIssueCard agent={agent} />
          </div>
        ) : null}
        {agent.status === 'ready' ? (
          <div className="mt-4 space-y-3 border-t border-border pt-4">
            <div>
              <h4 className="text-sm font-medium">{t('nodeSettings.defaultsTitle')}</h4>
              <p className="text-xs text-fg-subtle">{t('nodeSettings.defaultsHint')}</p>
            </div>
            <SettingsControls agent={agent} value={sel} onChange={setSel} disabled={offline} dangerousForbidden={dangerousForbidden} customKey={`${nodeId}:${agent.id}`} idPrefix={`default-${agent.id}`} layout="inline" />
            <Button variant="primary" size="sm" icon={<Save className="size-4" />} loading={busy} disabled={offline || !dirty} onClick={() => void save()} data-testid="agent-defaults-save">
              {t('nodeSettings.saveDefaults')}
            </Button>
          </div>
        ) : null}
      </div>
    </Card>
  )
}
