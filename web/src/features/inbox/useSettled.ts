// 左栏勾选的节点这一轮同步是不是都做完了：状态播报取基线、手机「待处理」决定要不要默认展开第一条，都等它。
import { useEffect, useState } from 'react'
import { selectedNodes } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

/** 等一个节点同步完最多等这么久（毫秒），之后不管它了、当作定下来了。 */
export const SETTLE_TIMEOUT_MS = 15_000

/** 节点这一轮同步做完了（`synced`），或者确认它不会有同步：离线、不认识这个浏览器（`noise_failed`）、协议版本不兼容。还在连、正在同步都不算。 */
function isSettled(conn: string, synced: boolean, syncing: boolean, versionMismatch: boolean): boolean {
  if (conn === 'peer_offline' || conn === 'noise_failed' || versionMismatch) return true
  return conn === 'ready' && synced && !syncing
}

export function useNodesSettled(): boolean {
  const byId = useNodes((st) => st.byId)
  const order = useNodes((st) => st.order)
  const saved = usePrefs((st) => st.sidebarNodes)
  const unsettled = selectedNodes(order, saved).filter((id) => {
    const n = byId[id]
    return !n || !isSettled(n.conn, n.synced, n.syncing, n.versionMismatch)
  })
  // 某个节点一直定不下来（第一次同步失败、卡在连接上）：只对它不再等（每个节点自己计时），别的节点同步时照样等。
  // 不然一个卡住的节点会让别的节点重连同步期间的数字变化也被读出来
  const [gaveUp, setGaveUp] = useState<ReadonlySet<string>>(new Set())
  const sig = unsettled.join('\u0000')
  useEffect(() => {
    // 已经定下来的节点从放弃名单里去掉：下次再卡住重新计时
    setGaveUp((prev) => {
      const next = new Set([...prev].filter((id) => unsettled.includes(id)))
      return next.size === prev.size ? prev : next
    })
    const timers = unsettled.filter((id) => !gaveUp.has(id)).map((id) => setTimeout(() => setGaveUp((p) => new Set(p).add(id)), SETTLE_TIMEOUT_MS))
    return () => timers.forEach(clearTimeout)
    // sig 代表 unsettled 的内容；gaveUp 只在这里读一次，不当依赖（它变了不用重排计时）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sig])
  return unsettled.every((id) => gaveUp.has(id))
}
