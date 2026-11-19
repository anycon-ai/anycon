import { useEffect, useRef, useState } from 'react'
import { useSessions } from '@/stores/sessions'
import { markRead } from './actions'

/** 页面停下来多久再上报：同一批事件陆续到达时合成一次请求。 */
const REPORT_DELAY_MS = 300

function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => document.visibilityState !== 'hidden')
  useEffect(() => {
    const on = (): void => setVisible(document.visibilityState !== 'hidden')
    document.addEventListener('visibilitychange', on)
    return () => document.removeEventListener('visibilitychange', on)
  }, [])
  return visible
}

/**
 * 会话页开着、页面可见、会话没在跑时，把已经显示出来的最大 seq 报给 daemon（`session.mark_read`，已读水位）。
 * 运行中不报：流式输出时 seq 一直在涨，每一下都报太碎；会话停下来（或离开页面）时再一次报到最新。
 * 页面在后台时不报——人没在看。离开会话页时补报一次（运行中途离开，看到哪算哪）。
 */
export function useMarkRead({ nodeId, sessionId, lastSeq, settled, ready }: { nodeId: string | undefined; sessionId: string; lastSeq: number | undefined; settled: boolean; ready: boolean }): void {
  const readSeq = useSessions((s) => s.list[sessionId]?.read_seq ?? 0)
  const visible = usePageVisible()
  const latest = useRef({ nodeId, lastSeq, readSeq, visible, ready })
  latest.current = { nodeId, lastSeq, readSeq, visible, ready }

  const due = !!nodeId && ready && visible && settled && lastSeq !== undefined && lastSeq > readSeq
  useEffect(() => {
    if (!due || !nodeId || lastSeq === undefined) return
    const timer = setTimeout(() => void markRead(nodeId, sessionId, lastSeq), REPORT_DELAY_MS)
    return () => clearTimeout(timer)
  }, [due, nodeId, sessionId, lastSeq])

  // 换会话或离开页面：把看到的最后位置补报一次
  useEffect(
    () => () => {
      const l = latest.current
      if (l.nodeId && l.ready && l.visible && l.lastSeq !== undefined && l.lastSeq > l.readSeq) void markRead(l.nodeId, sessionId, l.lastSeq)
    },
    [sessionId],
  )
}
