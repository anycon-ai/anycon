import { AlertCircle } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AgentInfo } from '@/protocol'
import { CopyButton, Card } from '@/ui/Misc'

/** agent 状态 → 给用户看的修复建议。优先用 daemon 给的 issue.hint，没有就用本地通用提示。 */
export function AgentIssueCard({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation()
  const hint = agent.issue?.hint
  return (
    <Card tone="warning" className="p-3" testId="agent-issue">
      <div className="flex items-start gap-2">
        <AlertCircle className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
        <div className="min-w-0 flex-1 text-sm">
          <p className="font-medium">
            {agent.label} — {t(`agent.status.${agent.status}`)}
          </p>
          {agent.issue?.message ? <p className="mt-0.5 text-fg-muted">{agent.issue.message}</p> : null}
          {hint ? (
            <div className="mt-2 flex items-center gap-2 rounded-md bg-bg-muted px-2 py-1.5">
              <code className="min-w-0 flex-1 overflow-x-auto font-mono text-xs whitespace-nowrap">{hint}</code>
              <CopyButton text={hint} />
            </div>
          ) : null}
        </div>
      </div>
    </Card>
  )
}

export function NoAgentsNotice({ agents }: { agents: AgentInfo[] }) {
  const { t } = useTranslation()
  const problems = agents.filter((a) => a.status !== 'ready' && a.status !== 'disabled')
  return (
    <div className="space-y-2" data-testid="no-agents">
      <p className="text-sm text-fg-muted">{t('agent.noneReady')}</p>
      {problems.length > 0 ? problems.map((a) => <AgentIssueCard key={a.id} agent={a} />) : <p className="text-sm text-fg-subtle">{t('agent.noneDetected')}</p>}
    </div>
  )
}
