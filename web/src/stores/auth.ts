import { create } from 'zustand'

export interface Me {
  id: string
  display_name: string
  email?: string | null
  is_admin: boolean
  login_methods: string[]
}

export interface HubConfig {
  mode: string
  public_url?: string
  login_methods: string[]
  protocol_version: number
  version: string
}

// 账号信息由 TanStack Query 从 /api/me 拉；这里只放传输层和路由守卫要同步读取的那一点。
interface AuthState {
  me: Me | null
  config: HubConfig | null
  setMe: (me: Me | null) => void
  setConfig: (c: HubConfig | null) => void
}

export const useAuth = create<AuthState>()((set) => ({
  me: null,
  config: null,
  setMe: (me) => set({ me }),
  setConfig: (config) => set({ config }),
}))
