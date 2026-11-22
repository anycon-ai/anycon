import { Link } from '@tanstack/react-router'
import { ChevronRight, Plus, ScrollText, Server, Settings } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { AccountFooter } from '@/components/Sidebar'
import { PageHeader } from '@/components/PageHeader'
import { connectionLabel, connectionTone } from '@/components/useConnectionLabel'
import { useTwinAvailable } from '@/features/twin/store'
import { useRulesAvailable } from '@/features/rules/store'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Dot } from '@/ui/Misc'

const ROW = 'flex min-h-11 items-center gap-2 rounded-lg px-3 py-2 text-sm font-medium hover:bg-bg-hover'

/**
 * 手机「更多」标签页（`/more`，spec/web.md § 3.3）：节点（每台已配对节点的首页和设置）和设置；账号菜单（退出、扫码批准新设备、
 * 管理）在最下面——手机没有左栏了，它们不能没有入口。
 */
export function MorePage() {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const rulesAvailable = useRulesAvailable()
  const twinAvailable = useTwinAvailable()
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="more-page">
      <PageHeader root crumbs={[{ label: t('more.title') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 space-y-5 overflow-y-auto p-3">
        <section aria-labelledby="more-nodes">
          <h2 id="more-nodes" className="mb-1 px-3 text-xs font-semibold tracking-wide text-fg-muted uppercase">
            {t('more.nodes')}
          </h2>
          {order.length === 0 ? <p className="px-3 py-2 text-sm text-fg-subtle">{t('more.noNodes')}</p> : null}
          <ul className="space-y-1">
            {order.map((id) => {
              const n = byId[id]
              if (!n) return null
              const param = shortNodeId(id, order)
              return (
                <li key={id} className="rounded-lg border border-border bg-surface" data-testid="more-node" data-node={param}>
                  <p className="flex items-center gap-2 px-3 pt-2.5 text-sm font-medium">
                    <Server className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                    <span className="min-w-0 flex-1 truncate">{n.name}</span>
                    <Dot tone={connectionTone(n.conn)} />
                    <span className="text-xs font-normal text-fg-subtle">{connectionLabel(n.conn, t)}</span>
                  </p>
                  <div className="flex">
                    <Link to="/n/$node" params={{ node: param }} className={`${ROW} flex-1`} data-testid="more-node-home">
                      <span className="flex-1">{t('more.nodeHome')}</span>
                      <ChevronRight className="size-4 text-fg-subtle" aria-hidden />
                    </Link>
                    <Link to="/n/$node/settings" params={{ node: param }} className={`${ROW} flex-1`} data-testid="more-node-settings">
                      <span className="flex-1">{t('more.nodeSettings')}</span>
                      <ChevronRight className="size-4 text-fg-subtle" aria-hidden />
                    </Link>
                  </div>
                </li>
              )
            })}
          </ul>
          <Link to="/pair" className={`${ROW} mt-1 text-accent-text`} data-testid="more-pair">
            <Plus className="size-4" aria-hidden />
            {t('more.pair')}
          </Link>
        </section>
        {twinAvailable ? <Link to="/twin" className={`${ROW} border border-border bg-surface`} data-testid="more-twin"><Server className="size-4" aria-hidden />{t('twin.title')}</Link> : null}
        {rulesAvailable ? (
          <section aria-labelledby="more-rules">
            <h2 id="more-rules" className="mb-1 px-3 text-xs font-semibold tracking-wide text-fg-muted uppercase">
              {t('rules.title')}
            </h2>
            <Link to="/rules" className={`${ROW} border border-border bg-surface`} data-testid="more-rules">
              <ScrollText className="size-4 text-fg-subtle" aria-hidden />
              <span className="flex-1">{t('rules.title')}</span>
              <ChevronRight className="size-4 text-fg-subtle" aria-hidden />
            </Link>
          </section>
        ) : null}
        <section aria-labelledby="more-settings">
          <h2 id="more-settings" className="mb-1 px-3 text-xs font-semibold tracking-wide text-fg-muted uppercase">
            {t('more.settings')}
          </h2>
          <Link to="/settings" className={`${ROW} border border-border bg-surface`} data-testid="more-settings">
            <Settings className="size-4 text-fg-subtle" aria-hidden />
            <span className="flex-1">{t('more.settings')}</span>
            <ChevronRight className="size-4 text-fg-subtle" aria-hidden />
          </Link>
        </section>
        <section aria-labelledby="more-account">
          <h2 id="more-account" className="mb-1 px-3 text-xs font-semibold tracking-wide text-fg-muted uppercase">
            {t('more.account')}
          </h2>
          <div className="rounded-lg border border-border bg-surface">
            <AccountFooter />
          </div>
        </section>
      </div>
    </div>
  )
}
