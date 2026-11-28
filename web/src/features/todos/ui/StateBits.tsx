// 周期项的跨次状态（docs/spec/daemon.md § 26.11，spec/web.md § 4.12）的界面零件：表单里的开关和用法说明、「插入示例」、
// 分段显示的第一条消息（模板的预览和派发对话框共用）、模板详情里的查看和清理。
// 用法说明的文字只从节点取（`todo.repeat_preview`），这里没有它的副本；条目只放在组件自己的状态里，每次从节点取。
import { useEffect, useId, useRef, useState } from 'react'
import { ChevronDown, Plus, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PromptSegment, TodoRepeatPreviewParams } from '@/protocol'
import { formatDateTime } from '@/lib/format'
import { stripUnsafe } from '@/lib/hint'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { CheckboxField } from '@/ui/Choice'
import { Input } from '@/ui/Field'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { Dialog } from '@/ui/Overlay'
import { cn } from '@/ui/cn'
import { countRules } from '../dispatch'
import { useHistoryEntry } from '../history'
import { formOf, withStateFlags } from '../repeat'
import { clearState, deleteStateEntry, describeStateError, isAuthored, listState, STATE_EXAMPLES, STATE_PAGE, stateUsageSegment, useNodeSupportsState, useRepeatPreview, useStateCount, useStateCounts, useStateTick, type StatePage } from '../state'
import type { ViewTodo } from '../types'
import { cleanOneLine, type FieldError } from '../validate'
import { fieldErrorText } from './errors'

const monoBox = 'scroll-thin max-h-64 overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap [overflow-wrap:anywhere]'

/** agent 写的多行文字：一行一行去掉控制字符和双向控制符，保留换行。 */
const cleanLines = (s: string): string => s.split('\n').map(stripUnsafe).join('\n')

/**
 * 第一条消息按段显示：「你写的」（标题、备注）和「anycon 加的」（其余）用不同的样式，anycon 加的每段旁边一个短标签说它是什么。
 * 纯文本，不当 Markdown。模板的预览和派发对话框共用。
 */
export function PromptSegments({ segments, testId = 'prompt-segments', busy }: { segments: readonly PromptSegment[]; testId?: string; busy?: boolean }) {
  const { t } = useTranslation()
  return (
    <ol className="space-y-2" data-testid={testId} aria-busy={busy}>
      {segments.map((s, i) => {
        const authored = isAuthored(s)
        const label = t(`todos.runState.preview.segment.${s.kind}`, { defaultValue: t('todos.runState.preview.segment.unknown') })
        return (
          <li key={i} className="space-y-1" data-testid="prompt-segment" data-kind={s.kind} data-authored={authored ? 'true' : 'false'}>
            <p className="flex flex-wrap items-center gap-1.5 text-xs">
              <span className={cn('rounded-full px-2 py-0.5 font-medium', authored ? 'bg-bg-muted text-fg-muted' : 'bg-info-subtle text-info')}>{authored ? t('todos.runState.preview.yours') : t('todos.runState.preview.added')}</span>
              <span className="text-fg-subtle">{label}</span>
            </p>
            <pre className={cn(monoBox, authored ? 'border-border bg-surface text-fg' : 'border-dashed border-info/40 bg-bg-subtle text-fg-muted')}>{s.text}</pre>
          </li>
        )
      })}
    </ol>
  )
}

/** 表单的基础内容：模板（或还没保存的新项）的 id、表单里的标题 / 备注 / 项目。 */
export interface PreviewBase {
  id: string
  title?: string
  body?: string
  project?: string
}

/** 预览请求的参数：表单里此刻的开关值也带上（还没保存的勾选要即时反映）。 */
export function previewParamsOf(base: PreviewBase, repeat: string): TodoRepeatPreviewParams {
  const f = formOf(repeat)
  return { ...base, keep_state: f.keepState, state_hint_off: f.keepState && f.stateHintOff }
}

/**
 * 「跨次保留状态」和它下面的「附带用法说明」。勾着用法说明时直接显示 daemon 会多发的那段原文（`segments` 里 `state_usage`
 * 那段）；不勾时说明要自己写，给「复制到备注」（`state_usage_text`，前面空一行追加到备注末尾）。`onAppend` 返回错误（备注
 * 超过上限）就不追加、把错误显示出来。节点没有 `capabilities.todo_repeat_state` 时整块不出现；没有规则时也不出现。
 */
