import { create } from 'zustand'

export type NewProjectMode = 'empty' | 'clone' | 'existing'

/**
 * 从左栏、命令面板、新建会话页打开的「新建项目」对话框（节点首页用它自己的那个）。
 * `then`：建好之后去项目页，还是回新建会话页并选中它。`pickNode`：对话框里能不能换节点。
 */
export interface NewProjectRequest {
  nodeId: string
  mode: NewProjectMode
  then: 'project' | 'newSession'
  pickNode: boolean
}

interface NewProjectState {
  request: NewProjectRequest | null
  open: (req: NewProjectRequest) => void
  /** 对话框里换了节点（`pickNode` 时）。 */
  setNode: (nodeId: string) => void
  close: () => void
}

export const useNewProject = create<NewProjectState>()((set) => ({
  request: null,
  open: (request) => set({ request }),
  setNode: (nodeId) => set((s) => (s.request ? { request: { ...s.request, nodeId } } : s)),
  close: () => set({ request: null }),
}))
