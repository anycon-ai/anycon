import { useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { spaceLabelOf } from '@/lib/spaces'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { abandonMove, canDeleteCopy, copyText, crossesSpace, isRunning, isTouched, moveBlock, moveKey, moveText, moveTodo, removeCopy, useMoves, wasAgentAdded } from '../move'
import { isAgentSource } from '../source'
import type { ViewTodo } from '../types'

/**
 * 「移到另一个节点…」（spec/web.md § 4.13）：选目标节点（按空间分组），跨空间先确认；两个节点都要在线、这一项没有没发出去的改动。
 * 这里只显示 `move.ts` 里这一项的那次移动、向它发命令：进度不在对话框里，关掉、重新挂载之后打开仍然是同一次移动，不会再起第二次。
 * 移动进行中对话框关不掉（取消、Esc、×、点外面都不行，写明原因）；半途停下的结果另外还有一条提示。
 * 目标上已经有了一份之后目标不能再换，只给能接着做的那一步，外加「删掉目标上的那一份」和「两份都留着」。
 * 内容和检查对着打开对话框那一刻的样子（已经开始的移动对着它确认时的样子），不对着界面现在拿到的那一份。
 */
export function MoveTodoDialog({ item, open, onOpenChange }: { item: ViewTodo; open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const { spaces } = useNodeFilter()
  const entry = useMoves((s) => s.moves[moveKey(item.node_id, item.id)])
  // 打开那一刻的样子：之后界面上的这一项变了，移动对着的还是这一份，原节点变了就停下
  const [snap, setSnap] = useState({ item, open })
  if (open !== snap.open) setSnap({ item: open ? item : snap.item, open })
  const base = entry?.base ?? snap.item
  const [pick, setPick] = useState('')
  const [confirming, setConfirming] = useState(false)
  const touched = isTouched(entry)
  const target = touched ? entry!.targetNode : pick !== '' ? pick : (entry?.targetNode ?? '')
  const busy = isRunning(entry)
  const failure = !busy && entry?.outcome && !entry.outcome.ok ? entry.outcome : null
  const name = (id: string): string => cleanHintText(byId[id]?.name) || id.slice(0, 8)
  // 目标：别的、支持清单的节点（不论有没有选中：空间只管看什么，不管能移到哪）
  const targets = useMemo(() => order.filter((id) => id !== item.node_id && byId[id]?.info?.capabilities.todo === true), [order, item.node_id, byId])
  const own = moveBlock(base, target || undefined)
  const sourceBlock = moveBlock(base)
  const groups = spaces.hasGroups
    ? [...spaces.spaces.map((sp) => ({ label: sp.label, ids: sp.nodeIds.filter((id) => targets.includes(id)) })), { label: t('spaces.noSpace'), ids: spaces.ungrouped.filter((id) => targets.includes(id)) }].filter((g) => g.ids.length > 0)
    : [{ label: '', ids: targets }]
  const cross = target !== '' && crossesSpace(item.node_id, target)
  const stage = failure ? failure.stage : null
  const dead = stage === 'changed' || stage === 'copyGone'
  const actionLabel = busy
    ? t('spaces.move.moving')
    : touched
      ? stage === 'state'
        ? t('spaces.move.retryState')
        : stage === 'stuck' || (failure?.stage === 'link' && failure.at === 'delete')
          ? t('spaces.move.retryDelete')
          : t('spaces.move.resume')
      : t('spaces.move.action')
  const hasCopy = canDeleteCopy(entry)
  const copyFailure = !busy && entry?.copyOutcome ? entry.copyOutcome : null

  // 在跑的移动结束、记录没了（成功，或者删掉了目标上的那一份）：对话框也该关了，不管它是不是中途重新挂载的
  const wasRunning = useRef(false)
  useEffect(() => {
    if (busy) wasRunning.current = true
    else if (wasRunning.current && !entry) {
      wasRunning.current = false
      onOpenChange(false)
    } else if (entry) wasRunning.current = false
  }, [busy, entry, onOpenChange])

  const close = (o: boolean): void => {
    if (busy) return // 移动进行中不能关
    onOpenChange(o)
  }

  const run = (): void => {
    if (!target) return
    // 跨空间：先让用户确认一次（接着做下去的重试不再问：目标已经定了）
    if (cross && !confirming && !touched) {
      setConfirming(true)
      return
    }
    setConfirming(false)
    void moveTodo(base, target)
  }

  const leave = (): void => {
    if (busy) return
    abandonMove(item.node_id, item.id)
    onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={close} title={t('spaces.move.dialogTitle')} description={cleanHintText(base.title, 80)} closeLabel={t('common.close')} testId="move-todo-dialog">
      <div className="space-y-3 text-sm">
        {sourceBlock && !touched ? (
          <p role="alert" className="text-danger" data-testid="move-blocked">
            {t(`spaces.move.blocked.${sourceBlock}`)}
          </p>
        ) : targets.length === 0 && !touched ? (
          <p className="text-fg-muted">{t('spaces.move.noTarget')}</p>
        ) : (
          <>
            <label className="block space-y-1 font-medium">
              {t('spaces.move.pick')}
              <Select
                value={target}
                disabled={busy || touched}
                onChange={(e) => {
                  setPick(e.target.value)
                  setConfirming(false)
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
            {wasAgentAdded(base) ? <p className="text-fg-muted">{t(isAgentSource(base.source) ? 'spaces.move.agentNote' : 'spaces.move.agentNoteOther')}</p> : null}
            {target && own && !failure && !touched ? (
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
                {moveText(failure, name(target), base.state)}
              </p>
            ) : null}
            {copyFailure ? (
              <p role="alert" className="text-danger" data-testid="move-copy-failure">
                {copyText(copyFailure, name(target))}
              </p>
            ) : null}
            {busy ? (
              <p role="status" className="text-fg-muted" data-testid="move-busy">
                {t('spaces.move.cannotClose')}
              </p>
            ) : null}
          </>
        )}
        <div className="flex flex-wrap justify-end gap-2">
          <Button disabled={busy} onClick={leave} data-testid="move-cancel">
            {touched ? t('spaces.move.leaveBoth') : t('common.cancel')}
          </Button>
          {hasCopy ? (
            <Button onClick={() => void removeCopy(item.node_id, item.id)} data-testid="move-delete-copy">
              {t('spaces.move.deleteCopy', { node: name(target) })}
            </Button>
          ) : null}
          <Button variant="primary" disabled={busy || !target || dead || (!touched && (!!own || !!sourceBlock))} onClick={run} data-testid="move-confirm">
            {actionLabel}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