export function KeepStateField({ nodeId, value, onChange, base, onAppend, testId = 'repeat' }: { nodeId: string; value: string; onChange: (v: string) => void; base: PreviewBase; onAppend: (text: string) => FieldError | null; testId?: string }) {
  const { t } = useTranslation()
  const id = useId()
  const supported = useNodeSupportsState(nodeId)
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const f = formOf(value)
  const [appendError, setAppendError] = useState<FieldError | null>(null)
  const params = supported && online && f.choice !== 'none' && f.keepState ? previewParamsOf(base, value) : null
  const preview = useRepeatPreview(nodeId, params, t)
  if (!supported || f.choice === 'none') return null
  const usage = stateUsageSegment(preview.result?.segments)
  const copyText = preview.result?.state_usage_text ?? null
  return (
    <div className="space-y-1" data-testid={`${testId}-state`}>
      <CheckboxField id={`${id}-keep`} checked={f.keepState} onCheckedChange={(v) => onChange(withStateFlags(value, { keepState: v }))} label={t('todos.runState.keep.label')} describedBy={`${id}-keep-hint`} testId={`${testId}-keep-state`} />
      <p id={`${id}-keep-hint`} className="-mt-2 pl-[1.875rem] text-xs font-normal text-fg-subtle">
        {t('todos.runState.keep.hint')}
      </p>
      {f.keepState ? (
        <div className="space-y-1 pl-[1.625rem]">
          <CheckboxField id={`${id}-note`} checked={!f.stateHintOff} onCheckedChange={(v) => onChange(withStateFlags(value, { stateHintOff: !v }))} label={t('todos.runState.note.label')} describedBy={`${id}-note-detail`} testId={`${testId}-state-note`} />
          <div id={`${id}-note-detail`} className="space-y-1.5 pl-[1.875rem] font-normal">
            {!online ? (
              <p className="text-xs text-fg-subtle" data-testid={`${testId}-state-offline`}>
                {t('todos.runState.note.offline')}
              </p>
            ) : preview.error ? (
              <p className="text-xs text-danger" role="alert" data-testid={`${testId}-state-error`}>
                {preview.error}
              </p>
            ) : !f.stateHintOff ? (
              usage !== null ? (
                <>
                  <p className="text-xs text-fg-muted">{t('todos.runState.note.addedBy')}</p>
                  <pre className={cn(monoBox, 'border-dashed border-info/40 bg-bg-subtle text-fg-muted')} data-testid={`${testId}-state-usage`} aria-busy={preview.loading}>
                    {usage}
                  </pre>
                </>
              ) : (
                <p className="text-xs text-fg-subtle" role="status">
                  {t('todos.runState.note.loading')}
                </p>
              )
            ) : (
              <>
                <p className="text-xs text-fg-muted" data-testid={`${testId}-state-note-off`}>
                  {t('todos.runState.note.off')}
                </p>
                <Button
                  size="sm"
                  disabled={copyText === null}
                  onClick={() => {
                    if (copyText === null) return
                    setAppendError(onAppend(copyText))
                  }}
                  data-testid={`${testId}-state-copy`}
                >
                  {t('todos.runState.note.copy')}
                </Button>
              </>
            )}
            {appendError ? (
              <p className="text-xs text-danger" role="alert" data-testid={`${testId}-state-append-error`}>
                {fieldErrorText(t, appendError)}
              </p>
            ) : null}
          </div>
        </div>
      ) : null}
    </div>
  )
}

