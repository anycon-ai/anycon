// 周期项（docs/spec/daemon.md § 26.9，spec/web.md § 14.6）的界面零件：「重复」的选择控件、规则的文字、复制出来的项上的「来自周期项」。
import { Link } from '@tanstack/react-router'
import { Repeat } from 'lucide-react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import type { TodoRepeat } from '@/protocol'
import { formatDateTime } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Input, Select } from '@/ui/Field'
import { cleanOneLine } from '../validate'
import { formOf, inputOf, repeatTemplateOf, REPEAT_CHOICES, type RepeatChoice } from '../repeat'
import type { ViewTodo } from '../types'
import { useOriginTodo } from './TodoOrigin'

/** 这个节点支持周期项（`capabilities.todo_repeat`）。 */
export const useNodeSupportsRepeat = (nodeId: string | null | undefined): boolean => useNodes((s) => (nodeId ? s.byId[nodeId]?.info?.capabilities.todo_repeat === true : false))

/** 规则的一句话：「每天 09:00」「工作日 09:00」「每周一 09:00」。不认识的种类（更新的节点给的）只说看不懂。 */
export function repeatRuleText(t: TFunction, r: Pick<TodoRepeat, 'kind' | 'at' | 'weekday'>): string {
  if (r.kind === 'daily') return t('todos.repeat.rule.daily', { at: r.at })
  if (r.kind === 'weekdays') return t('todos.repeat.rule.weekdays', { at: r.at })
  if (r.kind === 'weekly') return t('todos.repeat.rule.weekly', { day: t(`todos.repeat.day.${r.weekday ?? 1}`), at: r.at })
  return t('todos.repeat.rule.unknown')
}

/** 草稿里的规则（`repeatToInput` 的写法）的一句话；空串是「不重复」。冲突提示里用，不给用户看原始的字符串。 */
export function repeatInputText(t: TFunction, input: string): string {
  const f = formOf(input)
  if (f.choice === 'none') return t('todos.repeat.choice.none')
  return repeatRuleText(t, { kind: f.choice, at: f.at, weekday: f.weekday })
}

/** 规则和下一次的时间（节点按它自己的时区算好的）：一行字，列表行和详情共用。 */
export function repeatSummary(t: TFunction, language: string, r: TodoRepeat): string {
  const rule = repeatRuleText(t, r)
  return r.next_ms ? `${rule} · ${t('todos.repeat.next', { time: formatDateTime(r.next_ms, language) })}` : rule
}

/**
 * 「重复」：不重复 / 每天 / 工作日 / 每周（再选星期几）+ 时间。值是 `repeatToInput` 的字符串，由调用方存。
 * `disabledReason` 给了就整个禁用并写明原因。
 */
export function RepeatField({ value, onChange, disabledReason, testId = 'repeat', className }: { value: string; onChange: (v: string) => void; disabledReason?: string; testId?: string; className?: string }) {
  const { t } = useTranslation()
  const f = formOf(value)
  const set = (patch: Partial<typeof f>): void => onChange(inputOf({ ...f, ...patch }))
  const disabled = disabledReason !== undefined
  return (
    <div className={className ?? 'flex flex-wrap items-center gap-1.5'} data-testid={testId}>
      <Select value={f.choice} disabled={disabled} title={disabledReason} aria-label={t('todos.repeat.label')} onChange={(e) => set({ choice: e.target.value as RepeatChoice })} className="!h-9 w-auto" data-testid={`${testId}-kind`}>
        {REPEAT_CHOICES.map((c) => (
          <option key={c} value={c}>
            {t(`todos.repeat.choice.${c}`)}
          </option>
        ))}
      </Select>
      {f.choice === 'weekly' ? (
        <Select value={String(f.weekday)} disabled={disabled} aria-label={t('todos.repeat.weekday')} onChange={(e) => set({ weekday: Number(e.target.value) })} className="!h-9 w-auto" data-testid={`${testId}-weekday`}>
          {[1, 2, 3, 4, 5, 6, 7].map((d) => (
            <option key={d} value={d}>
              {t(`todos.repeat.day.${d}`)}
            </option>
          ))}
        </Select>
      ) : null}
      {f.choice !== 'none' ? <Input type="time" value={f.at} disabled={disabled} aria-label={t('todos.repeat.at')} onChange={(e) => set({ at: e.target.value })} className="!h-9 w-auto" data-testid={`${testId}-at`} /> : null}
      {disabledReason ? <span className="text-xs text-fg-subtle">{disabledReason}</span> : null}
    </div>
  )
}

const chipClass = 'inline-flex max-w-full items-center gap-1 rounded-full bg-info-subtle px-2 py-0.5 text-xs font-medium whitespace-nowrap text-info'

/** 模板自己的标记：「重复」。 */
export function RepeatTemplateChip({ item }: { item: Pick<ViewTodo, 'repeat'> }) {
  const { t } = useTranslation()
  if (!item.repeat) return null
  return (
    <span className={chipClass} data-testid="todo-repeat-chip">
      <Repeat className="size-3" aria-hidden />
      <span className="truncate">{t('todos.repeat.template')}</span>
    </span>
  )
}

/** 列表行里的「来自周期项」：行本身是按钮，里面不能再放链接，所以只是标记（点进详情有去模板的链接）。 */
export function RepeatFromBadge({ item }: { item: Pick<ViewTodo, 'source'> }) {
  const { t } = useTranslation()
  if (!repeatTemplateOf(item)) return null
  return (
    <span className={chipClass} data-testid="todo-repeat-from-badge">
      <Repeat className="size-3" aria-hidden />
      <span className="truncate">{t('todos.repeat.from')}</span>
    </span>
  )
}

/** 复制出来的项：「来自周期项」，点了去模板；模板已经删掉时只说明。 */
export function RepeatFromChip({ item }: { item: Pick<ViewTodo, 'node_id' | 'source'> }) {
  const template = repeatTemplateOf(item)
  return template ? <RepeatFrom nodeId={item.node_id} templateId={template} /> : null
}

function RepeatFrom({ nodeId, templateId }: { nodeId: string; templateId: string }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const origin = useOriginTodo(nodeId, templateId)
  const inner = (
    <>
      <Repeat className="size-3" aria-hidden />
      <span className="truncate">{t('todos.repeat.from')}</span>
    </>
  )
  if (origin.state === 'found') {
    const title = cleanOneLine(origin.todo.title, 80)
    return (
      <Link to="/todos/$node/$todo" params={{ node: shortNodeId(nodeId, order), todo: templateId }} title={title} aria-label={t('todos.repeat.fromTitle', { title })} className={`${chipClass} hover:underline`} data-testid="todo-repeat-from">
        {inner}
      </Link>
    )
  }
  if (origin.state === 'deleted') {
    return (
      <span className={chipClass} data-testid="todo-repeat-from-deleted" title={t('todos.repeat.templateGone')}>
        {inner}
      </span>
    )
  }
  return (
    <span className={chipClass} data-testid="todo-repeat-from-plain">
      {inner}
    </span>
  )
}
