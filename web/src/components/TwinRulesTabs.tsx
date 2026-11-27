// Twin 区的两个标签：Twin（运行状态，`/twin`）和规则（`/rules`），放在页面顶栏里代替面包屑（spec/web.md § 14.8）。
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { useRulesAvailable } from '@/features/rules/store'
import { useTwinAvailable } from '@/features/twin/store'
import { cn } from '@/ui/cn'

/**
 * 是一组链接而不是 Radix Tabs：两页各有自己的地址，书签、通知落点和「引用的规则」链接照旧能直达。
 * 只放有节点支持的那一页；当前这一页总在（直接打开地址时页面自己说明不支持）。
 */
export function TwinRulesTabs({ current }: { current: 'twin' | 'rules' }) {
  const { t } = useTranslation()
  const twin = useTwinAvailable() || current === 'twin'
  const rules = useRulesAvailable() || current === 'rules'
  const tab = (on: boolean): string =>
    cn(
      'flex items-center border-b-2 px-2 text-sm font-medium whitespace-nowrap',
      on ? 'border-accent text-fg' : 'border-transparent text-fg-muted hover:text-fg',
    )
  return (
    <nav aria-label={t('twin.title')} className="-mb-px flex min-w-0 flex-1 items-stretch gap-1 self-stretch" data-testid="twin-rules-tabs">
      {twin ? (
        <Link to="/twin" aria-current={current === 'twin' ? 'page' : undefined} className={tab(current === 'twin')} data-testid="twin-rules-tab-twin">
          {t('twin.title')}
        </Link>
      ) : null}
      {rules ? (
        <Link to="/rules" aria-current={current === 'rules' ? 'page' : undefined} className={tab(current === 'rules')} data-testid="twin-rules-tab-rules">
          {t('rules.title')}
        </Link>
      ) : null}
    </nav>
  )
}
