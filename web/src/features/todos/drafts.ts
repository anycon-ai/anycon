// 详情里正在编辑的草稿（内存里，按项记）。草稿放在这里而不是组件里：窗口缩放跨过断点、点了别的行、离开这一页又回来，
// 详情组件会卸载重建，没存的修改不能因此悄悄丢掉。保存、明确丢弃、项被删之后才清掉。
// 每份草稿带着开始编辑时那一项的样子（`base`）：保存时只发用户改过的字段（草稿对 base 的差），不是对「现在的」项比较——
// 编辑期间别的设备改了别的字段，不会被这里旧的草稿值盖回去。
import { useEffect } from 'react'
import { create } from 'zustand'

export const EDIT_FIELDS = ['title', 'body', 'project', 'due'] as const
export type EditField = (typeof EDIT_FIELDS)[number]
export type EditValues = Record<EditField, string>

export interface EditDraft {
  /** 开始编辑时那一项各字段的值（`due` 是 `datetime-local` 的字符串）。 */
  base: EditValues
  /** 用户现在填的。 */
  values: EditValues
}

interface DraftsState {
  drafts: Record<string, EditDraft>
  start: (key: string, current: EditValues) => void
  set: (key: string, field: EditField, value: string) => void
  /** 把 `fields` 的 base 换成「现在的」值（接受别处的改动 / 选了「保留我的」）；`take` 时草稿值也换成现在的。 */
  rebase: (key: string, fields: readonly EditField[], current: EditValues, take: boolean) => void
  clear: (key: string) => void
  /** 忘记一个节点：它的草稿一起放掉。 */
  clearNode: (nodeId: string) => void
}

export const useDrafts = create<DraftsState>()((set) => ({
  drafts: {},
  start: (key, current) => set((s) => (s.drafts[key] ? s : { drafts: { ...s.drafts, [key]: { base: current, values: current } } })),
  set: (key, field, value) =>
    set((s) => {
      const d = s.drafts[key]
      return d ? { drafts: { ...s.drafts, [key]: { ...d, values: { ...d.values, [field]: value } } } } : s
    }),
  rebase: (key, fields, current, take) =>
    set((s) => {
      const d = s.drafts[key]
      if (!d) return s
      const base = { ...d.base }
      const values = { ...d.values }
      for (const f of fields) {
        base[f] = current[f]
        if (take) values[f] = current[f]
      }
      return { drafts: { ...s.drafts, [key]: { base, values } } }
    }),
  clear: (key) =>
    set((s) => {
      const { [key]: _gone, ...drafts } = s.drafts
      return { drafts }
    }),
  clearNode: (nodeId) => set((s) => ({ drafts: Object.fromEntries(Object.entries(s.drafts).filter(([k]) => !k.startsWith(`${nodeId}:`))) })),
}))

/** 草稿里用户改过的字段（值和 base 不一样的）。 */
export function dirtyFields(d: EditDraft): EditField[] {
  return EDIT_FIELDS.filter((f) => d.values[f] !== d.base[f])
}

/** 编辑期间别处改了的字段（现在的值和 base 不一样的）。 */
export function changedElsewhere(d: EditDraft, current: EditValues): EditField[] {
  return EDIT_FIELDS.filter((f) => current[f] !== d.base[f])
}

export const draftKey = (nodeId: string, id: string): string => `${nodeId}:${id}`

export const hasDirtyDrafts = (drafts: Record<string, EditDraft>): boolean => Object.values(drafts).some((d) => dirtyFields(d).length > 0)

/** 有没保存的编辑时，关标签页 / 刷新要先问一声（草稿在内存里，页面一刷新就没了）。装在应用外壳上：不管当前在哪一页都有效。 */
export function useUnsavedDraftsGuard(): void {
  useEffect(() => {
    const onBeforeUnload = (e: BeforeUnloadEvent): void => {
      if (hasDirtyDrafts(useDrafts.getState().drafts)) e.preventDefault()
    }
    window.addEventListener('beforeunload', onBeforeUnload)
    return () => window.removeEventListener('beforeunload', onBeforeUnload)
  }, [])
}

/** `draftKey` 拆回节点 id 和项 id（节点 id 里没有冒号）。 */
export function splitDraftKey(key: string): { nodeId: string; id: string } {
  const i = key.indexOf(':')
  return { nodeId: key.slice(0, i), id: key.slice(i + 1) }
}
