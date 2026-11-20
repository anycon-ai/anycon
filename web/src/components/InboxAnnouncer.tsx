import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useInboxNowCount } from '@/features/inbox/useInbox'
import { selectedNodes } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

/**
 * 「待处理」的数字一变，用状态播报区读出完整的一句话（「3 件事在等你」），不只是读一个数字（spec/web.md § 14.10）。
 * 常驻在外壳里（读屏用户不一定在「待处理」页上）。**启动和重连同步时不播**：勾选的节点都同步完（或确认离线）之后才取基线，
 * 同步进行中数字怎么变都不播，同步完再重新取基线；基线之后的真实变化才播。连着变几次合并成最后一句。
 */
export function InboxAnnouncer({ delayMs = 600 }: { delayMs?: number }) {
  const { t } = useTranslation()
  const count = useInboxNowCount()
  const byId = useNodes((st) => st.byId)
  const order = useNodes((st) => st.order)
  const saved = usePrefs((st) => st.sidebarNodes)
  const settledNow = selectedNodes(order, saved).every((id) => {
    const n = byId[id]
    return !!n && isSettled(n.conn, n.synced, n.syncing, n.versionMismatch)
  })
  // 有节点一直定不下来（第一次同步失败、卡在连接上）：等一阵子之后也取基线，不然这个播报区永远不出声
  const [gaveUp, setGaveUp] = useState(false)
  useEffect(() => {
    if (settledNow) {
      setGaveUp(false)
      return
    }
    const id = setTimeout(() => setGaveUp(true), SETTLE_TIMEOUT_MS)
    return () => clearTimeout(id)
  }, [settledNow])
  const settled = settledNow || gaveUp
  const [text, setText] = useState('')
  const prev = useRef<number | null>(null)
  useEffect(() => {
    if (!settled) {
      prev.current = null
      return
    }
    if (prev.current === null) {
      prev.current = count
      return
    }
    if (prev.current === count) return
    prev.current = count
    const id = setTimeout(() => setText(count > 0 ? t('inbox.announce', { count }) : t('inbox.announceNone')), delayMs)
    return () => clearTimeout(id)
  }, [count, settled, delayMs, t])
  return (
    <div role="status" aria-live="polite" aria-atomic="true" className="sr-only" data-testid="inbox-live">
      {text}
    </div>
  )
}

/** 等一个节点同步完最多等这么久（毫秒），之后不管它了、照常取基线。 */
export const SETTLE_TIMEOUT_MS = 15_000

/** 节点这一轮同步做完了（`synced`），或者确认它不会有同步：离线、不认识这个浏览器（`noise_failed`）、协议版本不兼容。还在连、正在同步都不算。 */
function isSettled(conn: string, synced: boolean, syncing: boolean, versionMismatch: boolean): boolean {
  if (conn === 'peer_offline' || conn === 'noise_failed' || versionMismatch) return true
  return conn === 'ready' && synced && !syncing
}
