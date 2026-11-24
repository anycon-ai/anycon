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
  /** 手机（< 768px）上的左侧抽屉：顶栏的菜单按钮打开，里面是和桌面一样的左栏。 */
  leftDrawerOpen: boolean
  setLeftDrawer: (v: boolean) => void
  /** 命令面板（⌘K）：左栏（手机上是抽屉）的搜索按钮和快捷键共用一个。 */
  paletteOpen: boolean
  setPalette: (v: boolean) => void
  /** 「待处理」页正显示着从通知点进来的那条请求：升级说明先不弹，免得盖住它。 */
  deferHomeNotice: boolean
  setDeferHomeNotice: (v: boolean) => void
  /** 最近一次在「会话」里（`/n/:node…`，不含节点设置）打开的位置：`G` 然后 `S` 回到这里。只在内存里。 */
  lastSessionsPath: string | null
  setLastSessionsPath: (path: string) => void
  /** 写 IndexedDB 失败过（草稿 / 缓存可能丢）：状态栏提示，用户可以关掉。 */
  storageError: boolean
  setStorageError: (v: boolean) => void
  push: (kind: ToastKind, text: string, action?: ToastAction, durationMs?: number) => void
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
  leftDrawerOpen: false,
  setLeftDrawer: (leftDrawerOpen) => set({ leftDrawerOpen }),
  paletteOpen: false,
  setPalette: (paletteOpen) => set({ paletteOpen }),
  deferHomeNotice: false,
  setDeferHomeNotice: (deferHomeNotice) => set({ deferHomeNotice }),
  lastSessionsPath: null,
  setLastSessionsPath: (lastSessionsPath) => set({ lastSessionsPath }),
  storageError: false,
  setStorageError: (storageError) => set({ storageError }),
  push: (kind, text, action, durationMs) => {
    const id = nextId++
    set((s) => ({ toasts: [...s.toasts.slice(-3), { id, kind, text, ...(action ? { action } : {}) }] }))
    // 带动作的留得久一点：要给人时间点
    setTimeout(() => get().dismiss(id), durationMs ?? (kind === 'error' || action ? 7000 : 4000))
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  setUpdateReady: (apply) => set({ updateReady: apply !== null, applyUpdate: apply }),
  setRightDrawer: (rightDrawerOpen) => set({ rightDrawerOpen }),
}))

export const toast = {
  info: (t: string, action?: ToastAction, durationMs?: number) => useUi.getState().push('info', t, action, durationMs),
  success: (t: string) => useUi.getState().push('success', t),
  error: (t: string) => useUi.getState().push('error', t),
}
