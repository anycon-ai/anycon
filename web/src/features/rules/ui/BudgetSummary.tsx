// 「已用 / 上限」（`rule.list` 的 `budget`，docs/spec/daemon.md § 23.4）：一个会话拿到的规则文字（含模板）最多 `cap` 字节。
// 全节点规则一行；每个有项目规则的项目一行，是这个项目的会话实际拿到的字节数（`node_bytes + projects[项目]`）。
import { useTranslation } from 'react-i18next'
import type { RuleBudget } from '@/protocol'
import { cleanHintText } from '@/lib/hint'
import { cn } from '@/ui/cn'

function Row({ label, used, cap, testId }: { label: string; used: number; cap: number; testId: string }) {
  const { t } = useTranslation()
  const pct = cap > 0 ? Math.min(100, Math.round((used / cap) * 100)) : 0
  const near = used / cap >= 0.9
  return (
    <li className="space-y-1" data-testid={testId}>
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="min-w-0 truncate text-fg-muted">{label}</span>
        <span className={cn('shrink-0 tabular-nums', near ? 'font-medium text-warning' : 'text-fg-subtle')} data-testid={`${testId}-used`}>
          {t('rules.budget.used', { used, cap })}
        </span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-bg-muted" role="progressbar" aria-valuemin={0} aria-valuemax={cap} aria-valuenow={used} aria-label={label}>
        <div className={cn('h-full rounded-full', near ? 'bg-warning' : 'bg-accent')} style={{ width: `${pct}%` }} />
      </div>
    </li>
  )
}

export function BudgetSummary({ budget }: { budget: RuleBudget | null }) {
  const { t } = useTranslation()
  if (!budget) return null
  const projects = Object.entries(budget.projects).sort(([a], [b]) => (a < b ? -1 : 1))
  return (
    <div className="space-y-2 rounded-lg border border-border bg-bg-subtle p-3" data-testid="rules-budget">
      <p className="text-xs text-fg-subtle">{t('rules.budget.note', { cap: budget.cap })}</p>
      <ul className="space-y-2">
        <Row label={t('rules.budget.node')} used={budget.node_bytes} cap={budget.cap} testId="rules-budget-node" />
        {projects.map(([name, bytes]) => (
          <Row key={name} label={t('rules.budget.project', { project: cleanHintText(name) || name })} used={budget.node_bytes + bytes} cap={budget.cap} testId="rules-budget-project" />
        ))}
      </ul>
    </div>
  )
}
