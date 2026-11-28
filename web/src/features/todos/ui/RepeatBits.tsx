// 周期项（docs/spec/daemon.md § 26.9，spec/web.md § 14.6）的界面零件：「重复」的选择控件、规则的文字、复制出来的项上的「来自周期项」。
import { useId } from 'react'
import { Link } from '@tanstack/react-router'
import { Repeat } from 'lucide-react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import type { TodoRepeat } from '@/protocol'
import { formatDateTime } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { CheckboxField } from '@/ui/Choice'
import { Input, Select } from '@/ui/Field'
import { cleanOneLine } from '../validate'
import { formOf, inputOf, repeatTemplateOf, REPEAT_CHOICES, REPEAT_EVERY_CHOICES, type RepeatChoice } from '../repeat'
import type { ViewTodo } from '../types'
import { useOriginTodo } from './TodoOrigin'

/** 这个节点支持周期项（`capabilities.todo_repeat`）。 */
export const useNodeSupportsRepeat = (nodeId: string | null | undefined): boolean => useNodes((s) => (nodeId ? s.byId[nodeId]?.info?.capabilities.todo_repeat === true : false))

/** 这个节点支持一天里重复多次、自动关闭和「现在执行一次」（`capabilities.todo_repeat_interval`）。 */
export const useNodeSupportsInterval = (nodeId: string | null | undefined): boolean => useNodes((s) => (nodeId ? s.byId[nodeId]?.info?.capabilities.todo_repeat_interval === true : false))

type RuleLike = Pick<TodoRepeat, 'kind' | 'at' | 'weekday'> & Partial<Pick<TodoRepeat, 'days' | 'every_min' | 'until'>>

/** 间隔的一句话：「每 30 分钟」「每 2 小时」。整小时按小时说。 */
export function everyText(t: TFunction, minutes: number): string {
  if (minutes === 60) return t('todos.repeat.rule.everyOneHour')
  return minutes % 60 === 0 ? t('todos.repeat.rule.everyHours', { n: minutes / 60 }) : t('todos.repeat.rule.everyMinutes', { n: minutes })
}

/** 规则的一句话：「每天 09:00」「工作日 09:00」「每周一 09:00」「工作日 09:00–18:00 每 2 小时」。不认识的种类（更新的节点给的）只说看不懂。 */
export function repeatRuleText(t: TFunction, r: RuleLike): string {
  const day = t(`todos.repeat.day.${r.weekday ?? 1}`)
  if (r.kind === 'daily') return t('todos.repeat.rule.daily', { at: r.at })
  if (r.kind === 'weekdays') return t('todos.repeat.rule.weekdays', { at: r.at })
  if (r.kind === 'weekly') return t('todos.repeat.rule.weekly', { day, at: r.at })
  if (r.kind === 'interval' && (r.days === 'daily' || r.days === 'weekdays' || r.days === 'weekly') && r.every_min && r.until) {
    const days = r.days === 'weekly' ? t('todos.repeat.rule.weeklyDays', { day }) : t(`todos.repeat.rule.${r.days}Days`)
    return t('todos.repeat.rule.interval', { days, at: r.at, until: r.until, every: everyText(t, r.every_min) })
  }
  return t('todos.repeat.rule.unknown')
}

/** 草稿里的规则（`repeatToInput` 的写法）的一句话；空串是「不重复」。冲突提示里用，不给用户看原始的字符串。 */
export function repeatInputText(t: TFunction, input: string): string {
  const f = formOf(input)
  if (f.choice === 'none') return t('todos.repeat.choice.none')
  const rule = f.every > 0 ? repeatRuleText(t, { kind: 'interval', days: f.choice, at: f.at, weekday: f.weekday, every_min: f.every, until: f.until }) : repeatRuleText(t, { kind: f.choice, at: f.at, weekday: f.weekday })
  const parts = [rule, ...(f.autoClose ? [t('todos.repeat.autoClose.on')] : []), ...(f.keepState ? [t(f.stateHintOff ? 'todos.runState.keep.onNoNote' : 'todos.runState.keep.on')] : [])]
  return parts.join(' · ')
}

