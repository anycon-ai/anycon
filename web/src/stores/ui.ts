import { create } from 'zustand'

export type ToastKind = 'info' | 'success' | 'error'
export interface ToastAction {
  label: string
  run: () => void
}
export interface Toast {
  id: number
  kind: ToastKind
  text: string
  /** 可选的一个动作按钮（比如安全提醒里的「查看」）。 */
  action?: ToastAction
}

interface UiState {
  toasts: Toast[]
  /** 新版 service worker 已就绪，等用户点刷新（横幅优先级 5）。 */
  updateReady: boolean
  applyUpdate: (() => void) | null
  rightDrawerOpen: boolean
  /** 命令面板（⌘K）：左栏的搜索按钮、手机「会话」页的搜索按钮和快捷键共用一个。 */
  paletteOpen: boolean
  setPalette: (v: boolean) => void
  /** 最近一次在「会话」里（`/n/:node…`，不含节点设置）打开的位置：`G` 然后 `S` 回到这里。只在内存里。 */
  lastSessionsPath: string | null
  setLastSessionsPath: (path: string) => void
  /** 写 IndexedDB 失败过（草稿 / 缓存可能丢）：状态栏提示，用户可以关掉。 */
  storageError: boolean
  setStorageError: (v: boolean) => void
  push: (kind: ToastKind, text: string, action?: ToastAction) => void
  dismiss: (id: number) => void
  setUpdateReady: (apply: (() => void) | null) => void
  setRightDrawer: (v: boolean) => void
}

let nextId = 1

export const useUi = create<UiState>()((set, get) => ({
  toasts: [],
  updateReady: false,
  applyUpdate: null,
  rightDrawerOpen: false,
  paletteOpen: false,
  setPalette: (paletteOpen) => set({ paletteOpen }),
  lastSessionsPath: null,
  setLastSessionsPath: (lastSessionsPath) => set({ lastSessionsPath }),
  storageError: false,
  setStorageError: (storageError) => set({ storageError }),
  push: (kind, text, action) => {
    const id = nextId++
    set((s) => ({ toasts: [...s.toasts.slice(-3), { id, kind, text, ...(action ? { action } : {}) }] }))
    // 带动作的留得久一点：要给人时间点
    setTimeout(() => get().dismiss(id), kind === 'error' || action ? 7000 : 4000)
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  setUpdateReady: (apply) => set({ updateReady: apply !== null, applyUpdate: apply }),
  setRightDrawer: (rightDrawerOpen) => set({ rightDrawerOpen }),
}))

export const toast = {
  info: (t: string, action?: ToastAction) => useUi.getState().push('info', t, action),
  success: (t: string) => useUi.getState().push('success', t),
  error: (t: string) => useUi.getState().push('error', t),
}
