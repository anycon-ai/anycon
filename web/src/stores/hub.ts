import { create } from 'zustand'
import type { Quota } from '@/protocol'
import type { HubSnapshot } from '@/transport/client'

const initial: HubSnapshot = { phase: 'hub_disconnected', halt: null, attempt: 0, retryInMs: null, deviceId: null, paired: false, deviceName: '' }

interface HubState {
  snapshot: HubSnapshot
  quota: Quota | null
  /** 本页是否已经启动过传输层（用于区分「还没开始连」和「断了」）。 */
  started: boolean
  setSnapshot: (s: HubSnapshot) => void
  setQuota: (q: Quota | null) => void
  setStarted: (v: boolean) => void
}

export const useHub = create<HubState>()((set) => ({
  snapshot: initial,
  quota: null,
  started: false,
  setSnapshot: (snapshot) => set({ snapshot }),
  setQuota: (quota) => set({ quota }),
  setStarted: (started) => set({ started }),
}))
