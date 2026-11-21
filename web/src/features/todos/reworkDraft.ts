// 验收时「再改改」写了一半的话：按（节点、项）记在内存里，在待处理的详情、验收页之间共用，切换选中、离开页面再回来都还在；
// 发出去（或被明确取消）就清掉；退出登录、移除本设备、忘记节点时一起清（`service.ts`）。不落盘：这是还没发的草稿，不值得留在磁盘上。
import { create } from 'zustand'

interface ReworkDraftState {
  byKey: Record<string, string>
  set: (nodeId: string, todoId: string, text: string) => void
  clear: (nodeId: string, todoId: string) => void
  clearNode: (nodeId: string) => void
  reset: () => void
}

const key = (nodeId: string, todoId: string): string => `${nodeId}:${todoId}`

export const useReworkDrafts = create<ReworkDraftState>()((set) => ({
  byKey: {},
  set: (nodeId, todoId, text) =>
    set((s) => {
      const k = key(nodeId, todoId)
      if (text === '') {
        if (!(k in s.byKey)) return s
        const { [k]: _gone, ...rest } = s.byKey
        return { byKey: rest }
      }
      return { byKey: { ...s.byKey, [k]: text } }
    }),
  clear: (nodeId, todoId) =>
    set((s) => {
      const { [key(nodeId, todoId)]: _gone, ...rest } = s.byKey
      return { byKey: rest }
    }),
  clearNode: (nodeId) => set((s) => ({ byKey: Object.fromEntries(Object.entries(s.byKey).filter(([k]) => !k.startsWith(`${nodeId}:`))) })),
  reset: () => set({ byKey: {} }),
}))

/** 这一项的草稿、改它的函数、清掉它的函数。 */
export function useReworkDraft(nodeId: string, todoId: string): [string, (text: string) => void, () => void] {
  const text = useReworkDrafts((s) => s.byKey[key(nodeId, todoId)] ?? '')
  return [text, (t) => useReworkDrafts.getState().set(nodeId, todoId, t), () => useReworkDrafts.getState().clear(nodeId, todoId)]
}
