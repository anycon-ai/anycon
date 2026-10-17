// 项目的 git 元数据「还不知道」和「不是 git 项目」要分开（web.md § 4.5）。
//
// daemon 冷启动后第一次算某个仓库的 git 元数据可能超时，那时 `workspace.list` 里这个项目先不带 `git`，算完才发
// `workspace.changed { updated }`。新协议里这种项目带 `git_pending: true`：表单的占位一直保持到重新拉到的项目不再
// `git_pending`，补拉不影响它（见 `NewSessionForm`）。
// 旧 daemon 没有 `git_pending`，既没有 `git` 也没有 `git_pending` 的项目走这里的兜底：先显示占位
// 「正在读取 git 信息…」并补拉一次 `workspace.list`；补拉回来还是没有 `git` 就按非 git 项目处理（不再占位），
// 之后 `updated` 带来的新数据会让这一行自己出现。
//
// 这里只记「这个 (节点, 项目) 补拉过没有、什么时候补拉的」；项目数据本身仍然在 `stores/nodes.ts`。
import { useEffect } from 'react'
import { create } from 'zustand'
import { refreshProjects } from '@/features/sync/notifications'

/** 同一个 (节点, 项目) 两次补拉之间至少隔这么久（daemon 也有 5 秒的缓存，补拉很便宜，但不该每次渲染都拉）。 */
export const PROBE_THROTTLE_MS = 5000
/** 补拉请求自己的超时：比通用的 30 秒短得多，占位不能挂着发不出消息；超时按「补拉过了」处理。 */
export const PROBE_TIMEOUT_MS = 10_000

interface ProbeEntry {
  startedAt: number
  inflight: boolean
  /** 至少有一次补拉结束了（成功、失败、超时都算）：之后这个项目没有 `git` 就是非 git 项目，不再占位。 */
  settled: boolean
}

interface ProbeState {
  byKey: Record<string, ProbeEntry>
}

export const useGitProbe = create<ProbeState>()(() => ({ byKey: {} }))

const probeKey = (nodeId: string, project: string): string => `${nodeId}\u0000${project}`

/** 测试用：清空补拉记录。 */
export function resetGitProbes(): void {
  useGitProbe.setState({ byKey: {} })
}

/**
 * 补拉一次 `workspace.list`。在途时、或距上一次开始不到 `PROBE_THROTTLE_MS` 时什么也不做。
 * 已经补拉完过一次的项目再补拉是「静默」的（`settled` 保持 true，不会又出现占位）。
 */
export function probeProjectGit(nodeId: string, project: string, now: number = Date.now()): void {
  const key = probeKey(nodeId, project)
  const cur = useGitProbe.getState().byKey[key]
  if (cur && (cur.inflight || now - cur.startedAt < PROBE_THROTTLE_MS)) return
  const put = (e: ProbeEntry): void => useGitProbe.setState((s) => ({ byKey: { ...s.byKey, [key]: e } }))
  put({ startedAt: now, inflight: true, settled: cur?.settled ?? false })
  void refreshProjects(nodeId, { timeoutMs: PROBE_TIMEOUT_MS }).finally(() => put({ startedAt: now, inflight: false, settled: true }))
}

/**
 * 补拉一次是不是还没回来。`needed` 为真（节点支持 worktree、项目存在却没有 `git`、节点在线）时补拉；
 * 返回 true 表示补拉还没回来、这个项目是不是 git 仓库还不知道——调用方显示占位、先不让发送。
 * 项目 `git_pending` 时占位不由它决定（调用方另外判断），这里的补拉只是顺手确认一次（`updated` 丢了也不会卡住）。
 */
export function useGitPending(nodeId: string, project: string | null | undefined, needed: boolean): boolean {
  const active = needed && !!project
  const key = project ? probeKey(nodeId, project) : ''
  const entry = useGitProbe((s) => s.byKey[key])
  const started = entry !== undefined
  useEffect(() => {
    if (active && project) probeProjectGit(nodeId, project)
    // `started`：记录被清掉（resetGitProbes）之后要重新补拉，否则占位会一直挂着
  }, [active, nodeId, project, started])
  return active && !entry?.settled
}
