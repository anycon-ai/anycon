import { useEffect, useMemo, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { Bot, Pencil } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Markdown } from '@/components/Markdown'
import { formatDateTime } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { Input, Select, Textarea } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { Chip } from '@/ui/Misc'
import { setTodoState } from '../actions'
import { deleteTodo, discardOp, retryOp, updateTodo } from '../service'
import { useTodos } from '../store'
import { isClosed, UI_STATES, type UpdateFields, type ViewTodo } from '../types'
import { cleanOneLine, type FieldError } from '../validate'
import { dueFromInput } from './CaptureForm'
import { fieldErrorText, outboxErrorText } from './errors'

/** 毫秒 → `datetime-local` 的值（本地时间，到分钟）。 */
export function dueToInput(ms: number | null | undefined): string {
  if (ms == null) return ''
  const d = new Date(ms)
  const p = (n: number): string => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`
}

/**
 * 单项详情（桌面在右半边，手机是二级页，spec/web.md § 4.12）：标题和正文可编辑（不编辑时正文按 Markdown 渲染，和助手消息同一条
 * 消毒管线，从不当 HTML）、项目、期望时间、状态（打开 / 处理中 / 完成 / 不做了）、删除（二次确认）；agent 记的项有回到来源会话的链接。
 * 这一步没有「让 agent 处理」「交给分身」。
 */
export function TodoDetail({ item, online, onDeleted }: { item: ViewTodo; online: boolean; onDeleted?: () => void }) {
  const { t, i18n } = useTranslation()
  const nodeName = useNodes((s) => s.byId[item.node_id]?.name ?? item.node_id.slice(0, 8))
  const order = useNodes((s) => s.order)
  const sessionKnown = useSessions((s) => (item.source.session_id ? !!s.list[item.source.session_id] : false))
  const outbox = useTodos((s) => s.outbox)
  const failedOps = useMemo(() => outbox.filter((r) => r.node_id === item.node_id && r.id === item.id && r.state === 'failed'), [outbox, item.node_id, item.id])
  const [editing, setEditing] = useState(false)
  const [title, setTitle] = useState(item.title)
  const [body, setBody] = useState(item.body)
  const [project, setProject] = useState(item.project ?? '')
  const [due, setDue] = useState(dueToInput(item.due_ms))
  const [error, setError] = useState<FieldError | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)

  // 换了一项、或者别处改了它（`todo.changed`）而这里没在编辑：表单跟着
  useEffect(() => {
    if (editing) return
    setTitle(item.title)
    setBody(item.body)
    setProject(item.project ?? '')
    setDue(dueToInput(item.due_ms))
  }, [item.id, item.title, item.body, item.project, item.due_ms, editing])

  const save = async (): Promise<void> => {
    const fields: UpdateFields = {}
    if (title.trim() !== item.title) fields.title = title
    if (body !== item.body) fields.body = body
    const p = project.trim()
    if (p !== (item.project ?? '')) {
      if (p) fields.project = p
      else fields.clear = ['project']
    }
    const d = dueFromInput(due)
    if ((d ?? null) !== (item.due_ms ?? null)) {
      if (d != null) fields.due_ms = d
      else fields.clear = [...(fields.clear ?? []), 'due_ms']
    }
    if (Object.keys(fields).length > 0) {
      const r = await updateTodo(item.node_id, item.id, fields)
      if (!r.ok) {
        setError(r.error)
        return
      }
    }
    setError(null)
    setEditing(false)
  }

  const states = item.state === 'waiting' || item.state === 'review' ? [...UI_STATES, item.state] : UI_STATES
  return (
    <div className="space-y-4" data-testid="todo-detail" data-id={item.id}>
      <div className="flex flex-wrap items-center gap-2">
        {item.source.kind === 'agent' ? (
          <Chip tone="info" icon={<Bot className="size-3" aria-hidden />}>
            {t('todos.byAgent')}
          </Chip>
        ) : null}
        {item.failed ? <Chip tone="danger">{t('todos.notSaved')}</Chip> : item.unsynced && !online ? <Chip tone="warning">{t('todos.notSynced')}</Chip> : null}
        <span className="text-xs text-fg-subtle">{nodeName}</span>
      </div>

      {editing ? (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault()
            void save()
          }}
        >
          <label className="block space-y-1 text-sm font-medium">
            {t('todos.fieldTitle')}
            <Input value={title} onChange={(e) => setTitle(e.target.value)} data-testid="todo-edit-title" />
          </label>
          <label className="block space-y-1 text-sm font-medium">
            {t('todos.fieldBody')}
            <Textarea rows={8} value={body} onChange={(e) => setBody(e.target.value)} data-testid="todo-edit-body" />
          </label>
          <div className="flex flex-wrap gap-3">
            <label className="space-y-1 text-sm font-medium">
              {t('todos.project')}
              <Input value={project} onChange={(e) => setProject(e.target.value)} className="w-44" data-testid="todo-edit-project" />
            </label>
            <label className="space-y-1 text-sm font-medium">
              {t('todos.due')}
              <Input type="datetime-local" value={due} onChange={(e) => setDue(e.target.value)} className="w-auto" data-testid="todo-edit-due" />
            </label>
          </div>
          {error ? (
            <p role="alert" className="text-sm text-danger" data-testid="todo-edit-error">
              {fieldErrorText(t, error)}
            </p>
          ) : null}
          <div className="flex gap-2">
            <Button type="submit" variant="primary" data-testid="todo-save">
              {t('common.save')}
            </Button>
            <Button
              onClick={() => {
                setEditing(false)
                setError(null)
              }}
            >
              {t('common.cancel')}
            </Button>
          </div>
        </form>
      ) : (
        <>
          <div className="flex items-start gap-2">
            <h2 className="min-w-0 flex-1 text-lg font-semibold [overflow-wrap:anywhere]" data-testid="todo-title">
              {cleanOneLine(item.title)}
            </h2>
            <Button size="sm" onClick={() => setEditing(true)} icon={<Pencil className="size-3.5" aria-hidden />} data-testid="todo-edit">
              {t('todos.edit')}
            </Button>
          </div>
          {item.body ? (
            <div className="rounded-md border border-border p-3" data-testid="todo-body">
              <Markdown text={item.body} />
            </div>
          ) : (
            <p className="text-sm text-fg-subtle">{t('todos.noBody')}</p>
          )}
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
            <dt className="text-fg-subtle">{t('todos.project')}</dt>
            <dd>{item.project ? cleanOneLine(item.project) : t('todos.none')}</dd>
            <dt className="text-fg-subtle">{t('todos.due')}</dt>
            <dd>{item.due_ms ? formatDateTime(item.due_ms, i18n.language) : t('todos.none')}</dd>
          </dl>
        </>
      )}

      <div className="flex flex-wrap items-center gap-3">
        <label className="flex items-center gap-2 text-sm font-medium">
          {t('todos.state')}
          <Select value={item.state} onChange={(e) => void setTodoState(item, e.target.value as ViewTodo['state'], { undo: false })} className="w-auto" data-testid="todo-state">
            {states.map((s) => (
              <option key={s} value={s}>
                {t(`todos.states.${s}`)}
              </option>
            ))}
          </Select>
        </label>
        {isClosed(item.state) ? (
          <Button onClick={() => void setTodoState(item, 'open')} data-testid="todo-reopen">
            {t('todos.reopen')}
          </Button>
        ) : (
          <Button variant="primary" onClick={() => void setTodoState(item, 'done')} data-testid="todo-done">
            {t('todos.done')}
          </Button>
        )}
        <Button variant="outline-danger" onClick={() => setConfirmDelete(true)} data-testid="todo-delete">
          {t('todos.delete')}
        </Button>
      </div>

      {item.source.kind === 'agent' ? (
        <p className="text-sm text-fg-muted" data-testid="todo-source">
          {item.source.session_id ? (
            sessionKnown ? (
              <Link to="/n/$node/s/$session" params={{ node: shortNodeId(item.node_id, order), session: item.source.session_id }} className="font-medium text-accent-text hover:underline">
                {t('todos.fromSession')}
              </Link>
            ) : (
              t('todos.sessionGone')
            )
          ) : (
            t('todos.byAgentNoSession')
          )}
        </p>
      ) : null}

      {failedOps.map((r) => (
        <div key={r.seq} className="space-y-2 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm" role="alert" data-testid="todo-failed-op">
          <p>{r.error ? outboxErrorText(t, r.error) : t('todos.errors.rejected')}</p>
          <div className="flex gap-2">
            <Button size="sm" onClick={() => void retryOp(r)} data-testid="todo-retry">
              {t('todos.retry')}
            </Button>
            <Button size="sm" variant="outline-danger" onClick={() => void discardOp(r)} data-testid="todo-discard">
              {t('todos.discard')}
            </Button>
          </div>
        </div>
      ))}

      <Dialog open={confirmDelete} onOpenChange={setConfirmDelete} title={t('todos.deleteTitle')} description={t('todos.deleteBody', { title: cleanOneLine(item.title, 80) })} closeLabel={t('common.close')} testId="todo-delete-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirmDelete(false)}>{t('common.cancel')}</Button>
          <Button
            variant="danger"
            onClick={() => {
              setConfirmDelete(false)
              void deleteTodo(item.node_id, item.id).then(() => onDeleted?.())
            }}
            data-testid="todo-delete-confirm"
          >
            {t('todos.delete')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}
