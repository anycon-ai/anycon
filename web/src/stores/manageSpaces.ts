// 「管理空间…」对话框开没开（切换器的菜单关掉之后它还要留着，所以状态放在 store 里，对话框挂在应用外壳上）。
import { create } from 'zustand'

interface ManageSpacesState {
  open: boolean
  show: () => void
  hide: () => void
}

export const useManageSpaces = create<ManageSpacesState>()((set) => ({
  open: false,
  show: () => set({ open: true }),
  hide: () => set({ open: false }),
}))
