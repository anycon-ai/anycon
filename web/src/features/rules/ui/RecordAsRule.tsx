// 「记成规则」（docs/spec/daemon.md § 23.6，spec/web.md § 4.14）：权限请求和提问的卡片上，答复按钮旁边的一个小按钮。
// 打开一个编辑框，里面预填了这次的问题和用户的选择；agent 写的话和用户的选择分开显示，文字用户改成自己的话再存（`active`，
// 出处是这一次的会话和请求）。它不答复请求、不碰答复按钮和它们的解锁、不在键盘快捷键（⌘⇧A / ⌘⇧D）的路径上；编辑框里写明规则不批准任何东西。
// 预填的文字里有 agent 写的东西，所以只能经过编辑框、由用户看过再存，没有「一键记下」。
import { useRef, useState } from 'react'
import { ScrollText } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PermissionBlock } from '@/features/session/reducer'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { agentLines, chosenAnswers, whatWasAsked } from '../recordAsRule'
import { nodeSupportsRules } from '../store'
import { RuleEditor } from './RuleEditor'

export function RecordAsRule({ nodeId, sessionId, block, answers }: { nodeId: string; sessionId: string; block: PermissionBlock; answers: Readonly<Record<string, string | string[]>> }) {
  const { t } = useTranslation()
  const supported = useNodes((s) => s.byId[nodeId]?.info?.capabilities.rules === true)
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const project = useSessions((s) => s.list[sessionId]?.project)
  const [open, setOpen] = useState(false)
  const [choice, setChoice] = useState<string | null>(null)
  const lastPrefill = useRef('')
  if (!supported || !nodeSupportsRules(nodeId)) return null

  const isQuestion = block.kind === 'question'
  const prefillFor = (optionId: string | null): string => {
    if (isQuestion) {
      return chosenAnswers(block, answers)
        .map((c) => t('rules.record.prefillQuestion', { question: c.question, answer: c.answer }))
        .join('\n')
    }
    const opt = block.options.find((o) => o.id === optionId)
    return opt ? t('rules.record.prefillOption', { what: whatWasAsked(block), choice: opt.label }) : ''
  }
  const initial = prefillFor(choice)

  return (
    <>
      <Button
        type="button"
        size="sm"
        variant="outline"
        disabled={!online}
        title={online ? undefined : t('rules.errors.offline')}
        onClick={() => {
          lastPrefill.current = initial
          setOpen(true)
        }}
        data-testid="record-rule"
        className="max-md:min-h-11"
      >
        <ScrollText className="size-3.5" aria-hidden />
        {t('rules.record.button')}
      </Button>
      {open ? (
        <RuleEditor
          open
          onOpenChange={setOpen}
          mode={{
            kind: 'add',
            nodeIds: [nodeId],
            ...(project ? { project } : {}),
            text: initial,
            source: { kind: 'answer', session_id: sessionId, request_id: block.requestId },
            title: t('rules.record.title'),
            note: t('rules.record.note'),
            renderContext: ({ text, setText }) => (
              <div className="space-y-3" data-testid="record-context">
                <div className="space-y-1 rounded-md border border-dashed border-border-strong bg-bg-subtle p-2.5" data-testid="record-agent">
                  <p className="text-xs font-medium text-fg-muted">{t('rules.record.agentWrote')}</p>
                  {agentLines(block).map((l, i) => (
                    <p key={i} className="text-sm whitespace-pre-wrap [overflow-wrap:anywhere]">
                      {l}
                    </p>
                  ))}
                </div>
                <div className="space-y-1.5 rounded-md border border-border bg-surface p-2.5" data-testid="record-choice">
                  <p className="text-xs font-medium text-fg-muted">{t('rules.record.yourChoice')}</p>
                  {isQuestion ? (
                    chosenAnswers(block, answers).length > 0 ? (
                      <ul className="space-y-0.5 text-sm" data-testid="record-answers">
                        {chosenAnswers(block, answers).map((c, i) => (
                          <li key={i} className="[overflow-wrap:anywhere]">
                            {c.answer}
                          </li>
                        ))}
                      </ul>
                    ) : (
                      <p className="text-sm text-fg-subtle" data-testid="record-no-answer">
                        {t('rules.record.noAnswer')}
                      </p>
                    )
                  ) : (
                    <div role="radiogroup" aria-label={t('rules.record.yourChoice')} className="space-y-1">
                      {block.options.map((o) => (
                        <label key={o.id} className="flex min-h-8 cursor-pointer items-center gap-2 text-sm">
                          <input
                            type="radio"
                            name={`record-choice-${block.requestId}`}
                            checked={choice === o.id}
                            onChange={() => {
                              setChoice(o.id)
                              const next = prefillFor(o.id)
                              // 用户已经改过文字就不再替他换预填的那句
                              if (text === lastPrefill.current || text.trim() === '') setText(next)
                              lastPrefill.current = next
                            }}
                            className="size-4 accent-[var(--accent)]"
                            data-testid={`record-choice-${o.id}`}
                          />
                          {o.label}
                        </label>
                      ))}
                    </div>
                  )}
                  <p className="text-xs text-fg-subtle">{t('rules.record.notAnswered')}</p>
                </div>
              </div>
            ),
          }}
        />
      ) : null}
    </>
  )
}
