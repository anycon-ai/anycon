import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { spaceLabelOf } from '@/lib/spaces'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { crossesSpace, moveBlock, moveTodo, wasAgentAdded, type MoveResult } from '../move'
import type { ViewTodo } from '../types'

/** 「移到另一个节点…」（spec/web.md § 4.13）：选目标节点（按空间分组），跨空间先确认；两个节点都要在线、这一项没有没发出去的改动。 */
export function MoveTodoDialog({ item, open, onOpenChange, onMoved }: { item: ViewTodo; open: boolean; onOpenChange: (o: boolean) => void; onMoved?: () => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const { spaces } = useNodeFilter()
  const [target, setTarget] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  const [failure, setFailure] = useState<MoveResult | null>(null)
  const name = (id: string): string => cleanHintText(byId[id]?.name) || id.slice(0, 8)
  // 目标：别的、支持清单的节点（不论有没有选中：空间只管看什么，不管能移到哪）
  const targets = useMemo(() => order.filter((id) => id !== item.node_id && byId[id]?.info?.capabilities.todo === true), [order, item.node_id, byId])
  const own = moveBlock(item, target || undefined)
  const groups = spaces.hasGroups
    ? [...spaces.spaces.map((sp) => ({ label: sp.label, ids: sp.nodeIds.filter((id) => targets.includes(id)) })), { label: t('spaces.noSpace'), ids: spaces.ungrouped.filter((id) => targets.includes(id)) }].filter((g) => g.ids.length > 0)
    : [{ label: '', ids: targets }]
  const cross = target !== '' && crossesSpace(item.node_id, target)

  const run = async (): Promise<void> => {
    if (!target) return
    // 跨空间：先让用户确认一次
    if (cross && !confirming) {
      setConfirming(true)
      return
    }
    setBusy(true)
    setFailure(null)
    const r = await moveTodo(item, target)
    setBusy(false)
    if (r.ok) {
      toast.success(t('spaces.move.moved', { node: name(target) }))
      onOpenChange(false)
      onMoved?.()
    } else {
      setFailure(r)
      setConfirming(false)
    }
  }

  const failureText = (r: MoveResult): string | null => {
    if (r.ok) return null
    switch (r.stage) {
      case 'blocked':
        return t(`spaces.move.blocked.${r.block === 'unsupported' ? 'offlineTarget' : r.block}`, { node: name(target) })
      case 'rejected':
        return t('spaces.move.failedAdd', { node: name(target), reason: r.reason })
      case 'link':
        return t('spaces.move.failedLink', { node: name(target) })
      case 'stuck':
        return t('spaces.move.stuck', { node: name(target) })
    }
  }
  const sourceBlock = moveBlock(item)

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('spaces.move.dialogTitle')} description={cleanHintText(item.title, 80)} closeLabel={t('common.close')} testId="move-todo-dialog">
      <div className="space-y-3 text-sm">
        {sourceBlock ? (
          <p role="alert" className="text-danger" data-testid="move-blocked">
            {t(`spaces.move.blocked.${sourceBlock}`)}
          </p>
        ) : targets.length === 0 ? (
          <p className="text-fg-muted">{t('spaces.move.noTarget')}</p>
        ) : (
          <>
            <label className="block space-y-1 font-medium">
              {t('spaces.move.pick')}
              <Select value={target} onChange={(e) => { setTarget(e.target.value); setConfirming(false); setFailure(null) }} data-testid="move-target">
                <option value="">{'\u2014'}</option>
                {groups.map((g) => (
                  <optgroup key={g.label || 'all'} label={g.label || undefined}>
                    {g.ids.map((id) => (
                      <option key={id} value={id}>
                        {name(id)}
                      </option>
                    ))}
                  </optgroup>
                ))}
              </Select>
            </label>
            <p className="text-fg-muted">{t('spaces.move.projectNote')}</p>
            {wasAgentAdded(item) ? <p className="text-fg-muted">{t('spaces.move.agentNote')}</p> : null}
            {target && own && !failure ? (
              <p role="alert" className="text-danger" data-testid="move-blocked">
                {t(`spaces.move.blocked.${own === 'unsupported' ? 'offlineTarget' : own}`, { node: name(target) })}
              </p>
            ) : null}
            {confirming ? (
              <p role="alert" className="rounded-md border border-warning/40 bg-warning-subtle p-2" data-testid="move-confirm-space">
                {spaceLabelOf(spaces, target) ? t('spaces.move.confirmSpace', { node: name(target), space: spaceLabelOf(spaces, target) }) : t('spaces.move.confirmSpaceNone', { node: name(target) })}
              </p>
            ) : null}
            {failure ? (
              <p role="alert" className="text-danger" data-testid="move-failure">
                {failureText(failure)}
              </p>
            ) : null}
          </>
        )}
        <div className="flex justify-end gap-2">
          <Button onClick={() => onOpenChange(false)}>{t('common.cancel')}</Button>
          <Button variant="primary" disabled={busy || !target || !!own || !!sourceBlock} onClick={() => void run()} data-testid="move-confirm">
            {busy ? t('spaces.move.moving') : t('spaces.move.action')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
