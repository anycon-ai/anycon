// 规则页：按空间显示规则，提供确认、编辑和同组版本对齐。
import { GroupAlignment } from '@/features/rules/ui/GroupAlignment'
import { groupPeers, useRuleSync } from '@/features/rules/groupSync'
import { groupOf, displayName } from '@/lib/spaces'
import { Select } from '@/ui/Field'
import { ExtractHistory } from '@/features/rules/ui/ExtractHistory'
import { useMemo, useState } from 'react'
import { ChevronRight, Plus, ScrollText } from 'lucide-react'
import { Collapsible } from 'radix-ui'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { setStatusMany } from '@/features/rules/service'
import { isStaleRule, statusOf, useRuleView, useRules, useRulesNodes, type ViewRule } from '@/features/rules/store'
import { BudgetSummary } from '@/features/rules/ui/BudgetSummary'
import { failText } from '@/features/rules/ui/errors'
import { RuleCard } from '@/features/rules/ui/RuleCard'
import { ruleKey, useRuleOps } from '@/features/rules/ui/useRuleOps'
import { resolveNodeParam } from '@/lib/ids'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { EmptyState, Segmented, Spinner } from '@/ui/Misc'
import { cn } from '@/ui/cn'

type Tab = 'pending' | 'active'

/**
 * 规则页（`/rules`，spec/web.md § 4.14，docs/spec/daemon.md § 23）：勾选的、支持规则的节点上的规则。两个标签：待确认、已生效；
 * 停用和否决的（还有这个版本不认识状态的）收在「已生效」下面的折叠里。每条规则的全文（纯文字）都在卡片上；待确认的可以多选一起确认 / 否决。
 * 来源节点不在线时禁用写入；成功后，同组目标的期望状态由持久化队列转发。
 */
