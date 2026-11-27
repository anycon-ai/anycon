// 「用户看着时实时结束」的那个 turn（spec/web.md § 4.6）：收尾行的对勾只给它描一遍，提示音 / 震动也只为它响。
// 打开历史、追赶（启动和重连时补拉的历史）、向上翻页带来的旧 `turn_done` 都不算。
import { useEffect, useRef, useState } from 'react'
import type { OpenSession } from '@/stores/sessions'
import { usePrefs } from '@/stores/prefs'
import type { TurnDoneBlock } from './reducer'
import { playChime, vibrateOnce } from './feedback'

/** 对勾描边播完之后把 `animate` 撤掉（略长于 index.css 里 `.moment-check` 的时长）：之后行被虚拟列表卸载再挂上也不重播。 */
export const FINISH_ANIMATE_MS = 900

export interface LiveFinish {
  /** 那个 `turn_done` 块的 key。 */
  key: string
  animate: boolean
}

interface Snapshot {
  sessionId: string
  /** 已经同步好、没在追赶，而且在 watch：之后到的事件是实时的。 */
  live: boolean
  currentTurn: number | null
  lastSeq: number
}

function snapshot(open: OpenSession): Snapshot {
  return { sessionId: open.sessionId, live: open.phase === 'ready' && !open.catchingUp && open.watching, currentTurn: open.view.currentTurn, lastSeq: open.view.lastSeq }
}

/**
 * 判定：上一次渲染时会话已经在实时状态、有一个 turn 在进行（用户看到它在跑），这一次出现了那个 turn 的、seq 更大的 `turn_done`，
 * 而且这一次仍然是实时状态、页面在前台。满足时返回它（`animate` 一小会儿后变 false，对勾留着），并按偏好响一声 / 震一下
 * （只对正常完成的 turn）。换了会话就清掉。
 */
export function useLiveTurnEnd(open: OpenSession | undefined): LiveFinish | null {
  const [finish, setFinish] = useState<LiveFinish | null>(null)
  const prev = useRef<Snapshot | null>(null)
  const view = open?.view
  useEffect(() => {
    if (!open) {
      prev.current = null
      return
    }
    const before = prev.current
    const now = snapshot(open)
    prev.current = now
    if (!before || before.sessionId !== now.sessionId) {
      setFinish(null)
      return
    }
    if (!before.live || !now.live || before.currentTurn === null) return
    const turn = before.currentTurn
    const done = open.view.blocks.findLast((b): b is TurnDoneBlock => b.type === 'turn_done' && b.seq > before.lastSeq && b.turnId === turn)
    if (!done) return
    if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return
    setFinish({ key: done.key, animate: true })
    if (done.reason !== 'completed') return
    const p = usePrefs.getState()
    if (p.turnSound) playChime()
    if (p.turnVibrate) vibrateOnce()
    // 只在视图、阶段、追赶、watch 变化时比较；`open` 的其它字段（held、草稿……）变了不用重新判断
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open?.sessionId, view, open?.phase, open?.catchingUp, open?.watching])

  useEffect(() => {
    if (!finish?.animate) return
    const timer = setTimeout(() => setFinish((f) => (f && f.key === finish.key ? { ...f, animate: false } : f)), FINISH_ANIMATE_MS)
    return () => clearTimeout(timer)
  }, [finish])

  return finish
}
