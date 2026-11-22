// 模型写的问题和选项只作为纯文字；待处理和「经过」共用回答表单。
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TwinQuestion } from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Textarea } from '@/ui/Field'
import { describeError } from '@/lib/errors'
import { AlwaysDoThis } from '@/features/rules/ui/AlwaysDoThis'
import { openAfterTwinAnswer } from '@/features/rules/followUp'
import { answerTwin } from './service'

export function TwinQuestionForm({ nodeId, question, armed = true }: { nodeId: string; question: TwinQuestion; armed?: boolean }) {
  const { t } = useTranslation()
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const [answer, setAnswer] = useState('')
  const [always, setAlways] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const submit = async (text: string): Promise<void> => {
    if (busy || !text.trim() || new TextEncoder().encode(text).length > 2048) return
    setBusy(true)
    setError('')
    try {
      await answerTwin(nodeId, { id: question.id, answer: text })
      setAnswer('')
      // 答复成功之后才打开「记成规则」的编辑框；失败走下面的 catch，什么都不打开
      if (always) openAfterTwinAnswer(nodeId, question.id, question.question, text)
      setAlways(false)
    } catch (e) {
      setError(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  const disabled = busy || !online || !armed
  return (
    <div className="space-y-2" data-testid="twin-question">
      <p className="text-xs text-fg-subtle">{t('twin.agentWritten')}</p>
      <p className="whitespace-pre-wrap break-words">{question.question}</p>
      {question.status === 'open' ? (
        <>
          <AlwaysDoThis nodeId={nodeId} checked={always} onChange={setAlways} />
          <div className="flex flex-wrap gap-2">
            {(question.options ?? []).map((option, i) => (
              <Button key={i} disabled={disabled} onClick={() => void submit(option)}>
                {option}
              </Button>
            ))}
          </div>
          <form
            className="space-y-2"
            onSubmit={(e) => {
              e.preventDefault()
              void submit(answer)
            }}
          >
            <Textarea aria-label={t('twin.answer')} value={answer} onChange={(e) => setAnswer(e.target.value)} disabled={busy || !online} />
            {new TextEncoder().encode(answer).length > 2048 ? <p role="alert">{t('twin.answerLimit')}</p> : null}
            <Button type="submit" disabled={disabled || !answer.trim() || new TextEncoder().encode(answer).length > 2048}>
              {t('twin.answer')}
            </Button>
          </form>
          {!online ? <p>{t('twin.offline')}</p> : null}
        </>
      ) : (
        <p className="whitespace-pre-wrap">{question.status === 'answered' ? question.answer : t('twin.questionVoid')}</p>
      )}
      {error ? (
        <p role="alert" className="text-danger">
          {error}
        </p>
      ) : null}
    </div>
  )
}
