import { useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { spaceLabelOf } from '@/lib/spaces'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { crossesSpace, moveBlock, moveText, moveTodo, newMoveProgress, wasAgentAdded, type MoveProgress, type MoveResult } from '../move'
import type { ViewTodo } from '../types'

/**
 * 「移到另一个节点…」（spec/web.md § 4.13）：选目标节点（按空间分组），跨空间先确认；两个节点都要在线、这一项没有没发出去的改动。
 * 移动进行中对话框关不掉（取消、Esc、×、点外面都不行，写明原因）：结果要在这里看到，半途停下的结果另外还有一条提示。
 * 同一次移动的重试共用进度：目标上的 id 不变、做完的步骤不重做；补状态失败 / 原节点删不掉之后只给能接着做的那一步，不给「再移一次」。
 */
export function MoveTodoDialog({ item, open, onOpenChange, onMoved }: { item: ViewTodo; open: boolean; onOpenChange: (o: boolean) => void; onMoved?: () => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const { spaces } = useNodeFilter()
  const [target, setTarget] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  const [failure, setFailure] = useState<MoveResult | null>(null)
  const progress = useRef<MoveProgress>(newMoveProgress())
  const name = (id: string): string => cleanHintText(byId[id]?.name) || id.slice(0, 8)
  // 目标：别的、支持清单的节点（不论有没有选中：空间只管看什么，不管能移到哪）
  const targets = useMemo(() => order.filter((id) => id !== item.node_id && byId[id]?.info?.capabilities.todo === true), [order, item.node_id, byId])
  const own = moveBlock(item, target || undefined)
  const groups = spaces.hasGroups
    ? [...spaces.spaces.map((sp) => ({ label: sp.label, ids: sp.nodeIds.filter((id) => targets.includes(id)) })), { label: t('spaces.noSpace'), ids: spaces.ungrouped.filter((id) => targets.includes(id)) }].filter((g) => g.ids.length > 0)
    : [{ label: '', ids: targets }]
  const cross = target !== '' && crossesSpace(item.node_id, target)
  // 目标上已经有了一份：目标就不能再换，这次移动只能接着做下去（或放着不管）
  const started = progress.current.added !== null
  const lastStage = failure && !failure.ok ? failure.stage : null
  const changed = lastStage === 'changed'
  const actionLabel = busy ? t('spaces.move.moving') : lastStage === 'state' ? t('spaces.move.retryState') : lastStage === 'stuck' ? t('spaces.move.retryDelete') : t('spaces.move.action')

  const close = (o: boolean): void => {
    if (busy) return // 移动进行中不能关
    onOpenChange(o)
  }

  const run = async (): Promise<void> => {
    if (!target) return
    // 跨空间：先让用户确认一次（接着做下去的重试不再问：目标已经定了）
    if (cross && !confirming && !started) {
      setConfirming(true)
      return
    }
    setBusy(true)
    setFailure(null)
    const r = await moveTodo(item, target, progress.current)
    setBusy(false)
    if (r.ok) {
      if (!r.sourceAlreadyGone) toast.success(t('spaces.move.moved', { node: name(target) }))
      onOpenChange(false)
      onMoved?.()
    } else {
      setFailure(r)
      setConfirming(false)
    }
  }

  const sourceBlock = moveBlock(item)

  return (
    <Dialog open={open} onOpenChange={close} title={t('spaces.move.dialogTitle')} description={cleanHintText(item.title, 80)} closeLabel={t('common.close')} testId="move-todo-dialog">
      <div className="space-y-3 text-sm">
        {sourceBlock && !started ? (
          <p role="alert" className="text-danger" data-testid="move-blocked">
            {t(`spaces.move.blocked.${sourceBlock}`)}
          </p>
        ) : targets.length === 0 ? (
          <p className="text-fg-muted">{t('spaces.move.noTarget')}</p>
        ) : (
          <>
            <label className="block space-y-1 font-medium">
              {t('spaces.move.pick')}
              <Select
                value={target}
                disabled={busy || started}
                onChange={(e) => {
                  setTarget(e.target.value)
                  setConfirming(false)
                  setFailure(null)
                  progress.current = newMoveProgress()
                }}
                data-testid="move-target"
              >
                <option value="">{'—'}</option>
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
            {target && own && !failure && !started ? (
              <p role="alert" className="text-danger" data-testid="move-blocked">
                {t(`spaces.move.blocked.${own === 'unsupported' ? 'offlineTarget' : own}`, { node: name(target) })}
              </p>
            ) : null}
            {confirming ? (
              <p role="alert" className="rounded-md border border-warning/40 bg-warning-subtle p-2" data-testid="move-confirm-space">
                {spaceLabelOf(spaces, target) ? t('spaces.move.confirmSpace', { node: name(target), space: spaceLabelOf(spaces, target) }) : t('spaces.move.confirmSpaceNone', { node: name(target) })}
              </p>
            ) : null}
            {failure && !failure.ok ? (
              <p role="alert" className="text-danger" data-testid="move-failure">
                {moveText(failure, name(target), item.state)}
              </p>
            ) : null}
            {busy ? (
              <p role="status" className="text-fg-muted" data-testid="move-busy">
                {t('spaces.move.cannotClose')}
              </p>
            ) : null}
          </>
        )}
        <div className="flex justify-end gap-2">
          <Button disabled={busy} onClick={() => close(false)} data-testid="move-cancel">
            {started ? t('spaces.move.leave') : t('common.cancel')}
          </Button>
          <Button variant="primary" disabled={busy || !target || changed || (!started && (!!own || !!sourceBlock))} onClick={() => void run()} data-testid="move-confirm">
            {actionLabel}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
