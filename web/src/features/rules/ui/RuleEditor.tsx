// 写一条规则、改一条规则、改完直接确认（spec/web.md § 4.14）。文字框里是用户自己的话：本地校验和 daemon 同一套（1–600 个码点、
// 没有控制字符、没有看不见的字符），超出注入字数上限时说明字节数和怎么办。改已有的规则带着看到它时的版本（`if_updated_ms`）：
// 别的设备在这期间改过它就不覆盖，显示现在的版本，用户看过之后再存。
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Field, Select, Textarea } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { addRule, updateRule, type NewRule, type RuleFail } from '../service'
import { useRules, type ViewRule } from '../store'
import { codePoints, newRuleId, normalizeRuleText, ruleTextError, TEXT_MAX } from '../validate'
import { failText, textErrorText } from './errors'
import { groupPeers } from '../groupSync'
import { RuleText } from './RuleText'

export type EditorMode =
  /** 新写一条：从 `nodeIds` 里选节点（只有一个就不问），`project` 是预选的范围。 */
  | {
      kind: 'add'
      nodeIds: readonly string[]
      project?: string
      text?: string
      /** 「记成规则」：出处（会话和请求）。 */
      source?: NewRule['source']
      title?: string
      /** 说明（缺省是「规则只是告诉 agent 你的做法」那句）。 */
      note?: string
      /** 文字框上面的一块（「记成规则」里：agent 写的话、用户的选择）；`setText` 可以改文字框的内容。 */
      /** 返回现在预填的那句话：文字还和它一样（没有改过）就不让存，要用户自己改成自己的话（「记成规则」）。 */
      unchangedFrom?: () => string
      renderContext?: (api: { text: string; setText: (t: string) => void }) => ReactNode
    }
  /** 改一条已有的；`confirm`：存的时候同时确认（状态改成生效）。 */
  | { kind: 'edit'; rule: ViewRule; confirm: boolean }

const ALL = ''

