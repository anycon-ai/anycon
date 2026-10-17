// 偏好（localStorage，按浏览器）：外观、Enter 行为、各 (节点, agent) 上次用的选项、右栏开合等。
import { create } from 'zustand'
import { createJSONStorage, persist } from 'zustand/middleware'
import type { Candidate } from '@/features/settings/resolve'
import type { SessionWorkspaceMode } from '@/protocol'

export type ThemePref = 'system' | 'light' | 'dark'
export type LanguagePref = 'auto' | 'en' | 'zh'
export type FontSize = 'sm' | 'md' | 'lg'
export type ThinkingDisplay = 'collapsed' | 'expanded' | 'hidden'
/** auto：桌面 Enter 发送，手机 Enter 换行；send：总是 Enter 发送；newline：总是 Enter 换行（⌘/Ctrl+Enter 发送）。 */
export type EnterBehavior = 'auto' | 'send' | 'newline'
export type ProjectView = 'cards' | 'list'

export interface PrefsState {
  theme: ThemePref
  language: LanguagePref
  fontSize: FontSize
  compact: boolean
  thinkingDisplay: ThinkingDisplay
  enterBehavior: EnterBehavior
  projectView: ProjectView
  infoPanelOpen: boolean
  /** 宽屏左栏是否展开（手机上左栏是抽屉，不受影响）。 */
  sidebarOpen: boolean
  lastNode: string | null
  notifyBannerDismissed: boolean
  /** key = `${nodeId}:${agentId}` */
  lastUsed: Record<string, Candidate>
  /** key = `${nodeId}:${project}`：这个浏览器在这个项目上次选的工作位置（ADR-34）。 */
  lastWorkspace: Record<string, SessionWorkspaceMode>
  customModels: Record<string, string[]>
  set: (patch: Partial<Omit<PrefsState, 'set' | 'setLastUsed' | 'setLastWorkspace' | 'addCustomModel'>>) => void
  setLastUsed: (key: string, value: Candidate) => void
  setLastWorkspace: (key: string, mode: SessionWorkspaceMode) => void
  addCustomModel: (key: string, model: string) => void
}

function safeStorage(): Storage {
  // localStorage 在隐私模式 / 被禁用时会抛：退回内存实现，页面照常工作。
  try {
    const k = '__anycon_probe__'
    localStorage.setItem(k, '1')
    localStorage.removeItem(k)
    return localStorage
  } catch (e) {
    console.warn('anycon: localStorage is unavailable; preferences will not be remembered', e)
    const mem = new Map<string, string>()
    return {
      get length() {
        return mem.size
      },
      clear: () => mem.clear(),
      getItem: (k) => mem.get(k) ?? null,
      key: (i) => [...mem.keys()][i] ?? null,
      removeItem: (k) => void mem.delete(k),
      setItem: (k, v) => void mem.set(k, v),
    }
  }
}

export const usePrefs = create<PrefsState>()(
  persist(
    (set) => ({
      theme: 'system',
      language: 'auto',
      fontSize: 'md',
      compact: false,
      thinkingDisplay: 'collapsed',
      enterBehavior: 'auto',
      projectView: 'cards',
      infoPanelOpen: true,
      sidebarOpen: true,
      lastNode: null,
      notifyBannerDismissed: false,
      lastUsed: {},
      lastWorkspace: {},
      customModels: {},
      set: (patch) => set(patch),
      setLastUsed: (key, value) => set((s) => ({ lastUsed: { ...s.lastUsed, [key]: value } })),
      setLastWorkspace: (key, mode) => set((s) => (s.lastWorkspace[key] === mode ? s : { lastWorkspace: { ...s.lastWorkspace, [key]: mode } })),
      addCustomModel: (key, model) =>
        set((s) => {
          const cur = s.customModels[key] ?? []
          if (cur.includes(model)) return s
          return { customModels: { ...s.customModels, [key]: [model, ...cur].slice(0, 8) } }
        }),
    }),
    {
      name: 'anycon.prefs',
      version: 1,
      storage: createJSONStorage(safeStorage),
      partialize: (s) => ({
        theme: s.theme,
        language: s.language,
        fontSize: s.fontSize,
        compact: s.compact,
        thinkingDisplay: s.thinkingDisplay,
        enterBehavior: s.enterBehavior,
        projectView: s.projectView,
        infoPanelOpen: s.infoPanelOpen,
        sidebarOpen: s.sidebarOpen,
        lastNode: s.lastNode,
        notifyBannerDismissed: s.notifyBannerDismissed,
        lastUsed: s.lastUsed,
        lastWorkspace: s.lastWorkspace,
        customModels: s.customModels,
      }),
    },
  ),
)