/** 「插入示例」：选一种，真的写进备注（追加在末尾，前面空一行）。 */
export function StateExamples({ onAppend, testId = 'state-examples' }: { onAppend: (text: string) => FieldError | null; testId?: string }) {
  const { t } = useTranslation()
  const [error, setError] = useState<FieldError | null>(null)
  return (
    <div className="space-y-1">
      <Menu>
        <MenuTrigger asChild>
          <Button size="sm" variant="outline" icon={<Plus className="size-3.5" aria-hidden />} data-testid={testId}>
            {t('todos.runState.examples.label')}
            <ChevronDown className="size-3.5" aria-hidden />
          </Button>
        </MenuTrigger>
        <MenuContent>
          {STATE_EXAMPLES.map((ex) => (
            <MenuItem key={ex} onSelect={() => setError(onAppend(t(`todos.runState.examples.${ex}.text`)))} hint={t(`todos.runState.examples.${ex}.text`)} testId={`${testId}-${ex}`}>
              {t(`todos.runState.examples.${ex}.label`)}
            </MenuItem>
          ))}
        </MenuContent>
      </Menu>
      {error ? (
        <p className="text-xs font-normal text-danger" role="alert" data-testid={`${testId}-error`}>
          {fieldErrorText(t, error)}
        </p>
      ) : null}
    </div>
  )
}

/**
 * 模板的「预览发给 agent 的内容」：展开时取（`todo.repeat_preview`，带着表单里此刻的内容），表单变了重新取，旧的回复不盖掉新的，
 * 失败时显示原因。下面照派发对话框的做法列出会写进系统提示的规则。
 */
export function RepeatPreview({ nodeId, base, repeat, testId = 'repeat-preview' }: { nodeId: string; base: PreviewBase; repeat: string; testId?: string }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const supported = useNodeSupportsState(nodeId)
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const rulesSupported = useNodes((s) => s.byId[nodeId]?.info?.capabilities.rules === true)
  const boxId = useId()
  const preview = useRepeatPreview(nodeId, open && supported && online ? previewParamsOf(base, repeat) : null, t)
  if (!supported) return null
  const r = preview.result
  return (
    <div className="space-y-2" data-testid={testId}>
      <button type="button" aria-expanded={open} aria-controls={boxId} onClick={() => setOpen((o) => !o)} className="text-sm font-medium text-accent-text hover:underline" data-testid={`${testId}-toggle`}>
        {open ? t('todos.runState.preview.hide') : t('todos.runState.preview.show')}
      </button>
      {open ? (
        <div id={boxId} className="space-y-2 text-sm" data-testid={`${testId}-box`}>
          <p className="text-xs text-fg-muted">{t('todos.runState.preview.note')}</p>
          {!online ? (
            <p className="text-fg-muted" data-testid={`${testId}-offline`}>
              {t('todos.runState.preview.offline')}
            </p>
          ) : preview.error ? (
            <p className="text-danger" role="alert" data-testid={`${testId}-error`}>
              {preview.error}
            </p>
          ) : r === null ? (
            <p className="text-fg-muted" role="status">
              {t('todos.runState.preview.loading')}
            </p>
          ) : (
            <>
              <PromptSegments segments={r.segments} testId={`${testId}-segments`} busy={preview.loading} />
              {rulesSupported ? (
                <div className="space-y-1" data-testid={`${testId}-rules`}>
                  <p className="text-sm font-medium">{t('dispatch.rules.title')}</p>
                  {(r.rules_text ?? '') === '' ? (
                    <p className="text-fg-muted">{t('dispatch.rules.none')}</p>
                  ) : (
                    <>
                      <p className="text-xs text-fg-muted">{t('dispatch.rules.count', { count: countRules(r.rules_text ?? '') })}</p>
                      <pre className="scroll-thin max-h-48 overflow-auto rounded-md border border-border bg-bg-subtle p-2 text-xs whitespace-pre-wrap [overflow-wrap:anywhere]" data-testid={`${testId}-rules-text`}>
                        {r.rules_text}
                      </pre>
                    </>
                  )}
                </div>
              ) : null}
            </>
          )}
        </div>
      ) : null}
    </div>
  )
}

/** 这一项需不需要「跨次状态」这一块：开着状态的模板；关着或已经不再重复、但名下还有条目的（条数从节点取）。 */
function useStatePanelTarget(item: ViewTodo): { relevant: boolean; keep: boolean; former: boolean } {
  const supported = useNodeSupportsState(item.node_id)
  const history = useHistoryEntry(item.node_id, item.id)
  // 以前是模板：「经过」里有复制记录
  const former = !item.repeat && history.events.some((e) => e.kind === 'repeat_created')
  return { relevant: supported && (!!item.repeat || former), keep: !!item.repeat?.keep_state, former }
}