export function RuleEditor({ open, onOpenChange, mode, onSaved, onSaving, onSaveFailed }: { open: boolean; onOpenChange: (o: boolean) => void; mode: EditorMode; onSaved?: () => void; /** 发出去之前 / 被拒绝之后（「待处理」据此标记「在这里处理的」）。 */ onSaving?: () => void; onSaveFailed?: () => void }) {
  const { t } = useTranslation()
  const initial = mode.kind === 'edit' ? mode.rule : null
  const [nodeId, setNodeId] = useState(mode.kind === 'edit' ? mode.rule.node_id : (mode.nodeIds[0] ?? ''))
  const [text, setText] = useState(mode.kind === 'edit' ? mode.rule.text : (mode.text ?? ''))
  const [sync, setSync] = useState<'node' | 'group'>(mode.kind === 'edit' && !mode.confirm && mode.rule.sync === 'group' ? 'group' : 'node')
  const [project, setProject] = useState(mode.kind === 'edit' ? (mode.rule.project ?? ALL) : (mode.project ?? ALL))
  // 看到这条规则时的版本：别处改了它之后，用户看过现在的样子再存，就以现在的版本为准
  const [base, setBase] = useState<ViewRule | null>(initial)
  const [fail, setFail] = useState<RuleFail | null>(null)
  const [busy, setBusy] = useState(false)
  const idRef = useRef(newRuleId())
  const entry = useNodes((s) => s.byId[nodeId])
  const nodes = useNodes((s) => s.byId)
  const hasPeers = groupPeers(nodeId).length > 0
  const projectLocked = project !== ALL || !!initial?.project
  const effectiveSync = projectLocked ? 'node' : sync
  const online = entry?.conn === 'ready'
  const budget = useRules((s) => s.nodes[nodeId]?.budget)

  useEffect(() => {
    if (!open) return
    setNodeId(mode.kind === 'edit' ? mode.rule.node_id : (mode.nodeIds[0] ?? ''))
    setText(mode.kind === 'edit' ? mode.rule.text : (mode.text ?? ''))
    setProject(mode.kind === 'edit' ? (mode.rule.project ?? ALL) : (mode.project ?? ALL))
    setBase(mode.kind === 'edit' ? mode.rule : null)
    setSync(mode.kind === 'edit' && !mode.confirm && mode.rule.sync === 'group' ? 'group' : 'node')
    setFail(null)
    idRef.current = newRuleId()
    // 只在打开的那一刻重置
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const projects = useMemo(() => {
    const names = (entry?.projects ?? []).filter((p) => p.missing !== true).map((p) => p.name)
    if (project !== ALL && !names.includes(project)) names.push(project)
    return names
  }, [entry?.projects, project])

  const trimmed = normalizeRuleText(text)
  const localError = ruleTextError(text)
  const count = codePoints(trimmed)
  const editing = mode.kind === 'edit'
  const gone = fail?.kind === 'changed' && fail.current === null
  const changed = base ? effectiveSync !== base.sync || trimmed !== base.text || (project === ALL ? null : project) !== (base.project ?? null) : true
  const confirming = mode.kind === 'edit' && mode.confirm
  const untouched = mode.kind === 'add' && !!mode.unchangedFrom && trimmed !== '' && trimmed === normalizeRuleText(mode.unchangedFrom())
  const canSave = online && !busy && !gone && localError === null && !untouched && (changed || confirming)

  const save = async (): Promise<void> => {
    if (!canSave) return
    setBusy(true)
    setFail(null)
    onSaving?.()
    const res =
      mode.kind === 'add'
        ? await addRule(nodeId, { id: idRef.current, text: trimmed, project: project === ALL ? null : project, sync: effectiveSync, status: 'active', ...(mode.source ? { source: mode.source } : {}) })
        : await updateRule(
            nodeId,
            mode.rule.id,
            {
              ...(trimmed !== base!.text ? { text: trimmed } : {}),
              ...((project === ALL ? null : project) !== (base!.project ?? null) ? { project: project === ALL ? null : project } : {}),
              ...(mode.confirm ? { status: 'active' as const } : {}),
              ...(effectiveSync !== base!.sync ? { sync: effectiveSync } : {}),
            },
            base!.updated_ms,
          )
    setBusy(false)
    if (res.ok) {
      onOpenChange(false)
      onSaved?.()
      return
    }
    onSaveFailed?.()
    setFail(res.fail)
    // 别处改过：以现在的版本为准（用户的草稿还在），看过再存
    if (res.fail.kind === 'changed' && res.fail.current) setBase({ ...res.fail.current, node_id: nodeId })
  }

  const title = mode.kind === 'add' ? (mode.title ?? t('rules.editor.addTitle')) : mode.confirm ? t('rules.editor.confirmTitle') : t('rules.editor.editTitle')
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={title} description={mode.kind === 'add' && mode.note ? mode.note : t('rules.editor.note')} size="lg" testId="rule-editor" closeLabel={t('common.close')}>
      <form
        className="space-y-3 text-sm"
        // 回车（在单选框、下拉里）不提交：只有「保存」按钮能存
        onSubmit={(e) => e.preventDefault()}
      >
        {mode.kind === 'add' && mode.nodeIds.length > 1 ? (
          <Field label={t('rules.editor.node')} htmlFor="rule-node">
            <Select id="rule-node" value={nodeId} onChange={(e) => setNodeId(e.target.value)} data-testid="rule-editor-node">
              {mode.nodeIds.map((id) => (
                <option key={id} value={id}>
                  {cleanHintText(nodes[id]?.name) || id.slice(0, 8)}
                </option>
              ))}
            </Select>
          </Field>
        ) : null}
        {mode.kind === 'add' && mode.renderContext ? mode.renderContext({ text, setText }) : null}
        <Field
          label={t('rules.editor.text')}
          htmlFor="rule-text-input"
          error={text !== '' && localError && localError !== 'empty' ? textErrorText(t, localError) : undefined}
          hint={t('rules.editor.textHint')}
        >
          <Textarea id="rule-text-input" rows={6} value={text} onChange={(e) => setText(e.target.value)} aria-invalid={localError !== null && localError !== 'empty'} data-testid="rule-editor-text" autoFocus />
          <p className="text-xs text-fg-subtle" data-testid="rule-editor-count" aria-live="off">
            {t('rules.editor.count', { count, max: TEXT_MAX })}
          </p>
        </Field>
        <Field label={t('rules.editor.scope')} htmlFor="rule-scope" hint={t('rules.editor.scopeHint')}>
          <Select id="rule-scope" value={project} onChange={(e) => setProject(e.target.value)} data-testid="rule-editor-scope">
            <option value={ALL}>{t('rules.scope.node')}</option>
            {projects.map((p) => (
              <option key={p} value={p}>
                {t('rules.scope.project', { project: cleanHintText(p) || p })}
              </option>
            ))}
          </Select>
        </Field>
        {hasPeers || initial?.sync === 'group' ? (
          <Field label={t('rules.group.scope')} htmlFor="rule-sync" hint={projectLocked ? t('rules.group.projectDisabled') : t('rules.group.localNote')}>
            <Select id="rule-sync" value={effectiveSync} onChange={(e) => setSync(e.target.value === 'group' ? 'group' : 'node')} data-testid="rule-editor-sync">
              <option value="node">{t('rules.group.node')}</option>
              <option value="group" disabled={projectLocked}>{t('rules.group.group')}</option>
            </Select>
          </Field>
        ) : null}
        {budget ? <p className="text-xs text-fg-subtle">{t('rules.editor.budget', { cap: budget.cap })}</p> : null}
        {untouched ? (
          <p className="text-xs text-fg-muted" role="status" data-testid="rule-editor-edit-first">
            {t('rules.record.editFirst')}
          </p>
        ) : null}
        {confirming ? <p className="text-xs text-fg-muted">{t('rules.editor.confirmNote')}</p> : null}
        {editing && fail?.kind === 'changed' && fail.current ? (
          <div className="space-y-1.5 rounded-md border border-warning/40 bg-warning-subtle p-2.5" role="alert" data-testid="rule-editor-conflict">
            <p className="text-sm font-medium text-warning">{failText(t, fail)}</p>
            <p className="text-xs text-fg-muted">{t('rules.editor.currentVersion')}</p>
            <RuleText text={fail.current.text} testId="rule-editor-current" />
          </div>
        ) : fail ? (
          <p className="rounded-md border border-danger/40 bg-danger-subtle p-2.5 text-danger" role="alert" data-testid="rule-editor-error" data-reason={fail.kind}>
            {failText(t, fail)}
          </p>
        ) : null}
        {!online ? (
          <p className="text-fg-muted" role="status" data-testid="rule-editor-offline">
            {t('rules.errors.offline')}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={() => onOpenChange(false)} disabled={busy} data-testid="rule-editor-cancel">
            {t('common.cancel')}
          </Button>
          <Button type="button" variant="primary" disabled={!canSave} onClick={() => void save()} data-testid="rule-editor-save">
            {busy ? t('rules.editor.saving') : confirming ? t('rules.editor.saveConfirm') : t('rules.editor.save')}
          </Button>
        </div>
      </form>
    </Dialog>
  )
}
