// 周期项的跨次状态（`todo.state_*`、`todo.repeat_preview`，docs/spec/daemon.md § 26.11，spec/web.md § 4.12）：请求、
// 预览的取法和条数。条目（键和值）是 agent 写的用户内容：**只放在组件自己的状态里，每次从节点取**，不进 IndexedDB、不进
// 任何 store；这里的 store 只记每个模板的条数（`todo.state_changed` 只带条数），也只在内存里。
import { useEffect, useRef, useState } from 'react'
import type { TFunction } from 'i18next'
import { create } from 'zustand'
import type { PromptSegment, RepeatStateChanged, RepeatStateClearResult, RepeatStateDeleteResult, RepeatStateListResult, TodoRepeatPreviewParams, TodoRepeatPreviewResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'

/** 这个节点支持跨次状态（`capabilities.todo_repeat_state`）。 */
export const useNodeSupportsState = (nodeId: string | null | undefined): boolean => useNodes((s) => (nodeId ? s.byId[nodeId]?.info?.capabilities.todo_repeat_state === true : false))

/** 一页的条数（`todo.state_list` 的 `limit`）。 */
export const STATE_PAGE = 50

const countKey = (nodeId: string, templateId: string): string => `${nodeId}\u0000${templateId}`

interface StateCounts {
  /** 每个模板现在的条数；`undefined` = 还没取过。 */
  counts: Record<string, number>
  /** 每次收到 `todo.state_changed` 加一：展开着的列表据此重新取当前页。 */
  ticks: Record<string, number>
  set: (nodeId: string, templateId: string, count: number, changed?: boolean) => void
  reset: () => void
}

export const useStateCounts = create<StateCounts>((set) => ({
  counts: {},
  ticks: {},
  set: (nodeId, templateId, count, changed = false) =>
    set((s) => {
      const k = countKey(nodeId, templateId)
      return { counts: { ...s.counts, [k]: count }, ticks: changed ? { ...s.ticks, [k]: (s.ticks[k] ?? 0) + 1 } : s.ticks }
    }),
  reset: () => set({ counts: {}, ticks: {} }),
}))

export const useStateCount = (nodeId: string, templateId: string): number | undefined => useStateCounts((s) => s.counts[countKey(nodeId, templateId)])
export const useStateTick = (nodeId: string, templateId: string): number => useStateCounts((s) => s.ticks[countKey(nodeId, templateId)] ?? 0)

/** `todo.state_changed`：形状不对的丢掉。 */
export function handleStateChanged(nodeId: string, raw: unknown): void {
  if (typeof raw !== 'object' || raw === null) return
  const n = raw as Partial<RepeatStateChanged>
  if (typeof n.id !== 'string' || n.id === '' || typeof n.count !== 'number' || !Number.isFinite(n.count)) return
  useStateCounts.getState().set(nodeId, n.id, n.count, true)
}

export interface StatePage {
  entries: RepeatStateListResult['entries']
  total: number
  matched: number
  hasMore: boolean
}

/** 一页条目（按键升序）；`after` 是上一页最后一个键。顺便把条数记下来。 */
export async function listState(nodeId: string, id: string, prefix: string, after?: string): Promise<StatePage> {
  const res = await rpc().request<RepeatStateListResult>(nodeId, 'todo.state_list', { id, limit: STATE_PAGE, ...(prefix ? { prefix } : {}), ...(after ? { after } : {}) })
  useStateCounts.getState().set(nodeId, res.template_id, res.total)
  return { entries: res.entries, total: res.total, matched: res.matched, hasMore: res.has_more }
}

export async function deleteStateEntry(nodeId: string, id: string, key: string): Promise<RepeatStateDeleteResult> {
  const res = await rpc().request<RepeatStateDeleteResult>(nodeId, 'todo.state_delete', { id, key })
  useStateCounts.getState().set(nodeId, res.template_id, res.count)
  return res
}

export async function clearState(nodeId: string, id: string): Promise<RepeatStateClearResult> {
  const res = await rpc().request<RepeatStateClearResult>(nodeId, 'todo.state_clear', { id })
  useStateCounts.getState().set(nodeId, res.template_id, 0)
  return res
}

/** 节点回的错误翻成人话（读写状态、预览共用）。 */
export function describeStateError(e: unknown, t: TFunction): string {
  if (e instanceof RpcCallError) {
    if (e.kind !== 'rpc') return t('todos.runState.errors.link')
    if (e.code === 'invalid' && e.reason === 'state_off') return t('todos.runState.errors.off')
    if (e.code === 'invalid' && (e.reason === 'not_repeating' || e.reason === 'template_gone')) return t('todos.runState.errors.notRepeating')
    if (e.code === 'invalid' && e.field === 'title') return t('todos.runState.errors.title')
    if (e.code === 'invalid' && e.field === 'body') return t('todos.runState.errors.body')
    if (e.code === 'invalid' && e.field === 'project') return t('todos.runState.errors.project')
    if (e.code === 'forbidden') return t('todos.runState.errors.forbidden')
    if (e.code === 'not_found') return t('todos.runState.errors.notFound')
    if (e.code === 'unsupported') return t('todos.runState.errors.unsupported')
  }
  return t('todos.runState.errors.failed')
}

/** 分段里 anycon 加的那几种的说明（「你写的」是标题和正文）。不认识的种类照样显示，标成「anycon 加的」。 */
export const isAuthored = (s: Pick<PromptSegment, 'kind'>): boolean => s.kind === 'title' || s.kind === 'body'

/** 状态用法说明那一段（`segments` 里 `kind = state_usage`）；Web 里没有这段文字的副本。 */
export const stateUsageSegment = (segments: readonly PromptSegment[] | undefined): string | null => segments?.find((s) => s.kind === 'state_usage')?.text ?? null

export interface PreviewState {
  result: TodoRepeatPreviewResult | null
  error: string | null
  loading: boolean
}

/**
 * `todo.repeat_preview`：`params` 是表单里此刻的内容（`null` = 现在不取）。参数变了重新取，打字时等一小会儿再发；
 * 旧的回复不盖掉新的（只认最后一次请求的回复），失败时给原因。节点的条数变了（`todo.state_changed`）也重新取。
 */
export function useRepeatPreview(nodeId: string, params: TodoRepeatPreviewParams | null, t: TFunction, delayMs = 250): PreviewState {
  const [state, setState] = useState<PreviewState>({ result: null, error: null, loading: false })
  const seq = useRef(0)
  const key = params ? JSON.stringify(params) : null
  const tick = useStateTick(nodeId, params?.id ?? '')
  useEffect(() => {
    if (key === null) {
      seq.current++
      setState({ result: null, error: null, loading: false })
      return
    }
    const mine = ++seq.current
    setState((s) => ({ ...s, loading: true, error: null }))
    const timer = setTimeout(() => {
      rpc()
        .request<TodoRepeatPreviewResult>(nodeId, 'todo.repeat_preview', JSON.parse(key) as TodoRepeatPreviewParams)
        .then(
          (result) => mine === seq.current && setState({ result, error: null, loading: false }),
          (e: unknown) => mine === seq.current && setState({ result: null, error: describeStateError(e, t), loading: false }),
        )
    }, delayMs)
    return () => clearTimeout(timer)
    // `t` 换语言时不用重新取
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [nodeId, key, tick, delayMs])
  return state
}

/** 「插入示例」的三种（文案在 i18n 里：`todos.runState.examples.<id>.text`）。 */
export const STATE_EXAMPLES = ['mergeRequest', 'jira', 'cursor'] as const
export type StateExample = (typeof STATE_EXAMPLES)[number]

/** 往备注末尾追加一段，前面空一行（备注是空的就不空行）。 */
export function appendToNotes(body: string, text: string): string {
  const trimmed = body.replace(/\s+$/, '')
  return trimmed === '' ? text : `${trimmed}\n\n${text}`
}
