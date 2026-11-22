// 当前空间的规则对齐：在线节点读整表，离线显示未知；由用户选版本，写入与日常同步相同的持久化队列。
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { useNodes } from '@/stores/nodes'
import { cleanHintText } from '@/lib/hint'
import { alignRule, compareGroupRules, discardRuleRecord, readGroupRules, reloadRuleSync, retryRuleRecord, useRuleSync, type NodeRuleSnapshot } from '../groupSync'
import { desiredRule } from '../syncTypes'
import { syncErrorText } from './errors'
import { RuleText } from './RuleText'

export function GroupAlignment({ nodes, open, onOpenChange }: { nodes: readonly string[]; open: boolean; onOpenChange: (open: boolean) => void }) {
  const { t } = useTranslation()
  const entries = useNodes((s) => s.byId)
  const records = useRuleSync((s) => s.records)
  const [snapshots, setSnapshots] = useState<NodeRuleSnapshot[]>([])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState(false)
  // 断线后已读的快照也变为未知，不能拿旧值覆盖其他节点。
  const current = nodes.map((node) => ({ node, rules: entries[node]?.conn === 'ready' ? snapshots.find((s) => s.node === node)?.rules ?? null : null }))
  const differences = compareGroupRules(current)
  const availability = nodes.map((node) => entries[node]?.conn === 'ready' ? '1' : '0').join('')
  const revision = records.filter((r) => nodes.includes(r.node)).map((r) => `${r.seq}:${r.state}`).join(',')
  const failed = records.filter((r) => nodes.includes(r.node) && r.state === 'failed')
  const refresh = async (): Promise<void> => {
    setBusy(true)
    setSnapshots(await readGroupRules(nodes))
    await reloadRuleSync()
    setBusy(false)
  }
  useEffect(() => {
    if (!open) return
    let cancelled = false
    void readGroupRules(nodes).then((result) => { if (!cancelled) setSnapshots(result) })
    return () => { cancelled = true }
  }, [open, nodes, availability, revision])
  const run = async (fn: () => Promise<void>): Promise<void> => {
    setBusy(true)
    setError(false)
    try { await fn(); await refresh() } catch { setError(true) } finally { setBusy(false) }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('rules.group.align')} description={t('rules.group.alignNote')} size="lg" closeLabel={t('common.close')} testId="rule-alignment">
      <div className="space-y-4 text-sm">
        <Button disabled={busy} onClick={() => void run(async () => {})}>{t('rules.group.refresh')}</Button>
        {error ? <p role="alert">{t('rules.group.operationFailed')}</p> : null}
        {current.filter((s) => s.rules === null).map((s) => <p key={s.node}>{cleanHintText(entries[s.node]?.name) || s.node.slice(0, 8)}: {t('rules.group.unknown')}</p>)}
        {current.length > 0 && current.every((s) => s.rules !== null) && differences.length === 0 ? <p>{t('rules.group.consistent')}</p> : null}
        {differences.map((diff) => (
          <section key={diff.id} className="space-y-2 rounded-md border border-border p-3" data-testid="rule-difference">
            <p className="text-xs text-fg-muted">{diff.id}</p>
            {diff.versions.map((v) => (
              <div key={v.node} className="space-y-1 rounded bg-bg-subtle p-2">
                <p>{cleanHintText(entries[v.node]?.name) || v.node.slice(0, 8)}</p>
                {v.unknown ? <p>{t('rules.group.unknown')}</p> : v.rule ? <><RuleText text={v.rule.text} /><p>{t(`rules.status.${v.rule.status === 'unknown' ? 'unknown' : v.rule.status}`)} · {t(v.rule.sync === 'group' ? 'rules.group.badge' : 'rules.group.node')}</p></> : <p>{t('rules.group.missing')}</p>}
                {!v.unknown && v.rule && desiredRule({ ...v.rule, sync: 'group' }) ? <Button disabled={busy} onClick={() => void run(() => alignRule(nodes, diff.id, v.rule!, v.rule!.sync === 'group' ? v.node : undefined))}>{t('rules.group.useVersion')}</Button> : null}
              </div>
            ))}
            <Button variant="outline-danger" disabled={busy} onClick={() => void run(() => alignRule(nodes, diff.id, 'deleted'))}>{t('rules.group.deleteAll')}</Button>
          </section>
        ))}
        {failed.map((rec) => (
          <section key={rec.seq} className="space-y-2 rounded-md border border-danger p-3" data-testid="rule-sync-failed">
            <p>{cleanHintText(entries[rec.node]?.name) || rec.node.slice(0, 8)} · {rec.rule_id}</p>
            {rec.desired !== 'deleted' ? <RuleText text={rec.desired.text} /> : <p>{t('rules.group.deleteAll')}</p>}
            <p role="status">{syncErrorText(t, rec.error)}</p>
            <Button disabled={busy} onClick={() => void run(() => retryRuleRecord(rec))}>{t('rules.group.retry')}</Button>
            <Button disabled={busy} onClick={() => void run(() => discardRuleRecord(rec))}>{t('rules.group.discard')}</Button>
          </section>
        ))}
      </div>
    </Dialog>
  )
}
