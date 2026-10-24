// 会话页「预览」视图的端口列表（docs/spec/web.md § 13.1）：视图开着时每 5 秒问一次 `preview.ports { session_id }`。
import { useCallback, useEffect, useState } from 'react'
import type { PreviewDetection, PreviewPort, PreviewPortsParams, PreviewPortsResult } from '@/protocol'
import { rpc } from '@/transport/instance'

export const PORTS_POLL_MS = 5000

export interface PortsState {
  /** `loading`：还没有第一份结果；`error`：最近一次没拿到（上一份结果还留着）。 */
  status: 'loading' | 'ready' | 'error'
  ports: PreviewPort[]
  detection: PreviewDetection
  error: unknown
}

/** 按端口号排序；同一个端口出现多次时留信息更多的（不是 `config` 的）。 */
export function sortPorts(ports: readonly PreviewPort[]): PreviewPort[] {
  const byPort = new Map<number, PreviewPort>()
  for (const p of ports) {
    const prev = byPort.get(p.port)
    if (!prev || (prev.source === 'config' && p.source !== 'config')) byPort.set(p.port, p)
  }
  return [...byPort.values()].sort((a, b) => a.port - b.port)
}

/**
 * `enabled = false`（节点离线）时不请求。页面在后台时跳过定时的那一次，回到前台立刻补一次。
 * 失败时留着上一份结果，下一个 5 秒再试。
 */
export function usePreviewPorts(nodeId: string, sessionId: string, enabled: boolean): PortsState & { refresh: () => void } {
  const [state, setState] = useState<PortsState>({ status: 'loading', ports: [], detection: 'ok', error: null })
  const [tick, setTick] = useState(0)

  useEffect(() => {
    if (!enabled) return
    let alive = true
    let busy = false
    const poll = async (): Promise<void> => {
      if (busy || document.hidden) return
      busy = true
      try {
        const params: PreviewPortsParams = { session_id: sessionId }
        const res = await rpc().request<PreviewPortsResult>(nodeId, 'preview.ports', params)
        if (alive) setState({ status: 'ready', ports: sortPorts(res.ports), detection: res.detection, error: null })
      } catch (error) {
        console.warn('anycon: could not list the preview ports', error)
        if (alive) setState((s) => ({ ...s, status: 'error', error }))
      } finally {
        busy = false
      }
    }
    void poll()
    const id = setInterval(() => void poll(), PORTS_POLL_MS)
    const onVisible = (): void => void poll()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      alive = false
      clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [nodeId, sessionId, enabled, tick])

  const refresh = useCallback(() => setTick((n) => n + 1), [])
  return { ...state, refresh }
}
