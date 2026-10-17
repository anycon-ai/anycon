import { useEffect, useState } from 'react'
import type { WorkspaceBranchesResult } from '@/protocol'
import { rpc } from '@/transport/instance'

export type BranchesState = { state: 'idle' } | { state: 'loading' } | { state: 'ready'; branches: WorkspaceBranchesResult } | { state: 'error'; error: unknown }

/**
 * 新建会话选 base 用的分支列表（`workspace.branches`）。`enabled` 变成 true 时拉一次（选了「新 worktree」才需要），
 * 项目换了重新拉；失败不挡新建——base 下拉退回「默认」，由 daemon 决定。
 */
export function useBranches(nodeId: string, project: string | null | undefined, enabled: boolean): BranchesState {
  const [state, setState] = useState<{ key: string; value: BranchesState } | null>(null)
  const key = `${nodeId}\u0000${project ?? ''}`

  useEffect(() => {
    if (!enabled || !project) return
    let alive = true
    rpc()
      .request<WorkspaceBranchesResult>(nodeId, 'workspace.branches', { project })
      .then((branches) => alive && setState({ key, value: { state: 'ready', branches } }))
      .catch((error: unknown) => {
        console.warn('anycon: could not list the project’s branches; the daemon will pick the base', error)
        if (alive) setState({ key, value: { state: 'error', error } })
      })
    return () => {
      alive = false
    }
  }, [enabled, nodeId, project, key])

  if (!enabled || !project) return { state: 'idle' }
  return state && state.key === key ? state.value : { state: 'loading' }
}
