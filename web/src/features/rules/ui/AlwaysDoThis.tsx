// 「以后都这样」（docs/spec/daemon.md § 23.6，spec/web.md § 14.6）：回答分身的问题和 agent 的提问时，答复按钮旁边的一个勾选。
// 它只是一个本地的勾，不改答复的内容，也不碰答复按钮的锁和键盘快捷键（⌘⇧A / ⌘⇧D）；勾上并且答复成功之后，才打开「记成规则」
// 的同一个编辑框（`FollowUpRuleHost`）。预填的文字里有 agent / 模型写的东西，所以只能经过编辑框、由用户看过再存；预填原样不能保存。
// 节点不支持规则时不出现。
import { useTranslation } from 'react-i18next'
import { useNodes } from '@/stores/nodes'
import { nodeSupportsRules } from '../store'
import { useFollowUp } from '../followUp'
import { RuleEditor } from './RuleEditor'

export function AlwaysDoThis({ nodeId, checked, onChange }: { nodeId: string; checked: boolean; onChange: (v: boolean) => void }) {
  const { t } = useTranslation()
  const supported = useNodes((s) => s.byId[nodeId]?.info?.capabilities.rules === true)
  if (!supported || !nodeSupportsRules(nodeId)) return null
  return (
    <label className="flex min-h-8 cursor-pointer items-center gap-2 text-sm text-fg-muted max-md:min-h-11" title={t('rules.always.hint')} data-testid="always-do-this">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} className="size-4 accent-[var(--accent)] [@media(pointer:coarse)]:size-5" data-testid="always-do-this-box" />
      {t('rules.always.label')}
    </label>
  )
}

/** 勾了「以后都这样」、答复成功之后的编辑框。挂在应用外壳里，不随提问的卡片一起消失。 */
export function FollowUpRuleHost() {
  const { t } = useTranslation()
  const current = useFollowUp((s) => s.current)
  const close = useFollowUp((s) => s.close)
  if (!current) return null
  const initial = current.answers.map((c) => t('rules.record.prefillQuestion', { question: c.question, answer: c.answer })).join('\n')
  return (
    <RuleEditor
      open
      onOpenChange={(o) => {
        if (!o) close()
      }}
      mode={{
        kind: 'add',
        nodeIds: [current.nodeId],
        ...(current.project ? { project: current.project } : {}),
        text: initial,
        source: current.source,
        title: t('rules.always.title'),
        note: t('rules.record.note'),
        unchangedFrom: () => initial,
        renderContext: () => (
          <div className="space-y-3" data-testid="record-context">
            <div className="space-y-1 rounded-md border border-dashed border-border-strong bg-bg-subtle p-2.5" data-testid="record-agent">
              <p className="text-xs font-medium text-fg-muted">{t('rules.record.agentWrote')}</p>
              {current.asked.map((l, i) => (
                <p key={i} className="text-sm whitespace-pre-wrap [overflow-wrap:anywhere]">
                  {l}
                </p>
              ))}
            </div>
            <div className="space-y-1.5 rounded-md border border-border bg-surface p-2.5" data-testid="record-choice">
              <p className="text-xs font-medium text-fg-muted">{t('rules.record.yourChoice')}</p>
              {current.answers.length > 0 ? (
                <ul className="space-y-0.5 text-sm" data-testid="record-answers">
                  {current.answers.map((c, i) => (
                    <li key={i} className="[overflow-wrap:anywhere]">
                      {c.answer}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-sm text-fg-subtle" data-testid="record-no-answer">
                  {t('rules.record.noAnswer')}
                </p>
              )}
            </div>
          </div>
        ),
      }}
    />
  )
}