/** 规则和下一次的时间（节点按它自己的时区算好的）：一行字，列表行和详情共用。 */
export function repeatSummary(t: TFunction, language: string, r: TodoRepeat): string {
  const rule = repeatRuleText(t, r)
  return r.next_ms ? `${rule} · ${t('todos.repeat.next', { time: formatDateTime(r.next_ms, language) })}` : rule
}

/**
 * 「重复」：不重复 / 每天 / 工作日 / 每周（再选星期几）+ 时间；节点支持时（`intervalSupported`）还能选「每隔 N 分钟 / 小时」，
 * 这时时间是开始，多一个「到」。值是 `repeatToInput` 的字符串，由调用方存。
 * `disabledReason` 给了就整个禁用并写明原因。
 */
export function RepeatField({ value, onChange, disabledReason, intervalSupported = false, testId = 'repeat', className }: { value: string; onChange: (v: string) => void; disabledReason?: string; intervalSupported?: boolean; testId?: string; className?: string }) {
  const { t } = useTranslation()
  const f = formOf(value)
  const set = (patch: Partial<typeof f>): void => onChange(inputOf({ ...f, ...patch }))
  const disabled = disabledReason !== undefined
  // 节点上已有的间隔不在常用选项里（比如命令行设的 90 分钟）：把它也列出来，不然下拉框会显示成别的值
  const everyChoices = REPEAT_EVERY_CHOICES.includes(f.every) ? REPEAT_EVERY_CHOICES : [...REPEAT_EVERY_CHOICES, f.every]
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
      {f.choice !== 'none' && intervalSupported ? (
        <Select value={String(f.every)} disabled={disabled} aria-label={t('todos.repeat.every.label')} onChange={(e) => set({ every: Number(e.target.value) })} className="!h-9 w-auto" data-testid={`${testId}-every`}>
          {everyChoices.map((m) => (
            <option key={m} value={m}>
              {m === 0 ? t('todos.repeat.every.once') : m === 60 ? t('todos.repeat.every.oneHour') : m % 60 === 0 ? t('todos.repeat.every.hours', { n: m / 60 }) : t('todos.repeat.every.minutes', { n: m })}
            </option>
          ))}
        </Select>
      ) : null}
      {f.choice !== 'none' ? <Input type="time" value={f.at} disabled={disabled} aria-label={t(f.every > 0 ? 'todos.repeat.start' : 'todos.repeat.at')} onChange={(e) => set({ at: e.target.value })} className="!h-9 w-auto" data-testid={`${testId}-at`} /> : null}
      {f.choice !== 'none' && f.every > 0 ? (
        <>
          <span className="text-sm font-normal text-fg-muted">{t('todos.repeat.untilWord')}</span>
          <Input type="time" value={f.until} disabled={disabled} aria-label={t('todos.repeat.until')} onChange={(e) => set({ until: e.target.value })} className="!h-9 w-auto" data-testid={`${testId}-until`} />
        </>
      ) : null}
      {disabledReason ? <span className="text-xs text-fg-subtle">{disabledReason}</span> : null}
    </div>
  )
}

/** 「Twin 认为做完时自动关闭」：改的是同一个规则字符串（`repeatToInput` 的写法）；没有规则时不显示。 */
export function AutoCloseField({ value, onChange, testId = 'repeat' }: { value: string; onChange: (v: string) => void; testId?: string }) {
  const { t } = useTranslation()
  const id = useId()
  const f = formOf(value)
  if (f.choice === 'none') return null
  return <CheckboxField id={id} checked={f.autoClose} onCheckedChange={(v) => onChange(inputOf({ ...f, autoClose: v }))} label={t('todos.repeat.autoClose.label')} hint={t('todos.repeat.autoClose.hint')} testId={`${testId}-auto-close`} />
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
