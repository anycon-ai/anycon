// 规则上的动作：确认 / 否决 / 停用 / 启用（带着看到它时的版本，`if_updated_ms`）、改、改完确认、删除（先确认）。
// 规则页和「待处理」的详情共用，对话框（编辑器、删除确认）也在这里。失败时 toast 里写原因；别处改过这条规则的，说明并让界面显示新版本。
import { useMemo, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { deleteRule, updateRule, type RuleFail } from '../service'
import { groupPeers } from '../groupSync'
import type { ViewRule } from '../store'
import { failText } from './errors'
import { RuleEditor, type EditorMode } from './RuleEditor'
import { RuleText } from './RuleText'

export interface RuleOps {
  /** `status` 之间的转换：确认（`pending` → `active`）、否决、停用、启用。成功返回 true。 */
  setStatus: (rule: ViewRule, status: 'active' | 'rejected' | 'disabled') => Promise<boolean>
  /** 恢复一条过期的规则（`rule.update { stale: false }`，带着看到它时的版本）；`over_budget` 等原因 toast 说明。成功返回 true。 */
  restore: (rule: ViewRule) => Promise<boolean>
  /** 打开编辑器；`confirm`：存的时候同时确认。 */
  edit: (rule: ViewRule, confirm: boolean) => void
  /** 打开新建（`nodeIds` 里选节点）。 */
  add: (nodeIds: readonly string[], project?: string) => void
  /** 打开删除确认框。 */
  remove: (rule: ViewRule) => void
  /** 正在发的规则（`node:id`）：按钮禁用。 */
  busy: ReadonlySet<string>
}

export const ruleKey = (r: Pick<ViewRule, 'node_id' | 'id'>): string => `${r.node_id}:${r.id}`

export function useRuleOps({ onSaved, onRemoved, hold, release }: { onSaved?: (rule: ViewRule | null) => void; onRemoved?: (rule: ViewRule) => void; /** 在这里处理的（「待处理」用）：改状态 / 改完确认发出去之前标记，失败了撤掉。 */ hold?: (rule: ViewRule) => void; release?: (rule: ViewRule) => void } = {}): { ops: RuleOps; dialogs: ReactNode } {
  const { t } = useTranslation()
  const [busy, setBusy] = useState<ReadonlySet<string>>(new Set())
  const [editor, setEditor] = useState<{ mode: EditorMode } | null>(null)
  const [removing, setRemoving] = useState<ViewRule | null>(null)
  const [removeBusy, setRemoveBusy] = useState(false)
  const [removeFail, setRemoveFail] = useState<RuleFail | null>(null)

  const mark = (k: string, on: boolean): void =>
    setBusy((s) => {
      const n = new Set(s)
      if (on) n.add(k)
      else n.delete(k)
      return n
    })

  const ops = useMemo<RuleOps>(
    () => ({
      busy,
      setStatus: async (rule, status) => {
        if (rule.status === 'pending' && status === 'active' && groupPeers(rule.node_id).length > 0) {
          setEditor({ mode: { kind: 'edit', rule, confirm: true } })
          return false
        }
        const k = ruleKey(rule)
        mark(k, true)
        hold?.(rule)
        const res = await updateRule(rule.node_id, rule.id, { status }, rule.updated_ms)
        mark(k, false)
        if (!res.ok) {
          release?.(rule)
          toast.error(failText(t, res.fail))
          return false
        }
        return true
      },
      restore: async (rule) => {
        const k = ruleKey(rule)
        mark(k, true)
        const res = await updateRule(rule.node_id, rule.id, { stale: false }, rule.updated_ms)
        mark(k, false)
        if (!res.ok) {
          toast.error(failText(t, res.fail))
          return false
        }
        return true
      },
      edit: (rule, confirm) => setEditor({ mode: { kind: 'edit', rule, confirm } }),
      add: (nodeIds, project) => setEditor({ mode: { kind: 'add', nodeIds, project } }),
      remove: (rule) => {
        setRemoveFail(null)
        setRemoving(rule)
      },
    }),
    [busy, t, hold, release],
  )

  const dialogs = (
    <>
      {editor ? (
        <RuleEditor
          open
          onOpenChange={(o) => {
            if (!o) setEditor(null)
          }}
          mode={editor.mode}
          onSaving={editor.mode.kind === 'edit' && editor.mode.confirm ? () => hold?.((editor.mode as { rule: ViewRule }).rule) : undefined}
          onSaveFailed={editor.mode.kind === 'edit' && editor.mode.confirm ? () => release?.((editor.mode as { rule: ViewRule }).rule) : undefined}
          onSaved={() => onSaved?.(editor.mode.kind === 'edit' ? editor.mode.rule : null)}
        />
      ) : null}
      {removing ? (
        <Dialog open onOpenChange={(o) => !o && !removeBusy && setRemoving(null)} title={t('rules.delete.title')} description={t('rules.delete.body')} testId="rule-delete-dialog" closeLabel={t('common.close')}>
          <div className="space-y-3 text-sm">
            <div className="rounded-md border border-border bg-bg-subtle p-2.5">
              <RuleText text={removing.text} testId="rule-delete-text" />
            </div>
            {removeFail ? (
              <p className="text-danger" role="alert" data-testid="rule-delete-error">
                {failText(t, removeFail)}
              </p>
            ) : null}
            <div className="flex justify-end gap-2">
              <Button onClick={() => setRemoving(null)} disabled={removeBusy} data-testid="rule-delete-cancel">
                {t('common.cancel')}
              </Button>
              <Button
                variant="danger"
                disabled={removeBusy}
                data-testid="rule-delete-confirm"
                onClick={() => {
                  const r = removing
                  setRemoveBusy(true)
                  void deleteRule(r.node_id, r.id, r.updated_ms).then((res) => {
                    setRemoveBusy(false)
                    if (res.ok) {
                      setRemoving(null)
                      onRemoved?.(r)
                    } else {
                      setRemoveFail(res.fail)
                      // 别处改过它：显示现在的版本，用户看过再点一次
                      if (res.fail.kind === 'changed' && res.fail.current) setRemoving({ ...res.fail.current, node_id: r.node_id })
                    }
                  })
                }}
              >
                {t('rules.delete.confirm')}
              </Button>
            </div>
          </div>
        </Dialog>
      ) : null}
    </>
  )
  return { ops, dialogs }
}
