import { useMemo, useState } from 'react'
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
import { changedElsewhere, dirtyFields, draftKey, useDrafts, type EditField, type EditValues } from '../drafts'
import { deleteTodo, discardOp, retryOp, updateTodo } from '../service'
import { useTodos } from '../store'
import { isAgentSource, isHumanSource, isOtherSource } from '../source'
import { isClosed, UI_STATES, type UpdateFields, type ViewTodo } from '../types'
import { cleanHintText } from '@/lib/hint'
import { cleanOneLine, type FieldError } from '../validate'
import { dueFromInput } from './CaptureForm'
import { activityOf, isReview } from '../activity'
import { DispatchDialog } from './DispatchDialog'
import { HandlerChip, LinkedStatus } from './ActivityBits'
import { MoveTodoDialog } from './MoveTodoDialog'
import { RepeatField, RepeatFromChip, RepeatTemplateChip, repeatInputText, repeatSummary, useNodeSupportsRepeat } from './RepeatBits'
import { repeatFromInput, repeatInputError, repeatToInput } from '../repeat'
import { TwinHandlerButton } from '@/features/twin/TodoParts'
import { TodoHistory } from './TodoHistory'
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
 */
/** 草稿里的字段值（和 `datetime-local` 的格式）：从一项现在的样子读出来。 */
export function valuesOf(item: Pick<ViewTodo, 'title' | 'body' | 'project' | 'due_ms' | 'repeat'>): EditValues {
  return { title: item.title, body: item.body, project: item.project ?? '', due: dueToInput(item.due_ms), repeat: repeatToInput(item.repeat) }
}

