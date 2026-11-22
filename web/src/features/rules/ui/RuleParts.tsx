// 一条规则的几个展示件，规则页、「待处理」的详情共用。规则的文字是用户内容，也可能是 agent 写的待确认的话：
// 一律按纯文字显示（保留换行，不当 Markdown、不当 HTML）。
import { Link } from '@tanstack/react-router'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { formatRelative } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { Chip, type Tone } from '@/ui/Misc'
import { useMinute } from '@/features/inbox/ui/info'
import { sourceOf, statusOf, type ViewRule } from '../store'

export { RuleText } from './RuleText'

const STATUS_TONE: Record<ReturnType<typeof statusOf>, Tone> = { pending: 'warning', active: 'success', disabled: 'neutral', rejected: 'neutral', unknown: 'neutral' }

/** 状态：每种都带文字。不认识的状态（更新的 daemon）按「不生效」显示。 */
export function RuleStatusChip({ rule }: { rule: Pick<ViewRule, 'status'> }) {
  const { t } = useTranslation()
  const s = statusOf(rule)
  return (
    <Chip tone={STATUS_TONE[s]} testId="rule-status" dataState={s}>
      {t(`rules.status.${s}`)}
    </Chip>
  )
}

/** 范围：全节点，或某个项目。 */
export function ruleScope(t: TFunction, rule: Pick<ViewRule, 'project'>): string {
  return rule.project ? t('rules.scope.project', { project: cleanHintText(rule.project) || rule.project }) : t('rules.scope.node')
}

/** 来源一行：手写 / 答复里记成规则 / agent 提的（带回到那个会话的链接）/ 提炼出来的；不认识的种类按「不是人写的」。 */
export function RuleSourceLine({ rule }: { rule: ViewRule }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const sessionId = rule.source.session_id ?? undefined
  const sessionKnown = useSessions((s) => (sessionId ? s.list[sessionId]?.node_id === rule.node_id : false))
  const kind = sourceOf(rule)
  const label = t(`rules.source.${kind}`)
  return (
    <span data-testid="rule-source" data-kind={kind}>
      {label}
      {sessionId ? (
        <>
          {' · '}
          {sessionKnown ? (
            <Link to="/n/$node/s/$session" params={{ node: shortNodeId(rule.node_id, order), session: sessionId }} search={kind === 'extracted' && rule.source.seq != null ? { seq: rule.source.seq } : {}} className="font-medium text-accent-text hover:underline" data-testid="rule-source-session">
              {t('rules.source.session')}
            </Link>
          ) : (
            <span data-testid="rule-source-gone">{t('rules.source.sessionGone')}</span>
          )}
        </>
      ) : null}
    </span>
  )
}

/** 范围、来源、状态、最近一次用到的时间。 */
export function RuleMeta({ rule, showNode }: { rule: ViewRule; showNode?: boolean }) {
  const { t, i18n } = useTranslation()
  useMinute()
  const nodeName = useNodes((s) => s.byId[rule.node_id]?.name)
  return (
    <div className="space-y-1" data-testid="rule-meta">
      <div className="flex flex-wrap items-center gap-1.5">
        <RuleStatusChip rule={rule} />
        <Chip tone="info" testId="rule-scope">
          {ruleScope(t, rule)}
        </Chip>
        {showNode ? (
          <Chip testId="rule-node">{cleanHintText(nodeName) || rule.node_id.slice(0, 8)}</Chip>
        ) : null}
      </div>
      <p className="text-xs text-fg-subtle">
        <RuleSourceLine rule={rule} />
      </p>
      <p className="text-xs text-fg-subtle" data-testid="rule-last-used">
        {rule.last_injected_ms ? t('rules.lastUsed', { time: formatRelative(rule.last_injected_ms, i18n.language) }) : t('rules.neverUsed')}
      </p>
    </div>
  )
}
