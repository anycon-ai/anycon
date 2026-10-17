// 每个会话的 git 状态（`session.git_status`）：分支 chip 和「改动」标签共用。只在内存里，不持久化；
// 拉取、防抖在 features/worktree/git.ts。
import { create } from 'zustand'
import type { PullRequestInfo, SessionGitStatus } from '@/protocol'

/** 拉取失败的原因：daemon 的 `data.reason`（`not_git`、`worktree_missing`…），没有就是错误码；`code` 是错误码本身。 */
export interface GitFetchError {
  code: string
  reason: string
  message: string
}

export interface GitEntry {
  /** 最近一次成功的结果；会话不在 git 仓库里（`not_git`）时为 null。 */
  status: SessionGitStatus | null
  /** PR 只在进入「改动」标签和手动刷新时查（`include_pr`）；别的刷新沿用上一次的结果。 */
  pr: PullRequestInfo | null
  /** 查了但拿不到 PR 的原因（`no_pr`、`gh_not_installed`…）。 */
  prError: string | null
  /** 是否至少查过一次 PR。 */
  prChecked: boolean
  /** PR 正在查（进入「改动」标签时单独查，不动 `rev`）。 */
  prLoading: boolean
  loading: boolean
  error: GitFetchError | null
  /** 每次成功拉取 +1：改动列表据此重新加载。 */
  rev: number
  /** 同步 base 遇到冲突时的文件（合并还停在中途）。 */
  conflictFiles: string[]
}

export const EMPTY_GIT: GitEntry = { status: null, pr: null, prError: null, prChecked: false, prLoading: false, loading: false, error: null, rev: 0, conflictFiles: [] }

interface GitState {
  bySession: Record<string, GitEntry>
  patch: (sessionId: string, f: (e: GitEntry) => Partial<GitEntry>) => void
  clear: (sessionId: string) => void
}

export const useGit = create<GitState>()((set) => ({
  bySession: {},
  patch: (sessionId, f) =>
    set((s) => {
      const cur = s.bySession[sessionId] ?? EMPTY_GIT
      return { bySession: { ...s.bySession, [sessionId]: { ...cur, ...f(cur) } } }
    }),
  clear: (sessionId) =>
    set((s) => {
      if (!(sessionId in s.bySession)) return s
      const { [sessionId]: _gone, ...rest } = s.bySession
      return { bySession: rest }
    }),
}))

export function useGitEntry(sessionId: string): GitEntry {
  return useGit((s) => s.bySession[sessionId]) ?? EMPTY_GIT
}