export function TodoDetail({ item, online, onDeleted }: { item: ViewTodo; online: boolean; onDeleted?: () => void }) {
  const { t, i18n } = useTranslation()
  const nodeName = useNodes((s) => cleanHintText(s.byId[item.node_id]?.name) || item.node_id.slice(0, 8))
  const order = useNodes((s) => s.order)
  const sessionKnown = useSessions((s) => (item.source.session_id ? !!s.list[item.source.session_id] : false))
  const outbox = useTodos((s) => s.outbox)
  const failedOps = useMemo(() => outbox.filter((r) => r.node_id === item.node_id && r.id === item.id && r.state === 'failed'), [outbox, item.node_id, item.id])
  const key = draftKey(item.node_id, item.id)
  const draft = useDrafts((s) => s.drafts[key])
  const editing = draft !== undefined
  const [error, setError] = useState<FieldError | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmDiscard, setConfirmDiscard] = useState(false)
  const [saving, setSaving] = useState(false)
  const [moving, setMoving] = useState(false)
  const [dispatching, setDispatching] = useState(false)
  const activity = activityOf(item)
  // 有别的支持清单的节点才有「移到另一个节点…」
  // 周期项（模板）不能移：移过去的只是一份普通的项，规则会丢
  const otherNodeSupportsTodo = useNodes((s) => s.order.some((id) => id !== item.node_id && s.byId[id]?.info?.capabilities.todo === true))
  const canMove = !item.repeat && otherNodeSupportsTodo
  const { title, body, project, due_ms, repeat } = item
  const repeatInput = repeatToInput(repeat)
  const current = useMemo(() => valuesOf({ title, body, project, due_ms, repeat }), [title, body, project, due_ms, repeat])
  const repeatSupported = useNodeSupportsRepeat(item.node_id)

  const dirty = draft ? dirtyFields(draft) : []
  // 编辑期间别处改了的字段：没改过的字段不会被发出去（保存只发用户改过的），在这里告诉用户；两边都改了的字段让用户选
  const elsewhere = draft ? changedElsewhere(draft, current) : []
  const conflicts = elsewhere.filter((f) => dirty.includes(f))
  const quiet = elsewhere.filter((f) => !dirty.includes(f))
  const fieldName = (f: EditField): string => t(f === 'title' ? 'todos.fieldTitle' : f === 'body' ? 'todos.fieldBody' : f === 'project' ? 'todos.project' : f === 'repeat' ? 'todos.repeat.label' : 'todos.due')

  const setField = (f: EditField, v: string): void => useDrafts.getState().set(key, f, v)

  const save = async (): Promise<void> => {
    if (!draft || saving || conflicts.length > 0) return
    // 只发用户改过的字段（草稿对开始编辑时那一项的差）；期望时间只有改了输入才发（输入只到分钟，没碰就不能把秒和毫秒抹掉）
    const fields: UpdateFields = {}
    if (dirty.includes('title')) fields.title = draft.values.title
    if (dirty.includes('body')) fields.body = draft.values.body
    if (dirty.includes('project')) {
      const p = draft.values.project.trim()
      if (p) fields.project = p
      else fields.clear = ['project']
    }
    if (dirty.includes('due')) {
      const d = dueFromInput(draft.values.due)
      if (d != null) fields.due_ms = d
      else fields.clear = [...(fields.clear ?? []), 'due_ms']
    }
    if (dirty.includes('repeat')) {
      const r = repeatFromInput(draft.values.repeat ?? '')
      if (r) fields.repeat = r
      else fields.clear = [...(fields.clear ?? []), 'repeat']
    }
    if (Object.keys(fields).length > 0) {
      setSaving(true)
      const r = await updateTodo(item.node_id, item.id, fields)
      setSaving(false)
      if (!r.ok) {
        setError(r.error)
        return
      }
    }
    setError(null)
    useDrafts.getState().clear(key)
  }

  const cancel = (): void => {
    if (dirty.length > 0) setConfirmDiscard(true)
    else useDrafts.getState().clear(key)
  }

  // 单击「收下」的 agent 项：没有编辑控件，只有收下 / 丢掉（spec/web.md § 4.11、§ 4.12）
  // 只有 agent 项、或自己带着 `unreviewed` 的不认识的来源，才走「收下 / 丢掉」；人记的 `unreviewed` 恒为 false
  const pendingReview = item.unreviewed && !isHumanSource(item.source)

  const states = item.state === 'waiting' || item.state === 'review' ? [...UI_STATES, item.state] : UI_STATES
  return (
    <div className="space-y-4" data-testid="todo-detail" data-id={item.id}>
      <div className="flex flex-wrap items-center gap-2">
        {isAgentSource(item.source) ? (
          <Chip tone="info" icon={<Bot className="size-3" aria-hidden />}>
            {t('todos.byAgent')}
          </Chip>
        ) : isOtherSource(item.source) ? (
          <Chip tone="info" icon={<Bot className="size-3" aria-hidden />} testId="todo-auto">
            {t('todos.byOther')}
          </Chip>
        ) : null}
        {activity ? (
          <Chip tone={activity === 'review' ? 'success' : activity === 'waiting' ? 'warning' : 'info'} testId="todo-activity" dataState={activity}>
            {t(`todos.activity.${activity}`)}
          </Chip>
        ) : null}
        <LinkedStatus item={item} />
        <HandlerChip item={item} />
        <RepeatTemplateChip item={item} />
        <RepeatFromChip item={item} />
        {item.failed ? <Chip tone="danger">{t('todos.notSaved')}</Chip> : item.unsynced ? <Chip tone="warning">{t('todos.notSynced')}</Chip> : null}
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
            <Input value={draft?.values.title ?? ''} onChange={(e) => setField('title', e.target.value)} data-testid="todo-edit-title" />
          </label>
          <label className="block space-y-1 text-sm font-medium">
            {t('todos.fieldBody')}
            <Textarea rows={8} value={draft?.values.body ?? ''} onChange={(e) => setField('body', e.target.value)} data-testid="todo-edit-body" />
          </label>
          <div className="flex flex-wrap gap-3">
            <label className="space-y-1 text-sm font-medium">
              {t('todos.project')}
              <Input value={draft?.values.project ?? ''} onChange={(e) => setField('project', e.target.value)} className="w-44" data-testid="todo-edit-project" />
            </label>
            <label className="space-y-1 text-sm font-medium">
              {t('todos.due')}
              <Input type="datetime-local" value={draft?.values.due ?? ''} onChange={(e) => setField('due', e.target.value)} className="w-auto" data-testid="todo-edit-due" />
            </label>
          </div>
          {repeatSupported || repeatInput ? (
            <div className="space-y-1 text-sm font-medium">
              <span>{t('todos.repeat.label')}</span>
              <RepeatField value={draft?.values.repeat ?? ''} onChange={(v) => setField('repeat', v)} testId="todo-edit-repeat" />
              {repeatInputError(draft?.values.repeat ?? '') ? <p className="text-xs font-normal text-danger">{t('todos.errors.repeat.invalid')}</p> : null}
            </div>
          ) : null}
          {quiet.length > 0 ? (
            <div className="space-y-1 rounded-md border border-warning/40 bg-warning-subtle p-3 text-sm" role="status" data-testid="todo-changed-elsewhere">
              <p>{t('todos.editedElsewhere', { fields: quiet.map(fieldName).join(', ') })}</p>
              <Button size="sm" onClick={() => draft && useDrafts.getState().rebase(key, quiet, current, true)} data-testid="todo-show-theirs">
                {t('todos.showTheirs')}
              </Button>
            </div>
          ) : null}
          {conflicts.map((f) => (
            <div key={f} className="space-y-2 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm" role="alert" data-testid="todo-conflict" data-field={f}>
              <p className="font-medium">{t('todos.conflict', { field: fieldName(f) })}</p>
              <p className="text-fg-muted">
                {t('todos.theirs')}: <span className="whitespace-pre-wrap [overflow-wrap:anywhere]" data-testid="todo-conflict-theirs">{f === 'repeat' ? repeatInputText(t, current.repeat ?? '') : current[f] || t('todos.none')}</span>
              </p>
              <div className="flex gap-2">
                <Button size="sm" onClick={() => useDrafts.getState().rebase(key, [f], current, false)} data-testid="todo-keep-mine">
                  {t('todos.keepMine')}
                </Button>
                <Button size="sm" onClick={() => useDrafts.getState().rebase(key, [f], current, true)} data-testid="todo-use-theirs">
                  {t('todos.useTheirs')}
                </Button>
              </div>
            </div>
          ))}
          {error ? (
            <p role="alert" className="text-sm text-danger" data-testid="todo-edit-error">
              {fieldErrorText(t, error)}
            </p>
          ) : null}
          <div className="flex gap-2">
            <Button type="submit" variant="primary" disabled={saving || conflicts.length > 0} data-testid="todo-save">
              {t('common.save')}
            </Button>
            <Button
              onClick={() => {
                setError(null)
                cancel()
              }}
              data-testid="todo-cancel-edit"
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
            {pendingReview ? null : (
              <Button size="sm" onClick={() => useDrafts.getState().start(key, current)} icon={<Pencil className="size-3.5" aria-hidden />} data-testid="todo-edit">
                {t('todos.edit')}
              </Button>
            )}
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
            {item.repeat ? (
              <>
                <dt className="text-fg-subtle">{t('todos.repeat.label')}</dt>
                <dd data-testid="todo-repeat-rule">{isClosed(item.state) ? repeatSummary(t, i18n.language, { ...item.repeat, next_ms: null }) : repeatSummary(t, i18n.language, item.repeat)}</dd>
              </>
            ) : null}
          </dl>
          {item.repeat ? (
            <p className="text-sm text-fg-muted" data-testid="todo-repeat-note">
              {t(isClosed(item.state) ? 'todos.repeat.noteClosed' : 'todos.repeat.note')}
            </p>
          ) : null}
        </>
      )}

      {pendingReview ? (
        <div className="flex flex-wrap items-center gap-2" data-testid="todo-review">
          <p className="w-full text-sm text-fg-muted">{t(isAgentSource(item.source) ? 'inbox.laterBody' : 'inbox.laterBodyOther')}</p>
          <Button variant="primary" disabled={!online} onClick={() => void updateTodo(item.node_id, item.id, { reviewed: true })} data-testid="todo-accept">
            {t('inbox.accept')}
          </Button>
          <Button disabled={!online} onClick={() => void updateTodo(item.node_id, item.id, { state: 'dropped' })} data-testid="todo-dismiss">
            {t('inbox.dismiss')}
          </Button>
          <Button disabled={!online} onClick={() => setDispatching(true)} data-testid="todo-dispatch">
            {t('dispatch.action')}
          </Button>
        </div>
      ) : (
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
        {!isClosed(item.state) ? (
          <>
            <TwinHandlerButton item={item} />
            {item.repeat ? null : (
              <Button variant={isReview(item) ? 'outline' : 'primary'} disabled={!online} onClick={() => setDispatching(true)} title={online ? undefined : t('dispatch.blocked.offline')} data-testid="todo-dispatch">
                {t('dispatch.action')}
              </Button>
            )}
          </>
        ) : null}
        {isReview(item) ? (
          <Link to="/todos/$node/$todo/review" params={{ node: shortNodeId(item.node_id, order), todo: item.id }} className="inline-flex min-h-9 items-center rounded-md border border-border-strong px-3 text-sm font-medium hover:bg-bg-hover max-md:min-h-11" data-testid="todo-review-link">
            {t('todos.reviewPage.open')}
          </Link>
        ) : null}
        {canMove ? (
          <Button onClick={() => setMoving(true)} data-testid="todo-move">
            {t('spaces.move.title')}
          </Button>
        ) : null}
        <Button variant="outline-danger" onClick={() => setConfirmDelete(true)} data-testid="todo-delete">
          {t('todos.delete')}
        </Button>
      </div>
      )}

      {isAgentSource(item.source) ? (
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

      <TodoHistory item={item} />

      <DispatchDialog item={item} open={dispatching} onOpenChange={setDispatching} />
      <MoveTodoDialog item={item} open={moving} onOpenChange={setMoving} />

      <Dialog open={confirmDiscard} onOpenChange={setConfirmDiscard} title={t('todos.discardEditTitle')} description={t('todos.discardEditBody')} closeLabel={t('common.close')} testId="todo-discard-edit-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirmDiscard(false)}>{t('todos.keepEditing')}</Button>
          <Button
            variant="danger"
            onClick={() => {
              setConfirmDiscard(false)
              useDrafts.getState().clear(key)
            }}
            data-testid="todo-discard-edit-confirm"
          >
            {t('todos.discardEdit')}
          </Button>
        </div>
      </Dialog>

      <Dialog open={confirmDelete} onOpenChange={setConfirmDelete} title={t('todos.deleteTitle')} description={t('todos.deleteBody', { title: cleanOneLine(item.title, 80) })} closeLabel={t('common.close')} testId="todo-delete-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirmDelete(false)}>{t('common.cancel')}</Button>
          <Button
            variant="danger"
            onClick={() => {
              setConfirmDelete(false)
              useDrafts.getState().clear(key)
              void deleteTodo(item.node_id, item.id).then((ok) => ok && onDeleted?.())
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
