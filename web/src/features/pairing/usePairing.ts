import { useCallback, useRef, useState } from 'react'
import { isCompletePairCode, normalizePairCode } from '@/lib/code'
import { getClient } from '@/transport/instance'
import { PairError, type PairErrorCode, type PairStage } from '@/transport/client'

export type PairPhase = 'idle' | PairStage | 'error'

export interface PairingState {
  phase: PairPhase
  error: PairErrorCode | null
  nodeId: string | null
}

/** 配对流程的 React 封装：进度、错误分支、成功后登记节点。 */
export function usePairing() {
  const [state, setState] = useState<PairingState>({ phase: 'idle', error: null, nodeId: null })
  const running = useRef(false)

  const start = useCallback(async (code: string, deviceName: string): Promise<string | null> => {
    if (running.current) return null
    if (!isCompletePairCode(code)) {
      setState({ phase: 'error', error: 'invalid_code', nodeId: null })
      return null
    }
    running.current = true
    setState({ phase: 'connecting', error: null, nodeId: null })
    try {
      const res = await getClient().pair(normalizePairCode(code), deviceName, (stage) => setState((s) => ({ ...s, phase: stage })))
      setState({ phase: 'done', error: null, nodeId: res.nodeId })
      return res.nodeId
    } catch (e) {
      const codeErr: PairErrorCode = e instanceof PairError ? e.code : 'hub_unavailable'
      setState({ phase: 'error', error: codeErr, nodeId: null })
      return null
    } finally {
      running.current = false
    }
  }, [])

  const reset = useCallback(() => setState({ phase: 'idle', error: null, nodeId: null }), [])
  return { state, start, reset }
}
