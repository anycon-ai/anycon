// 规则页上的一条规则：全文（纯文字）、范围、来源、状态、最近用到的时间，和这个状态下能做的动作。
// 待确认的规则的全文一直在卡片上（不截断）：确认的按钮和全文在同一张卡片里，没有不看全文就确认的入口。
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { isStaleRule, statusOf, type ViewRule } from '../store'
import { RuleMeta, RuleText } from './RuleParts'
import { ruleKey, type RuleOps } from './useRuleOps'

export function RuleCard({ rule, showNode, online, ops, check }: { rule: ViewRule; showNode: boolean; online: boolean; ops: RuleOps; check?: { checked: boolean; /** 勾选之后这条规则变了：勾选作废，要读过新版本重新勾。 */ stale?: boolean; onChange: (v: boolean) => void } }) {
  const { t } = useTranslation()
  const status = statusOf(rule)
  const stale = isStaleRule(rule)
  const busy = ops.busy.has(ruleKey(rule))
  const off = !online || busy
  const small = 'max-md:min-h-11 max-md:px-4'
  return (
    <li className={cn('rounded-lg border bg-surface p-3', status === 'pending' ? 'border-warning/50' : 'border-border')} data-testid="rule-card" data-rule={rule.id} data-status={status} data-stale={stale ? 'true' : undefined}>
      <div className="flex items-start gap-3">
        {check ? (
          <input
            type="checkbox"
            checked={check.checked}
            onChange={(e) => check.onChange(e.target.checked)}
            aria-label={t('rules.select')}
            className="mt-1 size-4 shrink-0 accent-[var(--accent)] [@media(pointer:coarse)]:size-5"
            data-testid="rule-select"
          />
        ) : null}
        <div className="min-w-0 flex-1 space-y-2">
          {check?.stale ? (
            <p className="rounded-md border border-warning/40 bg-warning-subtle px-2.5 py-1.5 text-sm text-warning" role="status" data-testid="rule-stale">
              {t('rules.changedWhileSelected')}
            </p>
          ) : null}
          <RuleText text={rule.text} />
          <RuleMeta rule={rule} showNode={showNode} />
          <div className="flex flex-wrap gap-2 pt-1" data-testid="rule-actions">
            {status === 'pending' ? (
              <>
                <Button size="sm" variant="primary" className={small} disabled={off} onClick={() => void ops.setStatus(rule, 'active')} data-testid="rule-confirm">
                  {t('rules.actions.confirm')}
                </Button>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => ops.edit(rule, true)} data-testid="rule-edit-confirm">
                  {t('rules.actions.editConfirm')}
                </Button>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => void ops.setStatus(rule, 'rejected')} data-testid="rule-reject">
                  {t('rules.actions.reject')}
                </Button>
              </>
            ) : null}
            {stale ? (
              <Button size="sm" variant="primary" className={small} disabled={off} onClick={() => void ops.restore(rule)} data-testid="rule-restore-stale">
                {t('rules.actions.restoreStale')}
              </Button>
            ) : null}
            {status === 'active' ? (
              <>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => ops.edit(rule, false)} data-testid="rule-edit">
                  {t('rules.actions.edit')}
                </Button>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => void ops.setStatus(rule, 'disabled')} data-testid="rule-disable">
                  {t('rules.actions.disable')}
                </Button>
              </>
            ) : null}
            {status === 'disabled' || status === 'rejected' ? (
              <>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => void ops.setStatus(rule, 'active')} data-testid="rule-enable">
                  {t(status === 'disabled' ? 'rules.actions.enable' : 'rules.actions.restore')}
                </Button>
                <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => ops.edit(rule, false)} data-testid="rule-edit">
                  {t('rules.actions.edit')}
                </Button>
              </>
            ) : null}
            {status === 'unknown' ? (
              <Button size="sm" variant="secondary" className={small} disabled={off} onClick={() => ops.edit(rule, false)} data-testid="rule-edit">
                {t('rules.actions.edit')}
              </Button>
            ) : null}
            <Button size="sm" variant="outline-danger" className={small} disabled={off} onClick={() => ops.remove(rule)} data-testid="rule-delete">
              {t('rules.actions.delete')}
            </Button>
          </div>
        </div>
      </div>
    </li>
  )
}