/**
 * 模板详情里的「跨次状态」：条数；可展开的列表（翻页、按前缀筛选），每条键、值、写入时间，可以删单条；「清空全部」要二次确认。
 * 键和值是 agent 写的，按不可信的纯文本显示。条目不存任何本地缓存，每次从节点取；节点离线时这一块不可用。
 * 开关关着、或这一项已经不再重复时，条目看不了（节点不给），只能清空。
 */
export function StatePanel({ item }: { item: ViewTodo }) {
  const { t, i18n } = useTranslation()
  const nodeId = item.node_id
  const { relevant, keep, former } = useStatePanelTarget(item)
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const count = useStateCount(nodeId, item.id)
  const tick = useStateTick(nodeId, item.id)
  const [open, setOpen] = useState(false)
  const [prefix, setPrefix] = useState('')
  const [page, setPage] = useState<StatePage | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmClear, setConfirmClear] = useState(false)
  const [busy, setBusy] = useState(false)
  const seq = useRef(0)
  const listId = useId()

  // 条数：开着时由列表顺带给；关着（或已经不再重复）时列表取不了，用预览的 `state_count`
  const preview = useRepeatPreview(nodeId, relevant && online && count === undefined ? { id: item.id } : null, t, 0)
  useEffect(() => {
    if (preview.result) useStateCounts.getState().set(nodeId, item.id, preview.result.state_count)
  }, [preview.result, nodeId, item.id])

  // 展开着：前缀变了（等打完）、收到 `todo.state_changed` 时重新取当前已经翻到的那么多条
  const loaded = page?.entries.length ?? 0
  const want = Math.max(1, Math.ceil(loaded / STATE_PAGE))
  useEffect(() => {
    if (!open || !keep || !online) return
    const mine = ++seq.current
    setLoading(true)
    setError(null)
    const timer = setTimeout(() => {
      void (async () => {
        try {
          let acc: StatePage | null = null
          for (let i = 0; i < want; i++) {
            const next: StatePage = await listState(nodeId, item.id, prefix, acc?.entries.at(-1)?.key)
            acc = acc ? { ...next, entries: [...acc.entries, ...next.entries] } : next
            if (!next.hasMore) break
          }
          if (mine === seq.current) setPage(acc)
        } catch (e) {
          if (mine === seq.current) setError(describeStateError(e, t))
        } finally {
          if (mine === seq.current) setLoading(false)
        }
      })()
    }, 200)
    return () => clearTimeout(timer)
    // 翻到第几页（`want`）只在重新取时用：加载更多自己追加，不触发这里
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, keep, online, nodeId, item.id, prefix, tick])

  const more = async (): Promise<void> => {
    if (!page || loading) return
    const mine = ++seq.current
    setLoading(true)
    try {
      const next = await listState(nodeId, item.id, prefix, page.entries.at(-1)?.key)
      if (mine === seq.current) setPage({ ...next, entries: [...page.entries, ...next.entries] })
    } catch (e) {
      if (mine === seq.current) setError(describeStateError(e, t))
    } finally {
      if (mine === seq.current) setLoading(false)
    }
  }

  const remove = async (key: string): Promise<void> => {
    setBusy(true)
    try {
      await deleteStateEntry(nodeId, item.id, key)
      setPage((p) => (p ? { ...p, entries: p.entries.filter((e) => e.key !== key), total: Math.max(0, p.total - 1), matched: Math.max(0, p.matched - 1) } : p))
    } catch (e) {
      setError(describeStateError(e, t))
    } finally {
      setBusy(false)
    }
  }

  const clear = async (): Promise<void> => {
    setConfirmClear(false)
    setBusy(true)
    try {
      await clearState(nodeId, item.id)
      setPage((p) => (p ? { ...p, entries: [], total: 0, matched: 0, hasMore: false } : p))
    } catch (e) {
      setError(describeStateError(e, t))
    } finally {
      setBusy(false)
    }
  }

  if (!relevant || (!keep && !(count !== undefined && count > 0))) return null
  return (
    <section className="space-y-2 rounded-md border border-border p-3 text-sm" aria-labelledby={`${listId}-title`} data-testid="todo-state-panel">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h3 id={`${listId}-title`} className="font-semibold">
          {t('todos.runState.panel.title')}
        </h3>
        {online && count !== undefined ? (
          <span className="text-fg-muted" data-testid="todo-state-count">
            {t('todos.runState.panel.count', { count })}
          </span>
        ) : null}
      </div>
      {!online ? (
        <p className="text-fg-muted" data-testid="todo-state-offline">
          {t('todos.runState.panel.offline')}
        </p>
      ) : (
        <>
          {!keep ? <p className="text-fg-muted">{t(former ? 'todos.runState.panel.former' : 'todos.runState.panel.off')}</p> : null}
          <div className="flex flex-wrap items-center gap-2">
            {keep ? (
              <Button size="sm" aria-expanded={open} aria-controls={listId} onClick={() => setOpen((o) => !o)} data-testid="todo-state-toggle">
                {open ? t('todos.runState.panel.hide') : t('todos.runState.panel.show')}
              </Button>
            ) : null}
            <Button size="sm" variant="outline-danger" disabled={busy || count === 0} onClick={() => setConfirmClear(true)} data-testid="todo-state-clear">
              {t('todos.runState.panel.clear')}
            </Button>
          </div>
          {error ? (
            <p className="text-danger" role="alert" data-testid="todo-state-error">
              {error}
            </p>
          ) : null}
          {keep && open ? (
            <div id={listId} className="space-y-2" data-testid="todo-state-list">
              <p className="text-xs text-fg-subtle">{t('todos.runState.panel.untrusted')}</p>
              <label className="flex flex-col gap-1 text-xs font-medium text-fg-muted">
                {t('todos.runState.panel.filter')}
                <Input value={prefix} onChange={(e) => setPrefix(e.target.value)} className="!h-9 w-full max-w-72 font-mono" data-testid="todo-state-prefix" />
              </label>
              {page && prefix !== '' ? <p className="text-xs text-fg-subtle">{t('todos.runState.panel.matched', { matched: page.matched, total: page.total })}</p> : null}
              {page === null ? (
                <p className="text-fg-muted" role="status">
                  {t('todos.runState.panel.loading')}
                </p>
              ) : page.entries.length === 0 ? (
                <p className="text-fg-muted" data-testid="todo-state-empty">
                  {prefix ? t('todos.runState.panel.noMatch') : t('todos.runState.panel.empty')}
                </p>
              ) : (
                <ul className="divide-y divide-border rounded-md border border-border" aria-busy={loading}>
                  {page.entries.map((e) => (
                    <li key={e.key} className="flex items-start gap-2 px-2.5 py-2" data-testid="todo-state-entry">
                      <div className="min-w-0 flex-1 space-y-0.5">
                        <p className="font-mono text-xs font-medium [overflow-wrap:anywhere]" data-testid="todo-state-key">
                          {cleanOneLine(e.key, 200)}
                        </p>
                        {e.value !== '' ? (
                          <pre className="max-h-32 overflow-auto font-mono text-xs whitespace-pre-wrap text-fg-muted [overflow-wrap:anywhere]" data-testid="todo-state-value">
                            {cleanLines(e.value)}
                          </pre>
                        ) : null}
                        <p className="text-xs text-fg-subtle">
                          {t('todos.runState.panel.written')} {formatDateTime(e.written_ms, i18n.language)}
                        </p>
                      </div>
                      <Button size="sm" variant="outline" disabled={busy} aria-label={t('todos.runState.panel.deleteLabel', { key: cleanOneLine(e.key, 80) })} icon={<Trash2 className="size-3.5" aria-hidden />} onClick={() => void remove(e.key)} data-testid="todo-state-delete">
                        {t('todos.runState.panel.delete')}
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              {page?.hasMore ? (
                <Button size="sm" disabled={loading} onClick={() => void more()} data-testid="todo-state-more">
                  {t('todos.runState.panel.more')}
                </Button>
              ) : null}
            </div>
          ) : null}
        </>
      )}
      <Dialog open={confirmClear} onOpenChange={setConfirmClear} title={t('todos.runState.panel.clearTitle')} description={t('todos.runState.panel.clearBody', { count: count ?? 0 })} closeLabel={t('common.close')} testId="todo-state-clear-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirmClear(false)}>{t('common.cancel')}</Button>
          <Button variant="danger" onClick={() => void clear()} data-testid="todo-state-clear-confirm">
            {t('todos.runState.panel.clear')}
          </Button>
        </div>
      </Dialog>
    </section>
  )
}
