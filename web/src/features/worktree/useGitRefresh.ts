import { useEffect, useRef } from 'react'
import type { SessionEvent } from '@/protocol'
import { useSessions } from '@/stores/sessions'
import { dropGit, refreshGitStatus, scheduleGitRefresh } from './git'

/** 这批新事件该怎么刷新：`turn_done` / `git` 立刻刷；只有 `file_changed` 的防抖刷。 */
export function refreshKind(fresh: readonly SessionEvent[]): 'now' | 'debounced' | null {
  let kind: 'now' | 'debounced' | null = null
  for (const se of fresh) {
    const type = se.event.type
    if (type === 'turn_done' || type === 'git') return 'now'
    if (type === 'file_changed') kind = 'debounced'
  }
  return kind
}

/**
 * 会话页的 git 状态刷新（ADR-34，不轮询）：进入时拉一次，之后只在 `turn_done`、`git` 事件、
 * `file_changed`（防抖 2 秒）时拉；手动刷新在「改动」标签里。`enabled` 为 false（节点离线、不在 git 仓库、
 * worktree 还没建好 / 已经没了）时什么也不做；变成 true（比如 worktree 建好了）时拉一次。
 * 只看比「已处理到的 seq」更新的事件，所以向上翻页加载历史不会触发刷新。
 */
export function useGitRefresh({ nodeId, sessionId, enabled }: { nodeId: string | undefined; sessionId: string; enabled: boolean }): void {
  const events = useSessions((s) => s.open[sessionId]?.events)
  const phase = useSessions((s) => s.open[sessionId]?.phase)
  const handled = useRef<{ sessionId: string; seq: number } | null>(null)

  // 进入 / 变得可用：拉一次
  useEffect(() => {
    if (!nodeId || !enabled) return
    void refreshGitStatus(nodeId, sessionId)
  }, [nodeId, sessionId, enabled])

  // 离开：取消待办的防抖，丢掉状态
  useEffect(() => {
    handled.current = null
    return () => dropGit(sessionId)
  }, [sessionId])

  useEffect(() => {
    if (!events) return
    const last = events.at(-1)?.seq ?? 0
    if (handled.current?.sessionId !== sessionId) {
      // 第一次看到这个会话的事件：把已有的当作「已处理」（进入时已经拉过一次了）；还在加载时先不记
      if (phase !== 'ready') return
      handled.current = { sessionId, seq: last }
      return
    }
    const from = handled.current.seq
    if (last <= from) return
    handled.current.seq = last
    if (!nodeId || !enabled) return
    const fresh: SessionEvent[] = []
    for (let i = events.length - 1; i >= 0 && events[i]!.seq > from; i--) fresh.push(events[i]!)
    const kind = refreshKind(fresh)
    if (kind === 'now') void refreshGitStatus(nodeId, sessionId)
    else if (kind === 'debounced') scheduleGitRefresh(nodeId, sessionId)
  }, [events, phase, nodeId, sessionId, enabled])
}