export function RulesPage({ initialTab, focus }: { initialTab?: Tab; focus?: { node: string; rule: string } } = {}) {
  const { t } = useTranslation()
  const { selected } = useNodeFilter()
  const [alignmentTarget, setAlignmentTarget] = useState<string | null>(null)
  const [bulkSync, setBulkSync] = useState<'node' | 'group'>('node')
  const review = useRuleSync((s) => s.review)
  const records = useRuleSync((s) => s.records)
  const supporting = useRulesNodes(selected)
  const all = useRuleView(supporting)
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const focusNode = focus ? resolveNodeParam(focus.node, order) : undefined
  const focusNodes = useRulesNodes(focusNode ? [focusNode] : [])
  const focusRules = useRuleView(focusNodes)
  const focused = focusRules.find((r) => r.id === focus?.rule)
  const state = useRules((s) => s.nodes)
  const alignmentGroups = useMemo(() => {
    const groups = new Map<string, string>()
    for (const id of selected) {
      const key = groupOf(nodes[id]?.info)
      const members = [id, ...groupPeers(id)]
      if ((key && members.filter((n) => nodes[n]?.info?.capabilities.rules).length > 1) || records.some((r) => members.includes(r.node) && r.state === 'failed')) groups.set(key || id, id)
    }
    return [...groups.values()]
  }, [selected, nodes, records])
  const alignmentNode = review ?? alignmentTarget
  const alignmentMembers = alignmentNode ? [alignmentNode, ...groupPeers(alignmentNode)].filter((id) => !!nodes[id]).join(':') : ''
  const alignmentNodes = useMemo(() => alignmentMembers ? alignmentMembers.split(':') : [], [alignmentMembers])
  const showNode = supporting.length > 1
  const { ops, dialogs } = useRuleOps()

  const pending = useMemo(() => all.filter((r) => statusOf(r) === 'pending'), [all])
  // 过期的规则状态还是 `active`，但不再交给 agent、不占字数预算：另起一段
  const active = useMemo(() => all.filter((r) => statusOf(r) === 'active' && !isStaleRule(r)), [all])
  const staleRules = useMemo(() => all.filter(isStaleRule), [all])
  const other = useMemo(() => all.filter((r) => !['pending', 'active'].includes(statusOf(r))), [all])
  const [tab, setTab] = useState<Tab>(() => initialTab ?? (pending.length > 0 ? 'pending' : 'active'))
  // 勾选时记下那一刻看到的版本（`updated_ms`）：批量确认 / 否决发的就是它；规则在这之后变了，勾选作废（卡片上标出来，要重新勾）
  const [checked, setChecked] = useState<ReadonlyMap<string, number>>(new Map())
  const [bulkBusy, setBulkBusy] = useState(false)
  const loaded = supporting.every((id) => state[id]?.loaded)
  const offline = supporting.filter((id) => nodes[id]?.conn !== 'ready')
  const onlineAll = offline.length === 0

  const checkedRules = pending.filter((r) => checked.get(ruleKey(r)) === r.updated_ms)
  const staleKeys = new Set(pending.filter((r) => checked.has(ruleKey(r)) && checked.get(ruleKey(r)) !== r.updated_ms).map(ruleKey))
  const toggle = (r: ViewRule, on: boolean): void =>
    setChecked((s) => {
      const n = new Map(s)
      if (on) n.set(ruleKey(r), r.updated_ms)
      else n.delete(ruleKey(r))
      return n
    })

  const bulkHasPeers = checkedRules.some((r) => groupPeers(r.node_id).length > 0)
  const bulkProject = checkedRules.some((r) => !!r.project)

  const bulk = async (status: 'active' | 'rejected'): Promise<void> => {
    const targets = checkedRules.filter((r) => nodes[r.node_id]?.conn === 'ready')
    if (targets.length === 0) return
    setBulkBusy(true)
    const res = await setStatusMany(targets, status, status === 'active' && bulkHasPeers ? (bulkProject ? 'node' : bulkSync) : undefined)
    setBulkBusy(false)
    setChecked(new Map())
    setBulkSync('node')
    if (res.failed.length === 0) toast.success(t(status === 'active' ? 'rules.bulk.confirmed' : 'rules.bulk.rejected', { count: res.done }))
    else toast.error(t('rules.bulk.partial', { done: res.done, failed: res.failed.length, reason: failText(t, res.failed[0]!.fail) }))
  }

  const group = (rules: ViewRule[], withCheck: boolean) =>
    supporting.map((nodeId) => {
      const here = rules.filter((r) => r.node_id === nodeId)
      if (here.length === 0) return null
      return (
        <section key={nodeId} className="space-y-2" data-testid="rules-node" data-node={nodeId}>
          {showNode ? <h3 className="text-xs font-semibold tracking-wide text-fg-muted uppercase">{cleanHintText(nodes[nodeId]?.name) || nodeId.slice(0, 8)}</h3> : null}
          <ul className="space-y-2">
            {here.map((r) => (
              <RuleCard key={ruleKey(r)} rule={r} showNode={showNode} online={nodes[r.node_id]?.conn === 'ready'} ops={ops} check={withCheck ? { checked: checked.get(ruleKey(r)) === r.updated_ms, stale: staleKeys.has(ruleKey(r)), onChange: (v) => toggle(r, v) } : undefined} />
            ))}
          </ul>
        </section>
      )
    })

  const addNodes = supporting.filter((id) => nodes[id]?.conn === 'ready')

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="rules-page">
      <PageHeader
        root
        crumbs={[{ label: t('rules.title') }]}
        showNodeStatus={false}
        actions={
          <>
          {alignmentGroups.map((id) => <Button key={id} size="sm" onClick={() => setAlignmentTarget(id)} data-testid="rules-align">{t('rules.group.align')}{alignmentGroups.length > 1 ? ` · ${displayName(groupOf(nodes[id]?.info)) || cleanHintText(nodes[id]?.name) || id.slice(0, 8)}` : ''}</Button>)}
          <Button size="sm" variant="primary" disabled={addNodes.length === 0} onClick={() => ops.add(addNodes)} data-testid="rules-add" className="max-md:min-h-11">
            <Plus className="size-4" aria-hidden />
            {t('rules.add')}
          </Button>
          </>
        }
      />
      <GroupAlignment nodes={alignmentNodes} open={alignmentNode !== null} onOpenChange={(open) => { if (!open) { setAlignmentTarget(null); useRuleSync.setState({ review: null }) } }} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl space-y-4 p-3 md:p-4">
          <p className="text-sm text-fg-muted" data-testid="rules-intro">
            {t('rules.intro')}
          </p>
          {focus ? <section className="space-y-2 rounded border border-border p-3" data-testid="rules-focused">
            <h2 className="font-semibold">{t('twin.referencedRule')}</h2>
            {focused ? <ul><RuleCard rule={focused} showNode online={nodes[focused.node_id]?.conn === 'ready'} ops={ops} /></ul> : <p>{focusNode && nodes[focusNode]?.conn !== 'ready' ? t('twin.offline') : focusNode && state[focusNode]?.failed ? t('twin.loadFailed') : focusNodes.length && !state[focusNode!]?.loaded ? t('twin.loading') : t('twin.ruleGone')}</p>}
          </section> : null}
          {supporting.length === 0 ? (
            <EmptyState icon={<ScrollText className="size-6" />} title={t('rules.unsupported.title')}>
              {t('rules.unsupported.body')}
            </EmptyState>
          ) : (
            <>
              {offline.length > 0 ? (
                <p className="rounded-md border border-warning/40 bg-warning-subtle px-2.5 py-1.5 text-sm text-warning" role="status" data-testid="rules-offline">
                  {t('rules.offline', { nodes: offline.map((id) => cleanHintText(nodes[id]?.name) || id.slice(0, 8)).join(', ') })}
                </p>
              ) : null}
              {supporting.map((id) => (
                <div key={id} className="space-y-1">
                  {showNode ? <p className="text-xs font-medium text-fg-muted">{cleanHintText(nodes[id]?.name) || id.slice(0, 8)}</p> : null}
                  <BudgetSummary budget={state[id]?.budget ?? null} />
                </div>
              ))}
              <Segmented
                value={tab}
                onChange={setTab}
                ariaLabel={t('rules.tabs.label')}
                testId="rules-tabs"
                options={[
                  { value: 'pending', label: t('rules.tabs.pending', { count: pending.length }), testId: 'rules-tab-pending' },
                  { value: 'active', label: t('rules.tabs.active', { count: active.length }), testId: 'rules-tab-active' },
                ]}
              />
              {!loaded ? (
                <div className="flex justify-center py-6">
                  <Spinner />
                </div>
              ) : tab === 'pending' ? (
                <div className="space-y-3" data-testid="rules-pending">
                  {pending.length === 0 ? (
                    <p className="py-6 text-center text-sm text-fg-subtle" data-testid="rules-pending-empty">
                      {t('rules.pendingEmpty')}
                    </p>
                  ) : (
                    <>
                      <div className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-bg-subtle px-2.5 py-1.5" data-testid="rules-bulk">
                        <label className="flex items-center gap-2 text-sm">
                          <input
                            type="checkbox"
                            checked={checkedRules.length === pending.length}
                            ref={(el) => {
                              if (el) el.indeterminate = checkedRules.length > 0 && checkedRules.length < pending.length
                            }}
                            onChange={(e) => setChecked(e.target.checked ? new Map(pending.map((r) => [ruleKey(r), r.updated_ms])) : new Map())}
                            className={cn('size-4 accent-[var(--accent)] [@media(pointer:coarse)]:size-5')}
                            data-testid="rules-select-all"
                          />
                          {t('rules.selectAll')}
                        </label>
                        <span className="min-w-0 flex-1 text-xs text-fg-subtle" aria-live="polite">
                          {t('rules.selected', { count: checkedRules.length })}
                        </span>
                        {bulkHasPeers ? <label>{t('rules.group.scope')}<Select aria-label={t('rules.group.scope')} value={bulkProject ? 'node' : bulkSync} onChange={(e) => setBulkSync(e.target.value === 'group' ? 'group' : 'node')} data-testid="rules-bulk-sync"><option value="node">{t('rules.group.node')}</option><option value="group" disabled={bulkProject}>{t('rules.group.group')}</option></Select>{bulkProject ? <span>{t('rules.group.projectDisabled')}</span> : null}</label> : null}
                        <Button size="sm" variant="primary" className="max-md:min-h-11" disabled={checkedRules.length === 0 || bulkBusy || !onlineAll} onClick={() => void bulk('active')} data-testid="rules-bulk-confirm">
                          {t('rules.bulk.confirmSelected')}
                        </Button>
                        <Button size="sm" variant="secondary" className="max-md:min-h-11" disabled={checkedRules.length === 0 || bulkBusy || !onlineAll} onClick={() => void bulk('rejected')} data-testid="rules-bulk-reject">
                          {t('rules.bulk.rejectSelected')}
                        </Button>
                      </div>
                      {group(pending, true)}
                    </>
                  )}
                </div>
              ) : (
                <div className="space-y-4" data-testid="rules-active">
                  {active.length === 0 && staleRules.length === 0 ? (
                    <p className="py-6 text-center text-sm text-fg-subtle" data-testid="rules-active-empty">
                      {t('rules.activeEmpty')}
                    </p>
                  ) : (
                    group(active, false)
                  )}
                  {staleRules.length > 0 ? (
                    <Collapsible.Root className="space-y-2" data-testid="rules-stale">
                      <Collapsible.Trigger className="group flex min-h-9 items-center gap-1.5 text-sm font-medium text-fg-muted hover:text-fg" data-testid="rules-stale-toggle">
                        <ChevronRight className="size-4 transition-transform group-data-[state=open]:rotate-90" aria-hidden />
                        {t('rules.stale.title', { count: staleRules.length })}
                      </Collapsible.Trigger>
                      <Collapsible.Content className="space-y-3">
                        <p className="text-xs text-fg-subtle" data-testid="rules-stale-note">
                          {t('rules.stale.note')}
                        </p>
                        {group(staleRules, false)}
                      </Collapsible.Content>
                    </Collapsible.Root>
                  ) : null}
                  {other.length > 0 ? (
                    <Collapsible.Root className="space-y-2" data-testid="rules-inactive">
                      <Collapsible.Trigger className="group flex min-h-9 items-center gap-1.5 text-sm font-medium text-fg-muted hover:text-fg" data-testid="rules-inactive-toggle">
                        <ChevronRight className="size-4 transition-transform group-data-[state=open]:rotate-90" aria-hidden />
                        {t('rules.inactive', { count: other.length })}
                      </Collapsible.Trigger>
                      <Collapsible.Content className="space-y-3">{group(other, false)}</Collapsible.Content>
                    </Collapsible.Root>
                  ) : null}
                </div>
              )}
            </>
          )}
          <ExtractHistory nodeIds={selected} />
        </div>
      </div>
      {dialogs}
    </div>
  )
}
