import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react'
import { Send } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useIsTouch } from '@/lib/useMediaQuery'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input, Select, Textarea } from '@/ui/Field'
import { useCapture } from '../capture'
import { addTodo, deleteTodo } from '../service'
import { useSupportingNodes } from '../store'
import { parseCapture, type FieldError } from '../validate'
import { fieldErrorText } from './errors'

/** 本地时间的 `datetime-local` 值 → 毫秒；空 / 不合法 → undefined。 */
export function dueFromInput(v: string): number | undefined {
  if (!v) return undefined
  const ms = new Date(v).getTime()
  return Number.isFinite(ms) ? ms : undefined
}

/**
 * 记一项的输入条（spec/web.md § 14.5）：一个输入框，第一行是标题、换行之后是正文；上面一排可选的小项（节点、项目、期望时间）都有缺省值。
 * 发送之后输入框清空、条留着，可以接着记；提示里有「撤销」（撤销 = 删掉刚记的那一项，离线时也照常，走待发队列）。
 * 节点离线照常能记。多个节点支持清单时写明会记到哪个节点。
 */
export function CaptureForm({ autoFocus = true, onEscape, compact }: { autoFocus?: boolean; onEscape?: () => void; compact?: boolean }) {
  const { t } = useTranslation()
  const text = useCapture((s) => s.text)
  const setText = useCapture((s) => s.setText)
  const focusTick = useCapture((s) => s.focusTick)
  const { selected } = useNodeFilter()
  const supporting = useSupportingNodes(selected)
  const lastNode = usePrefs((s) => s.lastTodoNode)
  const setPrefs = usePrefs((s) => s.set)
  const nodes = useNodes((s) => s.byId)
  const touch = useIsTouch()
  const area = useRef<HTMLTextAreaElement>(null)
  const [pickedNode, setPickedNode] = useState<string | null>(null)
  const [project, setProject] = useState('')
  const [due, setDue] = useState('')
  const [error, setError] = useState<FieldError | null>(null)

  // 缺省节点：选中的、支持清单的节点里上次用过的，没有就第一个
  const nodeId = (pickedNode && supporting.includes(pickedNode) ? pickedNode : null) ?? (lastNode && supporting.includes(lastNode) ? lastNode : null) ?? supporting[0] ?? null
  const projects = useMemo(() => (nodeId ? (nodes[nodeId]?.projects ?? []).map((p) => p.name) : []), [nodes, nodeId])

  useEffect(() => {
    if (autoFocus) area.current?.focus()
  }, [autoFocus, focusTick])

  const send = async (): Promise<void> => {
    if (!nodeId) return
    const { title, body } = parseCapture(text)
    const r = await addTodo(nodeId, { title, body, project: project.trim() || null, due_ms: dueFromInput(due) ?? null })
    if (!r.ok) {
      setError(r.error)
      return
    }
    setError(null)
    setPrefs({ lastTodoNode: nodeId })
    setText('')
    setProject('')
    setDue('')
    toast.info(t('todos.added', { title: title.length > 60 ? `${[...title].slice(0, 60).join('')}…` : title }), { label: t('todos.undo'), run: () => void deleteTodo(nodeId, r.id) })
    area.current?.focus()
  }

  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (e.key === 'Escape' && onEscape) {
      e.preventDefault()
      onEscape()
    } else if (e.key === 'Enter' && !e.shiftKey && !touch && !e.nativeEvent.isComposing) {
      e.preventDefault()
      void send()
    }
  }

  if (supporting.length === 0) {
    return (
      <p className="text-sm text-fg-muted" role="status" data-testid="capture-unsupported">
        {t('todos.noSupport')}
      </p>
    )
  }

  const canSend = text.trim() !== ''
  return (
    <form
      className="space-y-2"
      onSubmit={(e) => {
        e.preventDefault()
        void send()
      }}
      data-testid="capture-form"
    >
      <div className="flex items-end gap-2">
        <Textarea
          ref={area}
          rows={compact || !text.includes('\n') ? 1 : Math.min(6, text.split('\n').length)}
          value={text}
          onChange={(e) => {
            setText(e.target.value)
            setError(null)
          }}
          onKeyDown={onKey}
          placeholder={t('todos.capturePlaceholder')}
          aria-label={t('todos.addTitle')}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? 'capture-error' : undefined}
          className="min-h-11 flex-1 resize-none py-2.5"
          data-testid="capture-input"
        />
        <Button type="submit" variant="primary" disabled={!canSend} aria-label={t('todos.send')} className="size-11 shrink-0 !p-0" data-testid="capture-send">
          <Send className="size-4" aria-hidden />
        </Button>
      </div>
      {error ? (
        <p id="capture-error" role="alert" className="text-sm text-danger" data-testid="capture-error">
          {fieldErrorText(t, error)}
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-2 text-xs text-fg-subtle" data-testid="capture-options">
        {supporting.length > 1 ? (
          <label className="flex items-center gap-1.5">
            <span>{t('todos.toNode')}</span>
            <Select value={nodeId ?? ''} onChange={(e) => setPickedNode(e.target.value)} className="!h-9 w-auto max-w-40" data-testid="capture-node">
              {supporting.map((id) => (
                <option key={id} value={id}>
                  {nodes[id]?.name ?? id.slice(0, 8)}
                </option>
              ))}
            </Select>
          </label>
        ) : null}
        <label className="flex items-center gap-1.5">
          <span>{t('todos.project')}</span>
          <Input value={project} onChange={(e) => setProject(e.target.value)} list="capture-projects" className="!h-9 w-32" placeholder={t('todos.none')} data-testid="capture-project" />
          <datalist id="capture-projects">
            {projects.map((p) => (
              <option key={p} value={p} />
            ))}
          </datalist>
        </label>
        <label className="flex items-center gap-1.5">
          <span>{t('todos.due')}</span>
          <Input type="datetime-local" value={due} onChange={(e) => setDue(e.target.value)} className="!h-9 w-auto" data-testid="capture-due" />
        </label>
      </div>
      {supporting.length > 1 && nodeId ? (
        <p className="text-xs text-fg-subtle" data-testid="capture-target">
          {t('todos.willGoTo', { node: nodes[nodeId]?.name ?? nodeId.slice(0, 8) })}
        </p>
      ) : null}
    </form>
  )
}
